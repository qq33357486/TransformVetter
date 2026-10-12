//! one-class 滑窗异常打分(商业化路由的排序/分档信号)。
//!
//! 分数空间与标定实验严格一致(tv-research/scripts/decision_sim.py):
//! - 请求文本按 max_scan_chars(默认 100K,对齐 MAX_CHARS)截断后切 8K
//!   字符非重叠滑窗;
//! - 每窗 char 2-4 gram 特征化,TF 归一化后与"正常流量质心"(训练产物,
//!   已 L2 归一)点积:sim = Σ(c_g/T)·cen_g(T=窗内命中词表的总计数);
//! - anomaly = -min_windows(sim) = 最异常窗取负(**越高越异常**),
//!   tf=0 的退化窗排除(空/纯未登录词),全退化按最不异常 -1.0;
//! - 可选按模型名前缀分组的基线分位归一化(p50→0,p95→1,越高越异常),
//!   修正 opus/grok 等模型线的系统性偏置;未配置基线时决策分=原始
//!   anomaly,OPERATING_POINT.json 的阈值可原样填入。
//!
//! 模型文件(JSON)放在 profile 目录,支持 mtime+size 签名的热重载
//! (与 basic.rs 词库 / hashlinear.rs runtime 同一套缓存模式):
//! - runtime_version=2:词表模式(标定导出,gram 字符串→词表下标,
//!   与 sklearn CountVectorizer 的 vocabulary_ 语义一致,未登录词丢弃);
//! - runtime_version=1:murmur 哈希桶模式(玩具/兼容,gram→murmur 桶)。
//! 模型文件不存在时优雅跳过:调用方降级为现状决策链,不影响可用性。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use tracing::warn;

use crate::moderation::hashlinear::murmurhash3_x86_32;
use crate::profile::ModerationProfile;

static MODEL_CACHE: OnceLock<Mutex<HashMap<String, CachedModel>>> = OnceLock::new();

#[derive(Clone)]
struct CachedModel {
    signature: ModelFileSignature,
    model: Arc<OneClassModel>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ModelFileSignature {
    modified_secs: u64,
    len: u64,
}

/// 模型文件(oneclass_model.json)的结构。
/// centroid 为稀疏质心:特征下标 -> 权重,加载时做 L2 归一。
/// 词表模式(v2)下特征下标 = vocab 数组位置(与导出侧一致);
/// 哈希模式(v1)下特征下标 = murmur 桶。
#[derive(Debug, Clone, Deserialize)]
struct ModelFile {
    runtime_version: u32,
    #[serde(default = "default_window_chars")]
    window_chars: usize,
    #[serde(default = "default_ngram_range")]
    ngram_range: Vec<usize>,
    #[serde(default = "default_n_features")]
    n_features: usize,
    #[serde(default = "default_true")]
    lowercase: bool,
    #[serde(default)]
    alternate_sign: bool,
    /// 词表模式(v2):gram 字符串数组,下标即特征下标。
    #[serde(default)]
    vocab: Option<Vec<String>>,
    centroid: HashMap<String, f64>,
}

fn default_window_chars() -> usize {
    8000
}

fn default_ngram_range() -> Vec<usize> {
    vec![2, 4]
}

fn default_n_features() -> usize {
    262_144
}

fn default_true() -> bool {
    true
}

/// 特征空间:gram → 特征下标的映射方式。
#[derive(Debug, Clone)]
enum FeatureSpace {
    /// 词表模式:精确匹配,未登录词丢弃(对齐 sklearn vocabulary_)。
    Vocab { index: HashMap<String, usize> },
    /// murmur 哈希模式(玩具/兼容):gram → murmur 桶。
    Murmur { n_features: usize, alternate_sign: bool },
}

/// 加载后的模型:质心已 L2 归一(单位向量),特征参数经校验。
#[derive(Debug, Clone)]
pub struct OneClassModel {
    window_chars: usize,
    ngram_range: (usize, usize),
    lowercase: bool,
    features: FeatureSpace,
    centroid: HashMap<usize, f64>,
}

/// 一次请求的打分结果。
#[derive(Debug, Clone, PartialEq)]
pub struct OneClassScore {
    /// 原始异常分 = -最异常窗的 TF·质心相似度(越高越异常;全退化窗=-1.0)。
    pub anomaly: f64,
    /// 按模型分组基线归一化后的异常分(p50→0,p95→1,越高越异常);
    /// 未配置基线时等于 anomaly。
    pub normalized_anomaly: f64,
    /// 命中的基线分组前缀(未命中任何分组时为 "*";未配置基线表时为 "raw")。
    pub baseline_group: String,
    /// 实际参与打分的窗数(空文本为 0)。
    pub window_count: usize,
    /// 最异常窗的字符区间 [start, end)(全文坐标;空文本/全退化窗为 None)。
    /// 供截断逻辑把"中段"对准最异常区段,避免中段藏毒被截掉。
    pub anomaly_window: Option<(usize, usize)>,
}

pub fn model_path(profile: &ModerationProfile) -> PathBuf {
    profile
        .base_dir
        .join(&profile.config.oneclass.model_file)
}

/// 模型文件是否存在(不触发加载与解析;预检/运维探针用)。
#[cfg_attr(not(test), allow(dead_code))]
pub fn model_exists(profile: &ModerationProfile) -> bool {
    model_path(profile).exists()
}

/// 对全文(不截断)打分。模型文件缺失 → Ok(None)(调用方降级);
/// 模型文件损坏 → 告警并 Ok(None),可用性优先。
pub fn score_request(
    text: &str,
    model_name: &str,
    profile: &ModerationProfile,
) -> Result<Option<OneClassScore>> {
    let Some(model) = load_model(profile)? else {
        return Ok(None);
    };

    let max_scan = profile.config.oneclass.max_scan_chars;
    let window_count = count_windows(text, model.window_chars, max_scan);
    if window_count == 0 {
        // 空文本无信号:对齐标定语义(全退化窗),按最不异常处理放行主上游
        return Ok(Some(OneClassScore {
            anomaly: -1.0,
            normalized_anomaly: -1.0,
            baseline_group: "empty".to_string(),
            window_count: 0,
            anomaly_window: None,
        }));
    }

    let (anomaly, anomaly_window) = score_windows(text, &model, max_scan);
    let (normalized, group) = normalize_with_baseline(
        anomaly,
        model_name,
        &profile.config.oneclass.baselines,
    );
    Ok(Some(OneClassScore {
        anomaly,
        normalized_anomaly: normalized,
        baseline_group: group,
        window_count,
        anomaly_window,
    }))
}

/// 计算请求级原始异常分:-min(窗相似度),并返回最异常窗的字符区间
/// (供截断逻辑定位中段)。退化窗(tf=0,如空窗/全部是未登录词)不参与
/// 取最小;全部退化时返回 (-1.0, None)(最不异常,对齐 decision_sim)。
fn score_windows(
    text: &str,
    model: &OneClassModel,
    max_scan_chars: usize,
) -> (f64, Option<(usize, usize)>) {
    let chars: Vec<char> = text.chars().take(max_scan_chars).collect();
    let mut min_sim = f64::MAX;
    let mut best: Option<(usize, usize)> = None;
    let mut offset = 0usize;
    for window in iter_windows(&chars, model.window_chars) {
        let counts = window_counts(window, model);
        if let Some(sim) = tf_centroid_similarity(&counts, model) {
            if sim < min_sim {
                min_sim = sim;
                best = Some((offset, offset + window.len()));
            }
        }
        offset += window.len();
    }
    if min_sim == f64::MAX {
        (-1.0, None)
    } else {
        (-min_sim, best)
    }
}

/// 最异常窗(相似度最小的滑窗)的字符区间 [start, end)(相对全文,UTF-8 安全:
/// 下标按 char 计)。模型缺失/空文本(无窗)返回 None。
///
/// 用途:truncate_text 的中段定位——长文本截断时优先保留被判为最异常的
/// 区段,避免"中段藏毒被整体截掉"(研究实测 30K 头尾截断下中段藏毒
/// glm 复核只拦 17.4%)。
fn most_anomalous_window_range(
    text: &str,
    profile: &ModerationProfile,
) -> Result<Option<(usize, usize)>> {
    let Some(model) = load_model(profile)? else {
        return Ok(None);
    };
    let (_, range) = score_windows(text, &model, profile.config.oneclass.max_scan_chars);
    Ok(range)
}

/// 最异常窗的中心字符下标(全文坐标)。None = 模型缺失/无窗,调用方回退中点。
pub fn most_anomalous_window_center(
    text: &str,
    profile: &ModerationProfile,
) -> Option<usize> {
    match most_anomalous_window_range(text, profile) {
        Ok(Some((start, end))) => Some((start + end) / 2),
        _ => None,
    }
}

/// 非重叠滑窗(按字符,与研究脚本一致:text[i..i+WIN])。
fn iter_windows(chars: &[char], window_chars: usize) -> impl Iterator<Item = &[char]> {
    let window_chars = window_chars.max(1);
    chars.chunks(window_chars)
}

fn count_windows(text: &str, window_chars: usize, max_scan_chars: usize) -> usize {
    let char_count = text.chars().take(max_scan_chars).count();
    if char_count == 0 {
        0
    } else {
        iter_windows(
            &text.chars().take(max_scan_chars).collect::<Vec<_>>(),
            window_chars,
        )
        .count()
    }
}

/// 单窗特征:char n-gram 计数(特征下标按模型特征空间分配)。
/// 用 BTreeMap:求和顺序确定,保证同输入同分数(与对数验证的可复现性)。
fn window_counts(window: &[char], model: &OneClassModel) -> std::collections::BTreeMap<usize, f64> {
    let mut counts: std::collections::BTreeMap<usize, f64> = std::collections::BTreeMap::new();
    let (min_n, max_n) = model.ngram_range;
    let len = window.len();
    if len == 0 {
        return counts;
    }

    let lowered: Vec<char> = if model.lowercase {
        window.iter().flat_map(|ch| ch.to_lowercase()).collect()
    } else {
        window.to_vec()
    };
    // 对齐 sklearn CountVectorizer._char_ngrams 的空白归一:
    // 先小写,再把 ≥2 连续空白折叠成单个空格(单个空白保留原字符)。
    let normalized = collapse_whitespace_runs(&lowered);

    let mut push_gram = |gram: &str,
                         model: &OneClassModel,
                         counts: &mut std::collections::BTreeMap<usize, f64>| {
        match &model.features {
            FeatureSpace::Vocab { index } => {
                // 未登录词丢弃(对齐 sklearn vocabulary_ 语义)
                if let Some(idx) = index.get(gram) {
                    *counts.entry(*idx).or_insert(0.0) += 1.0;
                }
            }
            FeatureSpace::Murmur { n_features, alternate_sign } => {
                let hash = murmurhash3_x86_32(gram.as_bytes(), 0);
                let bucket = hash.unsigned_abs() as usize % n_features;
                let sign = if *alternate_sign && hash < 0 { -1.0 } else { 1.0 };
                *counts.entry(bucket).or_insert(0.0) += sign;
            }
        }
    };

    for n in min_n..=max_n.min(normalized.len()) {
        if n == 0 {
            continue;
        }
        if n == 1 {
            for ch in &normalized {
                push_gram(&ch.to_string(), model, &mut counts);
            }
            continue;
        }
        for start in 0..=normalized.len().saturating_sub(n) {
            let gram: String = normalized[start..start + n].iter().collect();
            push_gram(&gram, model, &mut counts);
        }
    }
    counts
}

/// 复刻 sklearn `re.sub(r"\s\s+", " ", text)`:≥2 的连续空白串折叠成
/// 单个空格;单个空白字符原样保留。(Python \s 与 Rust is_whitespace 在
/// 极少数控制符上可能有差异,请求文本中不出现。)
fn collapse_whitespace_runs(chars: &[char]) -> Vec<char> {
    let mut out = Vec::with_capacity(chars.len());
    let mut idx = 0;
    while idx < chars.len() {
        let ch = chars[idx];
        if ch.is_whitespace() {
            let mut run_end = idx + 1;
            while run_end < chars.len() && chars[run_end].is_whitespace() {
                run_end += 1;
            }
            if run_end - idx >= 2 {
                out.push(' ');
            } else {
                out.push(ch);
            }
            idx = run_end;
        } else {
            out.push(ch);
            idx += 1;
        }
    }
    out
}

/// 窗 TF 向量与单位质心的相似度(对齐 decision_sim.score_all):
/// sim = Σ(c_i·cen_i) / Σc_i,分子分母都只统计"命中模型特征空间"的计数
/// (词表模式的未登录词、哈希模式落在质心空桶上的 gram 同样不计入分母,
/// 对齐 sklearn 只统计 vocabulary_ 列的语义)。
/// tf=0 的退化窗返回 None(空窗/全部未登录词)。
fn tf_centroid_similarity(
    counts: &std::collections::BTreeMap<usize, f64>,
    model: &OneClassModel,
) -> Option<f64> {
    let mut dot = 0.0_f64;
    let mut tf = 0.0_f64;
    for (idx, value) in counts {
        if let Some(weight) = model.centroid.get(idx) {
            dot += value * weight;
            tf += value;
        }
    }
    if tf <= 0.0 {
        return None;
    }
    Some(dot / tf)
}

/// 按模型名前缀匹配基线分组(大小写不敏感,顺序匹配,先配置先命中);
/// 未命中具体分组时用全局组 "*";连全局组都没有则不归一(直接用原始分)。
/// normalized = (anomaly - p50) / (p95 - p50):p95==p50 退化时不拉伸。
fn normalize_with_baseline(
    anomaly: f64,
    model_name: &str,
    baselines: &[crate::profile::OneClassBaseline],
) -> (f64, String) {
    let lowered = model_name.to_ascii_lowercase();
    let matched = baselines
        .iter()
        .find(|baseline| {
            let prefix = baseline.prefix.to_ascii_lowercase();
            !prefix.is_empty() && lowered.starts_with(&prefix)
        })
        .or_else(|| {
            baselines
                .iter()
                .find(|baseline| baseline.prefix == "*")
        });

    let Some(baseline) = matched else {
        return (anomaly, "raw".to_string());
    };

    let span = baseline.p95 - baseline.p50;
    if span.abs() < 1e-12 {
        // 退化基线(p95==p50):不做拉伸,避免除零放大噪声
        return (anomaly, baseline.prefix.clone());
    }
    // p50→0(组内中位),p95→1(组内 95 分位异常尾部),越高越异常
    let normalized = (anomaly - baseline.p50) / span;
    (normalized, baseline.prefix.clone())
}

fn load_model(profile: &ModerationProfile) -> Result<Option<Arc<OneClassModel>>> {
    let path = model_path(profile);
    if !path.exists() {
        return Ok(None);
    }

    let signature = match file_signature(&path) {
        Ok(signature) => signature,
        Err(err) => {
            warn!(path = %path.display(), error = %err, "oneclass model signature failed");
            return Ok(None);
        }
    };

    let cache = MODEL_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let cache_key = profile.base_dir.display().to_string();
    {
        let guard = cache.lock().expect("oneclass model cache");
        if let Some(cached) = guard.get(&cache_key) {
            if cached.signature == signature {
                return Ok(Some(cached.model.clone()));
            }
        }
    }

    let model = Arc::new(match OneClassModel::load(&path) {
        Ok(model) => model,
        Err(err) => {
            // 模型损坏:降级为"无模型",决策链回落现状,不阻断请求
            warn!(path = %path.display(), error = %err, "oneclass model load failed; falling back");
            return Ok(None);
        }
    });

    let mut guard = cache.lock().expect("oneclass model cache");
    guard.insert(
        cache_key,
        CachedModel {
            signature,
            model: model.clone(),
        },
    );
    Ok(Some(model))
}

fn file_signature(path: &Path) -> Result<ModelFileSignature> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to stat {}", path.display()))?;
    let modified_secs = metadata
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|err| anyhow!("mtime before unix epoch for {}: {err}", path.display()))?
        .as_secs();
    Ok(ModelFileSignature {
        modified_secs,
        len: metadata.len(),
    })
}

impl OneClassModel {
    fn load(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let file: ModelFile = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse {}", path.display()))?;

        let ngram_range = match file.ngram_range.as_slice() {
            [min_n, max_n] if *min_n > 0 && *min_n <= *max_n => (*min_n, *max_n),
            other => return Err(anyhow!("invalid oneclass ngram_range: {:?}", other)),
        };
        if file.centroid.is_empty() {
            return Err(anyhow!("oneclass centroid must not be empty"));
        }
        if file.window_chars == 0 {
            return Err(anyhow!("oneclass window_chars must be > 0"));
        }

        let features = match file.runtime_version {
            1 => FeatureSpace::Murmur {
                n_features: file.n_features,
                alternate_sign: file.alternate_sign,
            },
            2 => {
                let vocab = file
                    .vocab
                    .ok_or_else(|| anyhow!("oneclass v2 model requires vocab"))?;
                if vocab.is_empty() {
                    return Err(anyhow!("oneclass v2 vocab must not be empty"));
                }
                FeatureSpace::Vocab {
                    index: vocab
                        .into_iter()
                        .enumerate()
                        .map(|(idx, gram)| (gram, idx))
                        .collect(),
                }
            }
            other => {
                return Err(anyhow!(
                    "unsupported oneclass runtime_version: {other} (expected 1 or 2)"
                ))
            }
        };
        if let FeatureSpace::Murmur { n_features, .. } = &features {
            if *n_features == 0 {
                return Err(anyhow!("oneclass n_features must be > 0"));
            }
        }

        let vocab_len = match &features {
            FeatureSpace::Vocab { index } => Some(index.len()),
            FeatureSpace::Murmur { n_features, .. } => Some(*n_features),
        };
        let mut centroid = HashMap::with_capacity(file.centroid.len());
        let mut norm_sq = 0.0_f64;
        for (key, weight) in &file.centroid {
            let idx: usize = key
                .parse()
                .with_context(|| format!("invalid oneclass centroid key: {key}"))?;
            if idx >= vocab_len.expect("checked above") {
                return Err(anyhow!(
                    "oneclass centroid key {idx} out of range (features={})",
                    vocab_len.expect("checked above")
                ));
            }
            if !weight.is_finite() {
                return Err(anyhow!("oneclass centroid weight for {idx} is not finite"));
            }
            norm_sq += weight * weight;
            centroid.insert(idx, *weight);
        }
        let norm = norm_sq.sqrt();
        if norm <= 0.0 {
            return Err(anyhow!("oneclass centroid norm must be > 0"));
        }
        for weight in centroid.values_mut() {
            *weight /= norm;
        }

        Ok(Self {
            window_chars: file.window_chars,
            ngram_range,
            lowercase: file.lowercase,
            features,
            centroid,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn unique_profile() -> ModerationProfile {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("oneclass-test-{}-{}", std::process::id(), id));
        std::fs::create_dir_all(&base).expect("create profile dir");
        ModerationProfile {
            profile_name: format!("oneclass-{id}"),
            base_dir: base,
            config: crate::profile::ProfileConfig::default(),
        }
    }

    fn write_model(profile: &ModerationProfile, json: &serde_json::Value) {
        let mut file = fs::File::create(model_path(profile)).expect("create model file");
        file.write_all(json.to_string().as_bytes())
            .expect("write model file");
    }

    fn toy_model_json() -> serde_json::Value {
        serde_json::json!({
            "runtime_version": 1,
            "window_chars": 8,
            "ngram_range": [2, 4],
            "n_features": 1024,
            "lowercase": true,
            "alternate_sign": false,
            "centroid": {"0": 1.0, "1": 1.0}
        })
    }

    /// 按模型参数计算文本 gram 计数质心(单文本时即训练质心方向),
    /// murmur 桶模式。
    fn murmur_centroid_for_text(json: &mut serde_json::Value, text: &str) {
        let n_features = json["n_features"].as_u64().unwrap() as usize;
        let lowercase = json["lowercase"].as_bool().unwrap_or(true);
        let chars: Vec<char> = if lowercase {
            text.chars().flat_map(|ch| ch.to_lowercase()).collect()
        } else {
            text.chars().collect()
        };
        let mut counts: std::collections::BTreeMap<usize, f64> = std::collections::BTreeMap::new();
        for n in 2..=4usize {
            if chars.len() < n {
                continue;
            }
            for start in 0..=chars.len() - n {
                let gram: String = chars[start..start + n].iter().collect();
                let hash = murmurhash3_x86_32(gram.as_bytes(), 0);
                *counts.entry(hash.unsigned_abs() as usize % n_features).or_insert(0.0) += 1.0;
            }
        }
        let mut centroid = serde_json::Map::new();
        for (idx, count) in counts {
            centroid.insert(idx.to_string(), serde_json::json!(count));
        }
        json["centroid"] = serde_json::Value::Object(centroid);
    }

    /// 词表模式模型:词表取自 text 的全部 2-4 gram,质心=计数向量。
    fn vocab_model_json(text: &str, window_chars: usize) -> serde_json::Value {
        let chars: Vec<char> = text.chars().flat_map(|ch| ch.to_lowercase()).collect();
        let mut vocab: Vec<String> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        let mut counts: std::collections::BTreeMap<usize, f64> = std::collections::BTreeMap::new();
        for n in 2..=4usize {
            if chars.len() < n {
                continue;
            }
            for start in 0..=chars.len() - n {
                let gram: String = chars[start..start + n].iter().collect();
                let idx = *index.entry(gram.clone()).or_insert_with(|| {
                    vocab.push(gram);
                    vocab.len() - 1
                });
                *counts.entry(idx).or_insert(0.0) += 1.0;
            }
        }
        serde_json::json!({
            "runtime_version": 2,
            "window_chars": window_chars,
            "ngram_range": [2, 4],
            "lowercase": true,
            "vocab": vocab,
            "centroid": counts.into_iter().map(|(k, v)| (k.to_string(), v)).collect::<HashMap<String, f64>>(),
        })
    }

    #[test]
    fn missing_model_returns_none() {
        let profile = unique_profile();
        assert!(score_request("hello world", "claude-opus-4", &profile)
            .expect("score")
            .is_none());
    }

    #[test]
    fn corrupt_model_returns_none() {
        let profile = unique_profile();
        write_model(&profile, &serde_json::json!({"runtime_version": 99}));
        assert!(score_request("hello", "any", &profile)
            .expect("score")
            .is_none());
    }

    #[test]
    fn window_splitting_matches_expected_counts() {
        let text = "abcdef".repeat(5); // 30 chars
        assert_eq!(count_windows(&text, 8, 400_000), 4); // 8*3 + 6
        assert_eq!(count_windows(&text, 30, 400_000), 1);
        assert_eq!(count_windows(&text, 31, 400_000), 1);
        assert_eq!(count_windows("", 8, 400_000), 0);
        // 超出 max_scan_chars 的部分不参与(对齐标定 MAX_CHARS 语义)
        assert_eq!(count_windows(&text, 8, 10), 2);
        let chars: Vec<char> = text.chars().collect();
        let windows: Vec<&[char]> = iter_windows(&chars, 8).collect();
        assert_eq!(windows.len(), 4);
        assert_eq!(windows[0].len(), 8);
        assert_eq!(windows[3].len(), 6);
    }

    /// 手工可验的玩具样例:文本只由 2 个 gram 组成,TF·质心相似度可精确计算。
    /// text = "abab" → 2-gram: ab×1? 实为 ab,ba,ab → ab×2, ba×1;
    /// 3-gram: aba×1, bab×1;4-gram: abab×1。质心=同文本计数方向(归一后)。
    /// sim = Σc²/(T·||c||),c 为计数向量。
    #[test]
    fn tf_centroid_similarity_formula_matches_hand_computation() {
        let profile = unique_profile();
        let text = "abab";
        // 只含 "ab"/"ba" 两个 2-gram 的词表,质心取计数方向:
        // "abab" 的 2-gram = ab×2, ba×1 → c=(2,1)
        write_model(
            &profile,
            &serde_json::json!({
                "runtime_version": 2,
                "window_chars": 512,
                "ngram_range": [2, 2],
                "lowercase": true,
                "vocab": ["ab", "ba"],
                "centroid": {"0": 2.0, "1": 1.0}
            }),
        );

        let score = score_request(text, "m", &profile)
            .expect("score")
            .expect("model");
        // c = (2,1),T = 3,cen = (2,1)/sqrt(5);
        // sim = (2·2/√5 + 1·1/√5)/3 = 5/(3√5);anomaly = -sim
        let expected_sim = 5.0 / (3.0 * 5.0_f64.sqrt());
        assert!(
            (score.anomaly - (-expected_sim)).abs() < 1e-12,
            "anomaly {} should be -{}",
            score.anomaly,
            expected_sim
        );
    }

    #[test]
    fn anomaly_orders_alien_above_normal_text() {
        let profile = unique_profile();
        let mut json = toy_model_json();
        json["window_chars"] = serde_json::json!(512);
        let normal = "normal chat text about weather and code";
        murmur_centroid_for_text(&mut json, normal);
        write_model(&profile, &json);

        let same = score_request(normal, "claude-3", &profile)
            .expect("score")
            .expect("model present");
        let alien = "zzzzqqqqxxxxwwwwvvvv";
        let alien_score = score_request(alien, "claude-3", &profile)
            .expect("score")
            .expect("model present");
        assert!(
            alien_score.anomaly > same.anomaly,
            "alien anomaly {} should exceed normal {}",
            alien_score.anomaly,
            same.anomaly
        );
        // 与质心同向文本的相似度为正 → anomaly 为负(标定空间正常流量特征)
        assert!(same.anomaly < 0.0, "normal-text anomaly should be negative");
    }

    #[test]
    fn vocab_model_drops_out_of_vocab_grams() {
        let profile = unique_profile();
        // 词表只含 "normal chat" 相关 gram
        let json = vocab_model_json("normal chat text", 512);
        write_model(&profile, &json);

        // 全是未登录词的窗 → tf=0 退化 → 全退化按 -1.0(最不异常,glm 层接管)
        let alien = score_request("zz qq xx ww vv", "m", &profile)
            .expect("score")
            .expect("model");
        assert_eq!(alien.anomaly, -1.0);
        assert_eq!(alien.anomaly_window, None);

        // 命中词表的文本正常打分
        let normal = score_request("normal chat text", "m", &profile)
            .expect("score")
            .expect("model");
        assert!(normal.anomaly < 0.0);
        assert!(normal.anomaly_window.is_some());
    }

    #[test]
    fn most_anomalous_window_drives_request_score() {
        // 长正常文本中段塞入异质窗:请求级 anomaly 应被最异常窗抬高
        let profile = unique_profile();
        let mut json = toy_model_json();
        let normal_unit = "normal chat about weather and code ".repeat(20);
        murmur_centroid_for_text(&mut json, &normal_unit);
        json["window_chars"] = serde_json::json!(32);
        write_model(&profile, &json);

        let clean = score_request(&normal_unit, "m", &profile)
            .expect("score")
            .expect("model");
        let poisoned = format!(
            "{}{}{}",
            &normal_unit[..normal_unit.len() / 2],
            "zzqqxxwwvv z z qq xx ww vv ".repeat(2),
            &normal_unit[normal_unit.len() / 2..]
        );
        let poisoned_score = score_request(&poisoned, "m", &profile)
            .expect("score")
            .expect("model");
        assert!(
            poisoned_score.anomaly > clean.anomaly,
            "mid-poisoned anomaly {} should exceed clean {}",
            poisoned_score.anomaly,
            clean.anomaly
        );
        assert!(poisoned_score.window_count > 1);
    }

    #[test]
    fn baseline_group_prefix_matching_and_global_fallback() {
        let mut baselines = vec![
            crate::profile::OneClassBaseline {
                prefix: "claude-opus".to_string(),
                p50: 0.0,
                p95: 0.1,
            },
            crate::profile::OneClassBaseline {
                prefix: "*".to_string(),
                p50: -0.02,
                p95: 0.08,
            },
        ];

        // 命中 opus 组:p50→0, p95→1
        let (norm, group) = normalize_with_baseline(0.0, "claude-opus-4-5", &baselines);
        assert!((norm - 0.0).abs() < 1e-9);
        assert_eq!(group, "claude-opus");
        let (norm, _) = normalize_with_baseline(0.1, "claude-opus-4-5", &baselines);
        assert!((norm - 1.0).abs() < 1e-9);
        // 前缀匹配大小写不敏感
        let (norm, _) = normalize_with_baseline(0.0, "CLAUDE-OPUS-1", &baselines);
        assert!((norm - 0.0).abs() < 1e-9);
        // 比中位更不异常 → 负值(不切流)
        let (norm, _) = normalize_with_baseline(-0.05, "claude-opus-1", &baselines);
        assert!(norm < 0.0);

        // 未命中具体组 → 全局 "*"
        let (norm, group) = normalize_with_baseline(-0.02, "grok-4", &baselines);
        assert!((norm - 0.0).abs() < 1e-9);
        assert_eq!(group, "*");

        // 无任何基线 → 原始分
        baselines.clear();
        let (norm, group) = normalize_with_baseline(-0.42, "claude-opus", &baselines);
        assert!((norm - -0.42).abs() < 1e-12);
        assert_eq!(group, "raw");

        // 退化基线:不拉伸
        let degenerate = vec![crate::profile::OneClassBaseline {
            prefix: "*".to_string(),
            p50: 0.4,
            p95: 0.4,
        }];
        let (norm, _) = normalize_with_baseline(-0.9, "grok", &degenerate);
        assert!((norm - -0.9).abs() < 1e-12);
    }

    #[test]
    fn model_hot_reload_on_file_change() {
        let profile = unique_profile();
        let text = "aaaa bbbb cccc dddd";
        // 首版质心:正对该文本(计数方向)→ sim>0,anomaly<0,有最异常窗
        let mut json = toy_model_json();
        json["window_chars"] = serde_json::json!(512);
        murmur_centroid_for_text(&mut json, text);
        write_model(&profile, &json);

        let first = score_request(text, "m", &profile).expect("score").expect("model");
        assert!(first.anomaly_window.is_some());
        assert!(first.anomaly < 0.0);
        // 缓存命中:重复调用逐位一致
        let again = score_request(text, "m", &profile).expect("score").expect("model");
        assert_eq!(first, again);

        // 搜索与目标文本零桶重叠的异质文本:所有窗都退化(tf=0)
        let buckets_of = |s: &str| -> std::collections::HashSet<usize> {
            (2..=4)
                .flat_map(|n| {
                    let chars: Vec<char> = s.chars().collect();
                    (0..=chars.len().saturating_sub(n))
                        .map(move |i| chars[i..i + n].iter().collect::<String>())
                })
                .map(|gram: String| {
                    let hash = murmurhash3_x86_32(gram.as_bytes(), 0);
                    hash.unsigned_abs() as usize % 1024
                })
                .collect()
        };
        let text_buckets = buckets_of(text);
        let alien = ["zqxw", "vjyk", "gpzb", "qmxn", "wkrj", "xlfp", "ytsb", "ndvh"]
            .into_iter()
            .map(str::to_string)
            .chain((0..64).map(|i| format!("alien-unique-{i}")))
            .find(|candidate| text_buckets.is_disjoint(&buckets_of(candidate)))
            .expect("test setup: found no bucket-disjoint alien candidate");

        // 重载:质心换到异质方向 → 该文本全窗退化 → anomaly 固定 -1.0、无窗
        let mut json2 = toy_model_json();
        json2["window_chars"] = serde_json::json!(512);
        murmur_centroid_for_text(&mut json2, &alien);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        write_model(&profile, &json2);
        let reloaded = score_request(text, "m", &profile).expect("score").expect("model");
        assert_eq!(reloaded.anomaly, -1.0, "all-degenerate windows must score -1.0");
        assert_eq!(reloaded.anomaly_window, None);
        assert_ne!(first.anomaly, reloaded.anomaly, "hot reload must change the score");
    }

    #[test]
    fn empty_text_scores_as_least_anomalous() {
        let profile = unique_profile();
        write_model(&profile, &toy_model_json());
        let score = score_request("", "m", &profile)
            .expect("score")
            .expect("model");
        // 对齐 decision_sim:全退化窗 → anomaly = -1.0(最不异常)
        assert_eq!(score.anomaly, -1.0);
        assert_eq!(score.window_count, 0);
    }

    #[test]
    fn v2_model_without_vocab_is_rejected() {
        let profile = unique_profile();
        let mut json = toy_model_json();
        json["runtime_version"] = serde_json::json!(2);
        json["vocab"] = serde_json::Value::Null;
        write_model(&profile, &json);
        assert!(score_request("x", "m", &profile).expect("score").is_none());
    }
}
