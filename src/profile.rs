use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
struct PromptTemplateSignature {
    modified_at: Option<SystemTime>,
    len: Option<u64>,
}

#[derive(Debug, Clone)]
struct PromptTemplateEntry {
    signature: PromptTemplateSignature,
    template: String,
}

static PROMPT_TEMPLATES: OnceLock<Mutex<HashMap<String, PromptTemplateEntry>>> = OnceLock::new();

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AiConfig {
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
}

impl AiConfig {
    pub fn get_model_candidates(&self) -> Vec<String> {
        self.model
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(ToString::to_string)
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PromptConfig {
    #[serde(default = "default_template_file")]
    pub template_file: String,
    #[serde(default = "default_prompt_max_text_length")]
    pub max_text_length: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProbabilityConfig {
    #[serde(default = "default_ai_review_rate")]
    pub ai_review_rate: f64,
    #[serde(default = "default_random_seed")]
    pub random_seed: u64,
    #[serde(default = "default_low_risk_threshold")]
    pub low_risk_threshold: f64,
    #[serde(default = "default_high_risk_threshold")]
    pub high_risk_threshold: f64,
    #[serde(default)]
    pub enable_concurrency_limit_fallback: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VocabBucket {
    #[serde(default = "default_bucket_name")]
    pub name: String,
    #[serde(default)]
    pub min_doc_ratio: f64,
    #[serde(default = "default_one")]
    pub max_doc_ratio: f64,
    #[serde(default = "default_bucket_limit")]
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BowTrainingConfig {
    #[serde(default = "default_min_samples")]
    pub min_samples: usize,
    #[serde(default = "default_retrain_interval_minutes")]
    pub retrain_interval_minutes: usize,
    #[serde(default = "default_max_samples")]
    pub max_samples: usize,
    #[serde(default = "default_sample_loading")]
    pub sample_loading: String,
    #[serde(default = "default_max_features")]
    pub max_features: usize,
    #[serde(default = "default_true")]
    pub use_char_ngram: bool,
    #[serde(default = "default_char_ngram_range")]
    pub char_ngram_range: Vec<usize>,
    #[serde(default = "default_true")]
    pub use_word_ngram: bool,
    #[serde(default = "default_word_ngram_range")]
    pub word_ngram_range: Vec<usize>,
    #[serde(default = "default_bow_model_type")]
    pub model_type: String,
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,
    #[serde(default = "default_max_seconds")]
    pub max_seconds: usize,
    #[serde(default = "default_max_db_items")]
    pub max_db_items: usize,
    #[serde(default = "default_true")]
    pub use_layered_vocab: bool,
    #[serde(default = "default_vocab_buckets")]
    pub vocab_buckets: Vec<VocabBucket>,
}

impl Default for BowTrainingConfig {
    fn default() -> Self {
        Self {
            min_samples: default_min_samples(),
            retrain_interval_minutes: default_retrain_interval_minutes(),
            max_samples: default_max_samples(),
            sample_loading: default_sample_loading(),
            max_features: default_max_features(),
            use_char_ngram: true,
            char_ngram_range: default_char_ngram_range(),
            use_word_ngram: true,
            word_ngram_range: default_word_ngram_range(),
            model_type: default_bow_model_type(),
            batch_size: default_batch_size(),
            max_seconds: default_max_seconds(),
            max_db_items: default_max_db_items(),
            use_layered_vocab: true,
            vocab_buckets: default_vocab_buckets(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FastTextTrainingConfig {
    #[serde(default = "default_min_samples")]
    pub min_samples: usize,
    #[serde(default = "default_retrain_interval_minutes")]
    pub retrain_interval_minutes: usize,
    #[serde(default = "default_max_samples")]
    pub max_samples: usize,
    #[serde(default = "default_max_db_items")]
    pub max_db_items: usize,
    #[serde(default = "default_sample_loading")]
    pub sample_loading: String,
    #[serde(default)]
    pub use_jieba: bool,
    #[serde(default)]
    pub use_tiktoken: bool,
    #[serde(default = "default_tiktoken_model")]
    pub tiktoken_model: String,
    #[serde(default = "default_fasttext_dim")]
    pub dim: usize,
    #[serde(default = "default_fasttext_lr")]
    pub lr: f64,
    #[serde(default = "default_fasttext_epoch")]
    pub epoch: usize,
    #[serde(default = "default_fasttext_word_ngrams")]
    pub word_ngrams: usize,
    #[serde(default = "default_fasttext_minn")]
    pub minn: usize,
    #[serde(default = "default_fasttext_maxn")]
    pub maxn: usize,
    #[serde(default = "default_fasttext_bucket")]
    pub bucket: usize,
    #[serde(default = "default_true")]
    pub quantize: bool,
    #[serde(default)]
    pub qnorm: bool,
    #[serde(default)]
    pub cutoff: usize,
    #[serde(default = "default_true")]
    pub retrain: bool,
}

impl Default for FastTextTrainingConfig {
    fn default() -> Self {
        Self {
            min_samples: default_min_samples(),
            retrain_interval_minutes: default_retrain_interval_minutes(),
            max_samples: default_max_samples(),
            max_db_items: default_max_db_items(),
            sample_loading: default_sample_loading(),
            use_jieba: false,
            use_tiktoken: false,
            tiktoken_model: default_tiktoken_model(),
            dim: default_fasttext_dim(),
            lr: default_fasttext_lr(),
            epoch: default_fasttext_epoch(),
            word_ngrams: default_fasttext_word_ngrams(),
            minn: default_fasttext_minn(),
            maxn: default_fasttext_maxn(),
            bucket: default_fasttext_bucket(),
            quantize: true,
            qnorm: false,
            cutoff: 0,
            retrain: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HashLinearTrainingConfig {
    #[serde(default = "default_min_samples")]
    pub min_samples: usize,
    #[serde(default = "default_retrain_interval_minutes")]
    pub retrain_interval_minutes: usize,
    #[serde(default = "default_max_samples")]
    pub max_samples: usize,
    #[serde(default = "default_max_db_items")]
    pub max_db_items: usize,
    #[serde(default = "default_sample_loading")]
    pub sample_loading: String,
    #[serde(default = "default_hashlinear_analyzer")]
    pub analyzer: String,
    #[serde(default = "default_hashlinear_ngram_range")]
    pub ngram_range: Vec<usize>,
    #[serde(default = "default_hashlinear_n_features")]
    pub n_features: usize,
    #[serde(default)]
    pub alternate_sign: bool,
    #[serde(default = "default_hashlinear_norm")]
    pub norm: Option<String>,
    #[serde(default = "default_hashlinear_alpha")]
    pub alpha: f64,
    #[serde(default = "default_hashlinear_epochs")]
    pub epochs: usize,
    #[serde(default = "default_hashlinear_batch_size")]
    pub batch_size: usize,
    #[serde(default = "default_random_seed")]
    pub random_seed: u64,
    #[serde(default = "default_max_seconds")]
    pub max_seconds: usize,
    #[serde(default)]
    pub use_jieba: bool,
}

impl Default for HashLinearTrainingConfig {
    fn default() -> Self {
        Self {
            min_samples: default_min_samples(),
            retrain_interval_minutes: default_retrain_interval_minutes(),
            max_samples: default_max_samples(),
            max_db_items: default_max_db_items(),
            sample_loading: default_sample_loading(),
            analyzer: default_hashlinear_analyzer(),
            ngram_range: default_hashlinear_ngram_range(),
            n_features: default_hashlinear_n_features(),
            alternate_sign: false,
            norm: default_hashlinear_norm(),
            alpha: default_hashlinear_alpha(),
            epochs: default_hashlinear_epochs(),
            batch_size: default_hashlinear_batch_size(),
            random_seed: default_random_seed(),
            max_seconds: default_max_seconds(),
            use_jieba: false,
        }
    }
}

/// one-class 滑窗打分的按模型名前缀分组的基线分位(异常分空间)。
/// normalized = (anomaly - p50) / (p95 - p50):组内中位异常→0,95 分位(更
/// 异常的尾部)→1,越高越异常。prefix 为 "*" 时表示全局缺省组。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OneClassBaseline {
    pub prefix: String,
    /// 组内异常分中位数(normalized → 0)。
    #[serde(default)]
    pub p50: f64,
    /// 组内 95 分位异常分(normalized → 1)。
    #[serde(default)]
    pub p95: f64,
}

/// 商业化路由的 one-class 决策配置(全部带默认值,不硬编码)。
///
/// 分数空间与标定实验(tv-research/scripts/decision_sim.py)一致:
/// anomaly = -min_windows(TF 归一计数 · 单位质心),越高越异常;
/// 未配置基线表时决策分 = 原始 anomaly,OPERATING_POINT.json 的阈值
/// 可原样填入。阈值语义:score > block_threshold 拦截,score >
/// divert_threshold 存疑(block 线必须高于 divert 线,配反自动取 max)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OneClassConfig {
    /// 模型文件名(相对 profile 目录)。
    #[serde(default = "default_oneclass_model_file")]
    pub model_file: String,
    /// 参与打分的最大字符数。默认 100_000,对齐标定实验的 MAX_CHARS
    /// (run_oneclass2.py/decision_sim.py),超长文本分数分布不漂移。
    #[serde(default = "default_oneclass_max_scan_chars")]
    pub max_scan_chars: usize,
    /// anomaly > block_threshold → 拦截。默认 2.0 不可达(anomaly∈[-1,1]),
    /// 即未标定的模型文件绝不拦截,宁漏切勿误切。
    #[serde(default = "default_oneclass_block_threshold")]
    pub block_threshold: f64,
    /// anomaly > divert_threshold → 存疑走次级上游。默认 2.0 不可达,
    /// 未标定不切流。
    #[serde(default = "default_oneclass_divert_threshold")]
    pub divert_threshold: f64,
    /// 按模型名前缀分组的基线分位表(缺省组用 prefix="*")。
    #[serde(default)]
    pub baselines: Vec<OneClassBaseline>,
    /// glm 复核基础采样率(0~1,默认 0 = 不采样,灰度期零成本)。
    #[serde(default)]
    pub review_base_rate: f64,
    /// 高危模型线前缀列表:命中即强制送 glm 复核(优先于基础采样率)。
    #[serde(default)]
    pub review_forced_model_prefixes: Vec<String>,
    /// glm violation=true 且 confidence >= 此值 → 拦截;低于 → 存疑切上游。
    /// 标定值 C_BLOCK=0.85(decision_sim.py)。
    #[serde(default = "default_oneclass_review_violation_block_conf")]
    pub review_violation_block_conf: f64,
    /// glm violation=false 且 confidence < 此值 且 anomaly > divert 线 →
    /// 存疑切上游;否则豁免走主上游(高置信豁免=保缓存粘性,标定 C_GRAY=0.80)。
    #[serde(default = "default_oneclass_review_cleared_gray_conf")]
    pub review_cleared_gray_conf: f64,
    /// 复核返回不可解析时的策略(对齐 OPERATING_POINT 的 none_policy):
    /// - "self"(默认,运营点选择): 无语义判定 → 放行主上游
    /// - "divert": 降级存疑切次级
    /// - "fallthrough": 按分数走 R4
    /// - "block": fail-closed 拦截
    #[serde(default = "default_oneclass_review_unparseable_policy")]
    pub review_unparseable_policy: String,
}

impl Default for OneClassConfig {
    fn default() -> Self {
        Self {
            model_file: default_oneclass_model_file(),
            max_scan_chars: default_oneclass_max_scan_chars(),
            block_threshold: default_oneclass_block_threshold(),
            divert_threshold: default_oneclass_divert_threshold(),
            baselines: Vec::new(),
            review_base_rate: 0.0,
            review_forced_model_prefixes: Vec::new(),
            review_violation_block_conf: default_oneclass_review_violation_block_conf(),
            review_cleared_gray_conf: default_oneclass_review_cleared_gray_conf(),
            review_unparseable_policy: default_oneclass_review_unparseable_policy(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileConfig {
    #[serde(default)]
    pub ai: AiConfig,
    #[serde(default)]
    pub prompt: PromptConfig,
    #[serde(default)]
    pub probability: ProbabilityConfig,
    #[serde(default = "default_local_model_type")]
    pub local_model_type: String,
    #[serde(default)]
    pub bow_training: BowTrainingConfig,
    #[serde(default)]
    pub fasttext_training: FastTextTrainingConfig,
    #[serde(default)]
    pub hashlinear_training: HashLinearTrainingConfig,
    #[serde(default)]
    pub oneclass: OneClassConfig,
}

impl Default for ProfileConfig {
    fn default() -> Self {
        Self {
            ai: AiConfig::default(),
            prompt: PromptConfig::default(),
            probability: ProbabilityConfig::default(),
            local_model_type: default_local_model_type(),
            bow_training: BowTrainingConfig::default(),
            fasttext_training: FastTextTrainingConfig::default(),
            hashlinear_training: HashLinearTrainingConfig::default(),
            oneclass: OneClassConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ModerationProfile {
    pub profile_name: String,
    pub base_dir: PathBuf,
    pub config: ProfileConfig,
}

impl ModerationProfile {
    pub fn load(root_dir: impl AsRef<Path>, profile_name: &str) -> Result<Self> {
        let base_dir = root_dir
            .as_ref()
            .join("configs")
            .join("mod_profiles")
            .join(profile_name);
        let config_path = base_dir.join("profile.json");
        let config = if config_path.exists() {
            let raw = fs::read_to_string(&config_path)
                .with_context(|| format!("failed to read {}", config_path.display()))?;
            serde_json::from_str(&raw)
                .with_context(|| format!("failed to parse {}", config_path.display()))?
        } else {
            ProfileConfig::default()
        };

        Ok(Self {
            profile_name: profile_name.to_string(),
            base_dir,
            config,
        })
    }

    pub fn history_rocks_path(&self) -> PathBuf {
        self.base_dir.join("history.rocks")
    }

    pub fn history_rocks_path_string(&self) -> String {
        self.history_rocks_path().display().to_string()
    }

    pub fn training_status_path(&self) -> PathBuf {
        self.base_dir.join(".train_status.json")
    }

    pub fn hashlinear_model_path(&self) -> PathBuf {
        self.base_dir.join("hashlinear_model.pkl")
    }

    pub fn hashlinear_runtime_prefix(&self) -> PathBuf {
        self.base_dir.join("hashlinear_runtime")
    }

    pub fn hashlinear_runtime_json_path(&self) -> PathBuf {
        self.hashlinear_runtime_prefix().with_extension("json")
    }

    pub fn hashlinear_runtime_coef_path(&self) -> PathBuf {
        self.hashlinear_runtime_prefix().with_extension("coef.f32")
    }

    pub fn bow_model_path(&self) -> PathBuf {
        self.base_dir.join("bow_model.pkl")
    }

    pub fn bow_vectorizer_path(&self) -> PathBuf {
        self.base_dir.join("bow_vectorizer.pkl")
    }

    pub fn bow_runtime_json_path(&self) -> PathBuf {
        self.base_dir.join("bow_runtime.json")
    }

    pub fn bow_runtime_coef_path(&self) -> PathBuf {
        self.base_dir.join("bow_runtime.coef.f32")
    }

    pub fn fasttext_model_path(&self) -> PathBuf {
        self.base_dir.join("fasttext_model.bin")
    }

    pub fn fasttext_runtime_json_path(&self) -> PathBuf {
        self.base_dir.join("fasttext_runtime.json")
    }

    pub fn training_model_path(&self) -> PathBuf {
        match self.config.local_model_type.as_str() {
            "fasttext" => self.fasttext_model_path(),
            "hashlinear" => self.hashlinear_model_path(),
            _ => self.bow_model_path(),
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn training_max_db_items(&self) -> Result<usize> {
        match self.config.local_model_type.as_str() {
            "fasttext" => Ok(self.config.fasttext_training.max_db_items),
            "hashlinear" => Ok(self.config.hashlinear_training.max_db_items),
            "bow" => Ok(self.config.bow_training.max_db_items),
            other => Err(anyhow::anyhow!("unsupported local_model_type: {other}")),
        }
    }

    pub fn get_prompt_template(&self) -> String {
        let template_path = self.base_dir.join(&self.config.prompt.template_file);
        let cache_key = template_path.display().to_string();
        let signature = prompt_template_signature(&template_path);

        let cache = PROMPT_TEMPLATES.get_or_init(|| Mutex::new(HashMap::new()));
        {
            let guard = cache.lock().expect("prompt template cache");
            if let Some(entry) = guard.get(&cache_key) {
                if entry.signature == signature {
                    return entry.template.clone();
                }
            }
        }

        let template = fs::read_to_string(&template_path)
            .unwrap_or_else(|_| "请判断以下文本是否违规：\n{{text}}".to_string());

        let mut guard = cache.lock().expect("prompt template cache");
        guard.insert(
            cache_key,
            PromptTemplateEntry {
                signature,
                template: template.clone(),
            },
        );
        template
    }

    /// 长文本截断(中段取全文中点)。见 [truncate_text_with_center]。
    pub fn truncate_text(&self, text: &str) -> String {
        self.truncate_text_with_center(text, None)
    }

    /// 长文本截断:头 max/2 + 中段 max/3 + 尾 max/6(30K 配置下即 15K+10K+5K)。
    ///
    /// 旧实现是"头 2/3 + 尾 1/3",中段藏毒会被整体丢掉(可行性研究实测:
    /// 30K 头尾截断下中段藏毒 glm 复核只拦 17.4%)。新实现保留中段。
    /// `center_hint` 为中段中心字符下标:调用方(oneclass 三态链)传入
    /// 滑窗判定的最异常窗中心,让复核优先看到"最不像正常流量"的区段;
    /// None(模型缺失/无信号/旧链路)回退全文中点。三段拼接处沿用
    /// "...[中间省略]..." 分隔符,复核提示词已说明该拼接结构。
    pub fn truncate_text_with_center(&self, text: &str, center_hint: Option<usize>) -> String {
        let max_len = self.config.prompt.max_text_length;
        let chars = text.chars().collect::<Vec<_>>();
        if chars.len() <= max_len {
            return text.to_string();
        }

        let head_len = max_len / 2;
        let middle_len = max_len / 3;
        let tail_len = max_len - head_len - middle_len; // 余数归尾段,三段之和恒等于 max_len
        let total = chars.len();
        let mid_half = middle_len / 2;

        // clamp 保证中段落在 [head_len, total - tail_len) 内,不与头尾重叠
        let lower = head_len + mid_half;
        let upper = (total - tail_len - middle_len + mid_half).max(lower);
        let center = center_hint.unwrap_or(total / 2).clamp(lower, upper);

        let head: String = chars[..head_len].iter().collect();
        let middle: String = chars[(center - mid_half)..(center - mid_half + middle_len)]
            .iter()
            .collect();
        let tail: String = chars[(total - tail_len)..].iter().collect();
        format!("{head}\n...[中间省略]...\n{middle}\n...[中间省略]...\n{tail}")
    }

    pub fn render_prompt(&self, text: &str) -> String {
        self.get_prompt_template()
            .replace("{{text}}", &escape_html(text))
    }

    pub fn local_model_exists(&self) -> bool {
        match self.config.local_model_type.as_str() {
            "fasttext" => self.fasttext_runtime_json_path().exists(),
            "hashlinear" => {
                self.hashlinear_runtime_json_path().exists()
                    && self.hashlinear_runtime_coef_path().exists()
            }
            _ => self.bow_runtime_json_path().exists() && self.bow_runtime_coef_path().exists(),
        }
    }

    pub fn training_status(&self) -> Option<Value> {
        let path = self.training_status_path();
        let raw = fs::read_to_string(path).ok()?;
        serde_json::from_str(&raw).ok()
    }
}

fn prompt_template_signature(path: &Path) -> PromptTemplateSignature {
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path));

    match fs::metadata(&canonical) {
        Ok(metadata) => PromptTemplateSignature {
            modified_at: metadata.modified().ok(),
            len: Some(metadata.len()),
        },
        Err(_) => PromptTemplateSignature {
            modified_at: None,
            len: None,
        },
    }
}

fn escape_html(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#x27;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn default_provider() -> String {
    "openai".to_string()
}
fn default_base_url() -> String {
    "https://api.openai.com/v1".to_string()
}
fn default_model() -> String {
    "gpt-4o-mini".to_string()
}
fn default_api_key_env() -> String {
    "MOD_AI_API_KEY".to_string()
}
fn default_timeout() -> u64 {
    10
}
fn default_max_retries() -> u32 {
    2
}
fn default_template_file() -> String {
    "ai_prompt.txt".to_string()
}
fn default_prompt_max_text_length() -> usize {
    4000
}
fn default_ai_review_rate() -> f64 {
    0.3
}
fn default_random_seed() -> u64 {
    42
}
fn default_low_risk_threshold() -> f64 {
    0.2
}
fn default_high_risk_threshold() -> f64 {
    0.8
}
fn default_min_samples() -> usize {
    200
}
fn default_retrain_interval_minutes() -> usize {
    60
}
fn default_max_samples() -> usize {
    50_000
}
fn default_sample_loading() -> String {
    "balanced_undersample".to_string()
}
fn default_max_features() -> usize {
    8000
}
fn default_char_ngram_range() -> Vec<usize> {
    vec![2, 3]
}
fn default_word_ngram_range() -> Vec<usize> {
    vec![1, 2]
}
fn default_bow_model_type() -> String {
    "sgd_logistic".to_string()
}
fn default_batch_size() -> usize {
    2000
}
fn default_max_seconds() -> usize {
    300
}
fn default_max_db_items() -> usize {
    100_000
}
fn default_vocab_buckets() -> Vec<VocabBucket> {
    vec![
        VocabBucket {
            name: "high_freq".to_string(),
            min_doc_ratio: 0.05,
            max_doc_ratio: 0.6,
            limit: 1200,
        },
        VocabBucket {
            name: "mid_freq".to_string(),
            min_doc_ratio: 0.01,
            max_doc_ratio: 0.05,
            limit: 2600,
        },
        VocabBucket {
            name: "low_freq".to_string(),
            min_doc_ratio: 0.002,
            max_doc_ratio: 0.01,
            limit: 1200,
        },
    ]
}
fn default_bucket_name() -> String {
    "bucket".to_string()
}
fn default_bucket_limit() -> usize {
    1000
}
fn default_one() -> f64 {
    1.0
}
fn default_tiktoken_model() -> String {
    "cl100k_base".to_string()
}
fn default_fasttext_dim() -> usize {
    64
}
fn default_fasttext_lr() -> f64 {
    0.1
}
fn default_fasttext_epoch() -> usize {
    5
}
fn default_fasttext_word_ngrams() -> usize {
    2
}
fn default_fasttext_minn() -> usize {
    2
}
fn default_fasttext_maxn() -> usize {
    4
}
fn default_fasttext_bucket() -> usize {
    200_000
}
fn default_hashlinear_analyzer() -> String {
    "char".to_string()
}
fn default_hashlinear_ngram_range() -> Vec<usize> {
    vec![2, 4]
}
fn default_hashlinear_n_features() -> usize {
    1_048_576
}
fn default_hashlinear_norm() -> Option<String> {
    Some("l2".to_string())
}
fn default_hashlinear_alpha() -> f64 {
    1e-5
}
fn default_hashlinear_epochs() -> usize {
    3
}
fn default_hashlinear_batch_size() -> usize {
    2048
}
fn default_local_model_type() -> String {
    "bow".to_string()
}
fn default_oneclass_model_file() -> String {
    "oneclass_model.json".to_string()
}
fn default_oneclass_max_scan_chars() -> usize {
    // 对齐标定实验 MAX_CHARS=100_000(run_oneclass2.py:29 / decision_sim.py:56)
    100_000
}
/// 拦截线默认 2.0:anomaly∈[-1,1] 永远打不到,即"未标定就绝不拦截"
/// (宁漏切勿误切);真实阈值由 scripts/export_oneclass_model.py 从
/// OPERATING_POINT.json 标定导出。
fn default_oneclass_block_threshold() -> f64 {
    2.0
}
/// 存疑线默认 2.0 同理:未标定不切流,主上游缓存粘性不受影响。
fn default_oneclass_divert_threshold() -> f64 {
    2.0
}
fn default_oneclass_review_violation_block_conf() -> f64 {
    0.85
}
fn default_oneclass_review_cleared_gray_conf() -> f64 {
    0.80
}
fn default_oneclass_review_unparseable_policy() -> String {
    "self".to_string()
}
fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 截断后三段拼接的分隔符字符数("\n...[中间省略]...\n")。
    const SEPARATOR_CHARS: usize = 14;

    fn unique_profile(max_len: usize) -> ModerationProfile {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        // 跨运行唯一命名(时间戳+自增),避免命中上次运行遗留的同名目录;
        // 同名遗留目录做防御性清理(oneclass.rs 测试同款坑)
        static RUN_STAMP: std::sync::OnceLock<u128> = std::sync::OnceLock::new();
        let stamp = *RUN_STAMP.get_or_init(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        });
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let mut config = ProfileConfig::default();
        config.prompt.max_text_length = max_len;
        let base_dir =
            std::env::temp_dir().join(format!("tv-profile-truncate-{stamp}-{id}"));
        let _ = fs::remove_dir_all(&base_dir);
        fs::create_dir_all(&base_dir).expect("create profile dir");
        ModerationProfile {
            profile_name: format!("truncate-{id}"),
            base_dir,
            config,
        }
    }

    fn pad_until(text: &mut String, fill: char, target_chars: usize) {
        while text.chars().count() < target_chars {
            text.push(fill);
        }
    }

    #[test]
    fn truncate_text_keeps_short_text_unchanged() {
        let profile = unique_profile(90);
        let text = "short text stays as is, no truncation applied";
        assert_eq!(profile.truncate_text(text), text);
        // 恰好等于上限也不截断
        let exact: String = "x".repeat(90);
        assert_eq!(profile.truncate_text(&exact), exact);
    }

    #[test]
    fn truncate_text_keeps_head_midpoint_and_tail_segments() {
        // max_len=90 → 头45 + 中段30 + 尾15;全文 300 字符,无 oneclass 模型
        // → 中段取中点(150),覆盖 135..165。
        let profile = unique_profile(90);
        let mut text = String::from("HEADMARK-");
        pad_until(&mut text, 'a', 146);
        text.push_str("MIDMARK"); // 146..153,落在中点窗 135..165 内
        pad_until(&mut text, 'b', 196);
        text.push_str("DROPMARK"); // 196..204,落在丢弃区(165..285)
        pad_until(&mut text, 'c', 292);
        text.push_str("TAILMARK"); // 292..300,落在尾段 285..300 内
        assert_eq!(text.chars().count(), 300);

        let truncated = profile.truncate_text(&text);
        for marker in ["HEADMARK", "MIDMARK", "TAILMARK"] {
            assert!(
                truncated.contains(marker),
                "truncated output must contain {marker}"
            );
        }
        assert!(
            !truncated.contains("DROPMARK"),
            "marker in dropped zone must not survive: {truncated}"
        );
        // 三段之和 == max_len,外加两个分隔符
        assert_eq!(
            truncated.chars().count(),
            90 + 2 * SEPARATOR_CHARS,
            "unexpected truncated length"
        );
        assert_eq!(
            truncated.matches("...[中间省略]...").count(),
            2,
            "expected exactly two omission separators"
        );
    }

    #[test]
    fn truncate_text_prefers_center_hint_over_midpoint() {
        // max_len=120 → 头60 + 中段40 + 尾20;全文 400 字符。
        // 中点中段覆盖 180..220;异质段在 258..350,中点定位会整体丢弃。
        let profile = unique_profile(120);

        let normal_unit = "normal chat about weather and code "; // 35 chars
        let mut text = normal_unit.repeat(7); // 245 chars
        pad_until(&mut text, 'n', 258);
        let alien: String = "zqxjwv-".repeat(14).chars().take(92).collect();
        text.push_str(&alien); // 258..350:异质段(远离中点窗)
        pad_until(&mut text, 'k', 400);
        assert_eq!(text.chars().count(), 400);

        // center_hint = 最异常窗中心(模拟 oneclass 定位到异质段,如窗
        // 280..320 的中心 300):中段随之移动,异质内容必须保留。
        let truncated = profile.truncate_text_with_center(&text, Some(300));
        assert!(
            truncated.contains("zqxjwv"),
            "anomaly-centered middle segment must keep the anomalous content"
        );
        assert!(
            truncated.starts_with("normal"),
            "head segment must be the text head"
        );
        assert_eq!(
            truncated.chars().count(),
            120 + 2 * SEPARATOR_CHARS,
            "unexpected truncated length"
        );

        // 中点回退(对照):中段 180..220 不含异质段(258..350),必须被
        // 整体丢弃——证明上一步的保留来自 center_hint,而非段本身够长。
        let midpoint_truncated = profile.truncate_text(&text);
        assert!(
            !midpoint_truncated.contains("zqxjwv"),
            "midpoint fallback must drop the off-center anomaly"
        );
    }

    #[test]
    fn truncate_text_clamps_center_hint_into_head_tail_gap() {
        // center_hint 指向头段内部或越界:中段必须被夹回 [head, total-tail) 区间,
        // 不与头尾重叠,也不越界 panic。
        let profile = unique_profile(120);
        let text = "x".repeat(400);
        for hint in [0, 10, 59, 400, 10_000] {
            let truncated = profile.truncate_text_with_center(&text, Some(hint));
            assert_eq!(
                truncated.chars().count(),
                120 + 2 * SEPARATOR_CHARS,
                "hint={hint} must still produce exactly head+middle+tail"
            );
            assert_eq!(
                truncated.matches("...[中间省略]...").count(),
                2,
                "hint={hint} must keep two separators"
            );
        }
        // hint 指向头段内(10)→ clamp 到下界,中段从 head 开始
        let clamped = profile.truncate_text_with_center(&text, Some(10));
        let parts: Vec<&str> = clamped.split("\n...[中间省略]...\n").collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].chars().count(), 60, "head is 60 chars");
        assert_eq!(parts[1].chars().count(), 40, "middle is 40 chars");
        assert_eq!(parts[2].chars().count(), 20, "tail is 20 chars");
    }

    #[test]
    fn truncate_handles_multibyte_text_without_panicking() {
        // 全中文(多字节)长文本:截断按 char 计数,不得 panic 且尾段保留
        let profile = unique_profile(30);
        let mut text: String = "长".repeat(10_000);
        text.push_str("标记中文"); // 最后 4 字符,必落在尾段(尾段=5 字符)
        let truncated = profile.truncate_text(&text);
        assert!(truncated.contains("标记中文"));
        assert!(truncated.chars().count() <= 30 + 2 * SEPARATOR_CHARS);
    }
}
