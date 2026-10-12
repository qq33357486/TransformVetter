//! one-class 滑窗异常打分(商业化路由的排序/分档信号)。
//!
//! 思路沿用可行性研究(run_oneclass2.py 方法A):
//! - 请求文本切 8K 字符窗,逐窗 char 2-4 gram 哈希特征 + TF 归一化;
//! - 每窗与"正常流量质心"(训练产物,稀疏 khash->weight)算余弦;
//! - 取最小余弦窗 = 最异常窗,作为请求级原始分(**越高越合规**);
//! - 再按模型名前缀分组的基线分位(p05/p50)归一化,修正 opus/grok 等
//!   模型线的系统性偏置(基线表在 profile 配置,缺省组用 "*")。
//!
//! 模型文件(JSON)放在 profile 目录,支持 mtime+size 签名的热重载
//! (与 basic.rs 词库 / hashlinear.rs runtime 同一套缓存模式)。
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
/// centroid 为稀疏质心:khash(bucket 下标) -> 权重,加载时做 L2 归一。
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

/// 加载后的模型:质心已 L2 归一(单位向量),特征参数经校验。
#[derive(Debug, Clone)]
pub struct OneClassModel {
    window_chars: usize,
    ngram_range: (usize, usize),
    n_features: usize,
    lowercase: bool,
    alternate_sign: bool,
    centroid: HashMap<usize, f64>,
}

/// 一次请求的打分结果。
#[derive(Debug, Clone, PartialEq)]
pub struct OneClassScore {
    /// 最异常窗的原始余弦分,越高越接近正常流量。
    pub raw_score: f64,
    /// 按模型分组基线归一化后的分(p05→0,p50→1,越高越合规);
    /// 未配置基线时等于 raw_score。
    pub normalized_score: f64,
    /// 命中的基线分组前缀(未命中任何分组时为 "*";未配置基线表时为 "raw")。
    pub baseline_group: String,
    /// 实际参与打分的窗数(空文本为 0)。
    pub window_count: usize,
    /// 最异常窗的字符区间 [start, end)(全文坐标;空文本为 None)。
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
        // 空文本无信号:按"最合规"处理,决策链自然放行主上游
        return Ok(Some(OneClassScore {
            raw_score: 1.0,
            normalized_score: 1.0,
            baseline_group: "empty".to_string(),
            window_count: 0,
            anomaly_window: None,
        }));
    }

    let (raw_score, anomaly_window) = score_windows(text, &model, max_scan);
    let (normalized, group) =
        normalize_with_baseline(raw_score, model_name, &profile.config.oneclass.baselines);
    Ok(Some(OneClassScore {
        raw_score,
        normalized_score: normalized,
        baseline_group: group,
        window_count,
        anomaly_window,
    }))
}

/// 计算请求级原始分:所有窗中最小的余弦(最异常窗),并返回该窗的
/// 字符区间(供截断逻辑定位中段)。
fn score_windows(
    text: &str,
    model: &OneClassModel,
    max_scan_chars: usize,
) -> (f64, Option<(usize, usize)>) {
    let chars: Vec<char> = text.chars().take(max_scan_chars).collect();
    let mut min_cosine = f64::MAX;
    let mut best: Option<(usize, usize)> = None;
    let mut offset = 0usize;
    for window in iter_windows(&chars, model.window_chars) {
        let counts = window_counts(window, &model);
        let cosine = cosine_with_centroid(&counts, model);
        if cosine < min_cosine {
            min_cosine = cosine;
            best = Some((offset, offset + window.len()));
        }
        offset += window.len();
    }
    if min_cosine == f64::MAX {
        (1.0, best)
    } else {
        (min_cosine, best)
    }
}

/// 最异常窗(余弦最小的滑窗)的字符区间 [start, end)(相对全文,UTF-8 安全:
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

/// 单窗特征:char n-gram 哈希计数(TF)。TF 归一化在余弦里天然抵消
/// (余弦对向量缩放不敏感),此处直接产出计数向量。
fn window_counts(window: &[char], model: &OneClassModel) -> HashMap<usize, f64> {
    let mut counts: HashMap<usize, f64> = HashMap::new();
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

    let mut push_gram = |gram: &str| {
        let hash = murmurhash3_x86_32(gram.as_bytes(), 0);
        let idx = hash.unsigned_abs() as usize % model.n_features;
        let sign = if model.alternate_sign && hash < 0 { -1.0 } else { 1.0 };
        *counts.entry(idx).or_insert(0.0) += sign;
    };

    for n in min_n..=max_n.min(lowered.len()) {
        if n == 0 {
            continue;
        }
        if n == 1 {
            for ch in &lowered {
                push_gram(&ch.to_string());
            }
            continue;
        }
        for start in 0..=lowered.len().saturating_sub(n) {
            let gram: String = lowered[start..start + n].iter().collect();
            push_gram(&gram);
        }
    }
    counts
}

/// 窗 TF 向量与单位质心的余弦。
fn cosine_with_centroid(counts: &HashMap<usize, f64>, model: &OneClassModel) -> f64 {
    let mut dot = 0.0_f64;
    let mut norm_sq = 0.0_f64;
    for (idx, value) in counts {
        norm_sq += value * value;
        if let Some(weight) = model.centroid.get(idx) {
            dot += value * weight;
        }
    }
    let norm = norm_sq.sqrt();
    if norm <= 0.0 {
        // 无特征(理论上空窗才会):按"无信号=正常"处理
        return 1.0;
    }
    dot / norm
}

/// 按模型名前缀匹配基线分组(大小写不敏感,顺序匹配,先配置先命中);
/// 未命中具体分组时用全局组 "*";连全局组都没有则不归一(直接用原始分)。
fn normalize_with_baseline(
    raw_score: f64,
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
        return (raw_score, "raw".to_string());
    };

    let span = baseline.p50 - baseline.p05;
    if span.abs() < 1e-12 {
        // 退化基线(p50==p05):不做拉伸,避免除零放大噪声
        return (raw_score, baseline.prefix.clone());
    }
    // p05→0(组内最异常 5% 分位),p50→1(组内中位),越高越合规
    let normalized = (raw_score - baseline.p05) / span;
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

        if file.runtime_version != 1 {
            return Err(anyhow!(
                "unsupported oneclass runtime_version: {}",
                file.runtime_version
            ));
        }
        let ngram_range = match file.ngram_range.as_slice() {
            [min_n, max_n] if *min_n > 0 && *min_n <= *max_n => (*min_n, *max_n),
            other => return Err(anyhow!("invalid oneclass ngram_range: {:?}", other)),
        };
        if file.n_features == 0 {
            return Err(anyhow!("oneclass n_features must be > 0"));
        }
        if file.centroid.is_empty() {
            return Err(anyhow!("oneclass centroid must not be empty"));
        }
        if file.window_chars == 0 {
            return Err(anyhow!("oneclass window_chars must be > 0"));
        }

        let mut centroid = HashMap::with_capacity(file.centroid.len());
        let mut norm_sq = 0.0_f64;
        for (key, weight) in &file.centroid {
            let idx: usize = key
                .parse()
                .with_context(|| format!("invalid oneclass centroid key: {key}"))?;
            if idx >= file.n_features {
                return Err(anyhow!(
                    "oneclass centroid key {idx} out of range (n_features={})",
                    file.n_features
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
            n_features: file.n_features,
            lowercase: file.lowercase,
            alternate_sign: file.alternate_sign,
            centroid,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    std::thread_local! {
        static TEST_ID: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    }

    fn unique_profile() -> ModerationProfile {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        // 命名必须跨运行唯一:早期版本用 pid+自增 id,Windows pid 回卷后
        // 会命中上次运行遗留的目录,把"无模型"用例变成"读到陈旧模型"
        // (本地实测 366 个遗留目录时连续翻车)。此处加进程启动时间戳,
        // 并对同名遗留目录做防御性清理。
        static RUN_STAMP: std::sync::OnceLock<u128> = std::sync::OnceLock::new();
        let stamp = *RUN_STAMP.get_or_init(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        });
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let base =
            std::env::temp_dir().join(format!("oneclass-test-{stamp}-{id}"));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).expect("create profile dir");
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
            "centroid": {
                // "ababab" 的 2-gram "ab"/"ba" 会落到这两个桶,权重相等 → 质心即该方向
                "0": 1.0,
                "1": 1.0
            }
        })
    }

    fn set_centroid_for_text(json: &mut serde_json::Value, text: &str) {
        // 按模型参数计算文本 gram 计数向量,把质心对准该文本方向
        // (训练产物的质心 = 正常窗 TF 均值,单文本时即其计数向量方向)
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
        // 超出 max_scan_chars 的部分不参与
        assert_eq!(count_windows(&text, 8, 10), 2);
        let chars: Vec<char> = text.chars().collect();
        let windows: Vec<&[char]> = iter_windows(&chars, 8).collect();
        assert_eq!(windows.len(), 4);
        assert_eq!(windows[0].len(), 8);
        assert_eq!(windows[3].len(), 6);
    }

    #[test]
    fn cosine_identical_text_is_one_and_orthogonal_is_lower() {
        let profile = unique_profile();
        let mut json = toy_model_json();
        // 文本短于窗长 → 单窗,且该窗与质心同为整文本方向
        json["window_chars"] = serde_json::json!(512);
        let normal = "normal chat text about weather and code";
        set_centroid_for_text(&mut json, normal);
        write_model(&profile, &json);

        let same = score_request(normal, "claude-3", &profile)
            .expect("score")
            .expect("model present");
        // 与质心同向(同文本)的窗余弦应为 1(允许浮点误差)
        assert!((same.raw_score - 1.0).abs() < 1e-9, "got {}", same.raw_score);

        // 换成与训练文本完全不同字符组合的文本,余弦应显著更低
        let alien = "zzzzqqqqxxxxwwwwvvvv";
        let alien_score = score_request(alien, "claude-3", &profile)
            .expect("score")
            .expect("model present");
        assert!(
            alien_score.raw_score < same.raw_score,
            "alien {} should be below normal {}",
            alien_score.raw_score,
            same.raw_score
        );
    }

    #[test]
    fn most_anomalous_window_drives_request_score() {
        // 长正常文本中段塞入异质窗:请求级分数应被最异常窗拉低
        let profile = unique_profile();
        let mut json = toy_model_json();
        let normal_unit = "normal chat about weather and code ".repeat(20);
        set_centroid_for_text(&mut json, &normal_unit);
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
            poisoned_score.raw_score < clean.raw_score,
            "mid-poisoned {} should be below clean {}",
            poisoned_score.raw_score,
            clean.raw_score
        );
        assert!(poisoned_score.window_count > 1);
    }

    #[test]
    fn baseline_group_prefix_matching_and_global_fallback() {
        let mut baselines = vec![
            crate::profile::OneClassBaseline {
                prefix: "claude-opus".to_string(),
                p05: 0.20,
                p50: 0.40,
            },
            crate::profile::OneClassBaseline {
                prefix: "*".to_string(),
                p05: 0.30,
                p50: 0.50,
            },
        ];

        // 命中 opus 组:p05→0, p50→1
        let (norm, group) = normalize_with_baseline(0.40, "claude-opus-4-5", &baselines);
        assert!((norm - 1.0).abs() < 1e-9);
        assert_eq!(group, "claude-opus");
        let (norm, _) = normalize_with_baseline(0.20, "CLAUDE-OPUS-1", &baselines);
        assert!((norm - 0.0).abs() < 1e-9, "prefix match should be case-insensitive");

        // 未命中具体组 → 全局 "*"
        let (norm, group) = normalize_with_baseline(0.50, "grok-4", &baselines);
        assert!((norm - 1.0).abs() < 1e-9);
        assert_eq!(group, "*");

        // 无任何基线 → 原始分
        baselines.clear();
        let (norm, group) = normalize_with_baseline(0.42, "claude-opus", &baselines);
        assert!((norm - 0.42).abs() < 1e-12);
        assert_eq!(group, "raw");

        // 退化基线:不拉伸
        let degenerate = vec![crate::profile::OneClassBaseline {
            prefix: "*".to_string(),
            p05: 0.4,
            p50: 0.4,
        }];
        let (norm, _) = normalize_with_baseline(0.9, "grok", &degenerate);
        assert!((norm - 0.9).abs() < 1e-12);
    }

    #[test]
    fn model_hot_reload_on_file_change() {
        let profile = unique_profile();
        let mut json = toy_model_json();
        json["window_chars"] = serde_json::json!(512);
        write_model(&profile, &json);
        let text = "aaaa bbbb cccc dddd";

        let first = score_request(text, "m", &profile).expect("score").expect("model");
        // 原始分与缓存一致(重复调用命中缓存,结果不变)
        let again = score_request(text, "m", &profile).expect("score").expect("model");
        assert_eq!(first, again);

        // 改写模型:质心换到正对该文本的方向(计数质心)→ 分数变为 1,证明热重载生效
        let mut json2 = toy_model_json();
        json2["window_chars"] = serde_json::json!(512);
        set_centroid_for_text(&mut json2, text);
        // 签名含 mtime 秒与 len;主动 sleep 跨秒确保签名变化
        std::thread::sleep(std::time::Duration::from_millis(1100));
        write_model(&profile, &json2);
        let reloaded = score_request(text, "m", &profile).expect("score").expect("model");
        assert!(
            (reloaded.raw_score - 1.0).abs() < 1e-9,
            "reloaded model should score identical text as 1.0, got {}",
            reloaded.raw_score
        );
        assert!(
            first.raw_score < reloaded.raw_score,
            "score should change after hot reload: {} -> {}",
            first.raw_score,
            reloaded.raw_score
        );
    }

    #[test]
    fn empty_text_scores_as_fully_normal() {
        let profile = unique_profile();
        write_model(&profile, &toy_model_json());
        let score = score_request("", "m", &profile)
            .expect("score")
            .expect("model");
        assert_eq!(score.raw_score, 1.0);
        assert_eq!(score.window_count, 0);
        assert_eq!(score.anomaly_window, None);
    }

    #[test]
    fn most_anomalous_window_lands_on_alien_segment() {
        // 截断修复的定位来源:最异常窗必须落在远离中点的异质段上,
        // 且独立查询接口与 score_request 的结果一致。
        let profile = unique_profile();
        let mut json = toy_model_json();
        let normal_unit = "normal chat about weather and code ".repeat(7); // 245 chars
        set_centroid_for_text(&mut json, &normal_unit);
        json["window_chars"] = serde_json::json!(40);
        write_model(&profile, &json);

        // 全文除异质段外都用正常模式连续填充:单字符填充(如 'n'/'k' 重复)
        // 自身的 gram 分布与质心正交,会形成一个"假最异常窗"干扰断言
        let normal_filler: String = normal_unit.chars().cycle().take(258).collect();
        let mut text = normal_filler;
        let alien: String = "zqxjwv-".repeat(14).chars().take(92).collect();
        text.push_str(&alien); // 258..350:异质段(中点窗 180..220 之外)
        let normal_tail: String = normal_unit.chars().cycle().take(50).collect();
        text.push_str(&normal_tail); // 350..400
        assert_eq!(text.chars().count(), 400);

        let score = score_request(&text, "m", &profile)
            .expect("score")
            .expect("model");
        let (start, end) = score
            .anomaly_window
            .expect("anomaly window for multi-window text");
        assert!(
            end > 258 && start < 350,
            "window {start}..{end} must intersect the alien segment 258..350"
        );
        let center = (start + end) / 2;
        assert!(
            (258..=350).contains(&center),
            "window center {center} should sit inside the alien segment"
        );
        // 独立接口(供现状 smart 链定位)与打分结果一致
        assert_eq!(
            most_anomalous_window_center(&text, &profile),
            Some(center)
        );

        // 短文本(单窗):区间就是整段文本
        let short = "normal chat about weather";
        let short_score = score_request(short, "m", &profile)
            .expect("score")
            .expect("model");
        assert_eq!(short_score.anomaly_window, Some((0, short.chars().count())));
    }
}
