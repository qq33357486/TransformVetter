use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use std::time::Instant;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use indexmap::IndexMap;
use reqwest::header::CONTENT_TYPE;
use reqwest::Client;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::timeout as tokio_timeout;

use crate::moderation::bow;
use crate::moderation::fasttext;
use crate::moderation::hashlinear;
use crate::moderation::oneclass;
use crate::profile::ModerationProfile;
#[cfg(feature = "storage-debug")]
use crate::storage::SampleStorage;

const MAX_LLM_CONCURRENCY: usize = 5;
const CACHE_SIZE: usize = 1024;
const MAX_CACHE_PROFILES: usize = 50;
const CACHE_TTL: Duration = Duration::from_secs(60 * 60);

static LLM_SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();
static MODERATION_CACHE: OnceLock<
    Mutex<IndexMap<String, IndexMap<String, ModerationCacheEntry>>>,
> = OnceLock::new();
#[cfg(feature = "storage-debug")]
static HISTORY_STORAGE_LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
static REQUEST_RANDOM_STATE: OnceLock<AtomicU64> = OnceLock::new();
#[cfg(test)]
use std::collections::VecDeque;
#[cfg(test)]
static TEST_RANDOM_VALUES: OnceLock<Mutex<VecDeque<f64>>> = OnceLock::new();

#[derive(Debug, Clone, Serialize)]
pub struct SmartModerationResult {
    pub violation: bool,
    pub category: Option<String>,
    pub reason: Option<String>,
    pub source: String,
    pub confidence: Option<f64>,
    #[serde(skip_serializing)]
    pub debug: Option<ModerationDebugInfo>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ModerationDebugInfo {
    pub local_model_source: Option<String>,
    pub local_model_confidence: Option<f64>,
    pub local_model_latency_ms: Option<f64>,
    pub local_model_decision: Option<String>,
    pub llm_reviewed: bool,
    pub llm_result: Option<String>,
    pub llm_latency_ms: Option<f64>,
    pub llm_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalDecision {
    Allow,
    Block,
    Uncertain,
}

#[derive(Debug, Clone)]
struct LocalModelEvaluation {
    source: String,
    probability: f64,
    decision: LocalDecision,
    latency_ms: f64,
}

impl LocalModelEvaluation {
    fn debug_info(&self) -> ModerationDebugInfo {
        let local_model_decision = match self.decision {
            LocalDecision::Allow => "allow",
            LocalDecision::Block => "block",
            LocalDecision::Uncertain => "uncertain",
        };
        ModerationDebugInfo {
            local_model_source: Some(self.source.clone()),
            local_model_confidence: Some(self.probability),
            local_model_latency_ms: Some(self.latency_ms),
            local_model_decision: Some(local_model_decision.to_string()),
            ..Default::default()
        }
    }

    fn direct_result(&self) -> Option<SmartModerationResult> {
        match self.decision {
            LocalDecision::Allow => Some(SmartModerationResult {
                violation: false,
                category: None,
                reason: Some(format!(
                    "{}: low risk (p={:.3})",
                    self.source, self.probability
                )),
                source: self.source.clone(),
                confidence: Some(self.probability),
                debug: None,
            }),
            LocalDecision::Block => Some(SmartModerationResult {
                violation: true,
                category: None,
                reason: Some(format!(
                    "{}: high risk (p={:.3})",
                    self.source, self.probability
                )),
                source: self.source.clone(),
                confidence: Some(self.probability),
                debug: None,
            }),
            LocalDecision::Uncertain => None,
        }
    }

    fn fallback_result(&self, profile: &ModerationProfile) -> SmartModerationResult {
        let threshold = fallback_threshold(profile);
        let violation = self.probability >= threshold;
        SmartModerationResult {
            violation,
            category: None,
            reason: Some(format!(
                "{}: llm concurrency fallback (p={:.3}, threshold={threshold:.3})",
                self.source, self.probability
            )),
            source: "local_fallback".to_string(),
            confidence: Some(self.probability),
            debug: None,
        }
    }
}

#[derive(Debug)]
pub enum SmartModerationError {
    ConcurrencyLimit(String),
    /// LLM 复核"返回了内容但解析不出结论 JSON"(宽松提取重试后仍失败)。
    /// 与传输错误/超时区分开:三态路由把它降级为存疑(Divert)而非报错,
    /// 见 route_moderation 中的产品决策注释。
    LlmUnparseable(anyhow::Error, Option<ModerationDebugInfo>),
    Other(anyhow::Error, Option<ModerationDebugInfo>),
}

impl From<anyhow::Error> for SmartModerationError {
    fn from(value: anyhow::Error) -> Self {
        Self::Other(value, None)
    }
}

/// 三态路由结论:拦截 / 存疑走次级上游 / 合规走主上游。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModerationRoute {
    Block,
    Divert,
    Allow,
}

impl ModerationRoute {
    pub fn as_str(self) -> &'static str {
        match self {
            ModerationRoute::Block => "block",
            ModerationRoute::Divert => "divert",
            ModerationRoute::Allow => "allow",
        }
    }
}

/// 商业化路由的一次完整决策(one-class 打分 + glm 复核采样)。
#[derive(Debug, Clone)]
pub struct RouteDecision {
    pub route: ModerationRoute,
    /// 决策分(= oneclass normalized_anomaly,越高越异常)。
    pub score: f64,
    /// 最异常窗原始异常分(anomaly = -TF·质心相似度)。
    pub raw_anomaly: f64,
    /// 命中的基线分组。
    pub baseline_group: String,
    /// 请求模型名(用于采样与基线)。
    pub model_name: String,
    /// 是否真的送了 glm 复核。
    pub llm_reviewed: bool,
    pub debug: ModerationDebugInfo,
}

/// 三态决策链,逐条对齐标定实验 decision_sim.decide()(OPERATING_POINT):
/// - R2 anomaly > block_threshold → 拦截(形态极异常,硬拦,先于复核)
/// - R3 采样送审且 glm 有判定:
///     violation & conf >= review_violation_block_conf(0.85) → 拦截
///     violation & conf <  0.85 → 存疑切上游
///     cleared  & conf <  review_cleared_gray_conf(0.80) 且 anomaly >
///              divert_threshold → 存疑切上游
///     其余 cleared(高置信豁免)→ 主上游,保缓存粘性
/// - R3b 复核不可解析:按 review_unparseable_policy(默认 self,运营点选择)
/// - R4 anomaly > divert_threshold → 存疑切上游
/// - R5 主上游
/// glm 复核按 `review_base_rate` 基础采样 + 高危模型前缀强制采样;
/// 传输错误/超时按分数继续路由(可用性优先,风险由采样审计兜底)。
///
/// 返回 Ok(None) 表示 oneclass 模型不可用(文件缺失/损坏):
/// 调用方应降级为现状 smart_moderation 决策链,不影响可用性。
pub async fn route_moderation(
    text: &str,
    model_name: &str,
    cfg: &Value,
    root_dir: &Path,
    http_client: &Client,
    env_map: &HashMap<String, String>,
) -> Result<Option<RouteDecision>, SmartModerationError> {
    if !cfg.get("enabled").and_then(Value::as_bool).unwrap_or(false) {
        return Ok(None);
    }

    let profile_name = cfg
        .get("profile")
        .and_then(Value::as_str)
        .unwrap_or("default");
    let profile = ModerationProfile::load(root_dir, profile_name)?;

    // oneclass 用全文(不截断)——中段藏毒正是截断漏洞,滑窗必须覆盖
    let Some(score) = oneclass::score_request(text, model_name, &profile)? else {
        return Ok(None);
    };

    let oneclass_cfg = &profile.config.oneclass;
    // anomaly 取向:score > block 线拦截;block 线必须不低于 divert 线
    let divert_threshold = oneclass_cfg.divert_threshold;
    let block_threshold = oneclass_cfg.block_threshold.max(divert_threshold);
    let decision_score = score.normalized_anomaly;

    let mut llm_reviewed = false;
    let mut llm_result: Option<String> = None;
    let mut review_verdict: Option<ReviewVerdict> = None;

    // R2:形态极异常直接拦截,无需 glm 确认
    if decision_score > block_threshold {
        tracing::info!(
            model = %model_name,
            score = decision_score,
            raw_anomaly = score.anomaly,
            baseline_group = %score.baseline_group,
            block_threshold,
            windows = score.window_count,
            "oneclass route: block"
        );
        return Ok(Some(RouteDecision {
            route: route_for_score(decision_score, block_threshold, divert_threshold),
            score: decision_score,
            raw_anomaly: score.anomaly,
            baseline_group: score.baseline_group,
            model_name: model_name.to_string(),
            llm_reviewed: false,
            debug: ModerationDebugInfo {
                local_model_source: Some("oneclass_model".to_string()),
                local_model_confidence: Some(decision_score),
                local_model_decision: Some("block".to_string()),
                ..Default::default()
            },
        }));
    }

    if should_review_model(model_name, oneclass_cfg) {
        // glm 复核:用截断文本;中段对准 oneclass 最异常窗(score_request
        // 已算出,这里直接复用,不重复打分),让复核优先看到最可疑区段
        let center_hint = score.anomaly_window.map(|(start, end)| (start + end) / 2);
        let truncated = profile.truncate_text_with_center(text, center_hint);
        if let Some(cached) = check_cache(profile_name, &truncated) {
            llm_reviewed = true;
            review_verdict = Some(if cached.violation {
                ReviewVerdict::Violation {
                    confidence: cached.confidence.unwrap_or(0.0),
                }
            } else {
                ReviewVerdict::Cleared {
                    confidence: cached.confidence.unwrap_or(0.0),
                }
            });
            llm_result = Some(if cached.violation { "block" } else { "allow" }.to_string());
        } else {
            match run_ai_moderation_with_history(
                &truncated,
                &profile,
                http_client,
                env_map,
                None,
                None,
            )
            .await
            {
                Ok(result) => {
                    save_cache(profile_name, &truncated, &result);
                    llm_reviewed = true;
                    review_verdict = Some(if result.violation {
                        ReviewVerdict::Violation {
                            confidence: result.confidence.unwrap_or(0.0),
                        }
                    } else {
                        ReviewVerdict::Cleared {
                            confidence: result.confidence.unwrap_or(0.0),
                        }
                    });
                    llm_result =
                        Some(if result.violation { "block" } else { "allow" }.to_string());
                }
                Err(err) => {
                    // 复核是抽检,失败不阻断业务;解析失败(LlmUnparseable)是
                    // "有内容但读不懂",按 review_unparseable_policy 处理;
                    // 传输错误/超时是基础设施问题,按分数继续路由(R4)。
                    let message = match &err {
                        SmartModerationError::ConcurrencyLimit(message) => message.clone(),
                        SmartModerationError::LlmUnparseable(error, _) => format!("{error:#}"),
                        SmartModerationError::Other(error, _) => format!("{error:#}"),
                    };
                    if matches!(err, SmartModerationError::LlmUnparseable(..)) {
                        llm_reviewed = true;
                        review_verdict = Some(ReviewVerdict::Unparseable);
                        llm_result = Some("unparseable".to_string());
                        tracing::warn!(
                            model = %model_name,
                            score = decision_score,
                            policy = %oneclass_cfg.review_unparseable_policy,
                            error = %message,
                            "oneclass route: glm review unparseable"
                        );
                    } else {
                        tracing::warn!(
                            model = %model_name,
                            score = decision_score,
                            error = %message,
                            "oneclass route: glm review failed; continuing by score"
                        );
                    }
                }
            }
        }
    }

    // R3/R3b(有复核结论)或 R4(未采样/传输失败)
    let route = match &review_verdict {
        Some(verdict) => route_after_review(
            verdict,
            decision_score,
            block_threshold,
            divert_threshold,
            oneclass_cfg,
        ),
        None => route_for_score(decision_score, block_threshold, divert_threshold),
    };
    tracing::info!(
        model = %model_name,
        route = route.as_str(),
        score = decision_score,
        raw_anomaly = score.anomaly,
        baseline_group = %score.baseline_group,
        divert_threshold,
        block_threshold,
        windows = score.window_count,
        llm_reviewed,
        "oneclass route decision"
    );
    Ok(Some(RouteDecision {
        route,
        score: decision_score,
        raw_anomaly: score.anomaly,
        baseline_group: score.baseline_group,
        model_name: model_name.to_string(),
        llm_reviewed,
        debug: ModerationDebugInfo {
            local_model_source: Some("oneclass_model".to_string()),
            local_model_confidence: Some(decision_score),
            local_model_decision: Some(route.as_str().to_string()),
            llm_reviewed,
            llm_result,
            ..Default::default()
        },
    }))
}

/// glm 复核结论(R3 输入)。Unparseable = 返回了内容但解析不出判定。
#[derive(Debug, Clone, Copy)]
enum ReviewVerdict {
    Violation { confidence: f64 },
    Cleared { confidence: f64 },
    Unparseable,
}

/// R3:复核结论 → 路由(逐条对齐 decision_sim.decide() 的 R3/R3b):
/// - violation & conf >= review_violation_block_conf → Block
/// - violation & conf 低于线 → Divert(存疑切上游,不硬拦)
/// - cleared & conf < review_cleared_gray_conf & anomaly > divert 线 → Divert
/// - 其余 cleared(高置信豁免)→ Allow(主上游,保缓存粘性)
/// - Unparseable → 按 review_unparseable_policy:self/divert/fallthrough/block
fn route_after_review(
    verdict: &ReviewVerdict,
    score: f64,
    block_threshold: f64,
    divert_threshold: f64,
    cfg: &crate::profile::OneClassConfig,
) -> ModerationRoute {
    match verdict {
        ReviewVerdict::Violation { confidence } => {
            if *confidence >= cfg.review_violation_block_conf {
                ModerationRoute::Block
            } else {
                ModerationRoute::Divert
            }
        }
        ReviewVerdict::Cleared { confidence } => {
            if *confidence < cfg.review_cleared_gray_conf && score > divert_threshold {
                ModerationRoute::Divert
            } else {
                // 高置信豁免:老师明确放行,不因形态分切流(误切率标定的关键)
                ModerationRoute::Allow
            }
        }
        ReviewVerdict::Unparseable => match cfg.review_unparseable_policy.as_str() {
            "block" => ModerationRoute::Block,
            "divert" => ModerationRoute::Divert,
            "fallthrough" => route_for_score(score, block_threshold, divert_threshold),
            // "self":运营点 none_policy,无语义判定 → 放行主上游
            _ => ModerationRoute::Allow,
        },
    }
}

/// glm 复核采样:高危模型前缀强制(大小写不敏感前缀匹配),
/// 否则按基础采样率抽签。
fn should_review_model(model_name: &str, cfg: &crate::profile::OneClassConfig) -> bool {
    let lowered = model_name.to_ascii_lowercase();
    if cfg
        .review_forced_model_prefixes
        .iter()
        .any(|prefix| !prefix.is_empty() && lowered.starts_with(&prefix.to_ascii_lowercase()))
    {
        return true;
    }
    let rate = cfg.review_base_rate;
    if rate <= 0.0 {
        return false;
    }
    if rate >= 1.0 {
        return true;
    }
    next_random_unit() < rate
}

/// 三态边界(规格原文):
/// score < block → Block;block <= score < divert → Divert;score >= divert → Allow。
/// R2/R4 的分数路由(anomaly 取向,对齐 decision_sim):
/// score > block 线 → Block;score > divert 线 → Divert;否则 Allow。
fn route_for_score(score: f64, block_threshold: f64, divert_threshold: f64) -> ModerationRoute {
    if score > block_threshold {
        ModerationRoute::Block
    } else if score > divert_threshold {
        ModerationRoute::Divert
    } else {
        ModerationRoute::Allow
    }
}

pub async fn smart_moderation(
    text: &str,
    cfg: &Value,
    root_dir: &Path,
    http_client: &Client,
    env_map: &HashMap<String, String>,
) -> Result<Option<SmartModerationResult>, SmartModerationError> {
    if !cfg.get("enabled").and_then(Value::as_bool).unwrap_or(false) {
        return Ok(None);
    }

    let profile_name = cfg
        .get("profile")
        .and_then(Value::as_str)
        .unwrap_or("default");
    let profile = ModerationProfile::load(root_dir, profile_name)?;
    // 现状链同样受益于中段截断修复:oneclass 模型在就复用其最异常窗定位
    // (缺失时回退全文中点)。打分只做定位用,不影响本链的决策逻辑。
    let center_hint = oneclass::most_anomalous_window_center(text, &profile);
    let text = profile.truncate_text_with_center(text, center_hint);

    if let Some(result) = check_cache(profile_name, &text) {
        return Ok(Some(result));
    }

    let result = decide_moderation(&text, &profile, http_client, env_map).await?;
    save_cache(profile_name, &text, &result);
    Ok(Some(result))
}

pub fn llm_semaphore() -> Arc<Semaphore> {
    LLM_SEMAPHORE
        .get_or_init(|| Arc::new(Semaphore::new(MAX_LLM_CONCURRENCY)))
        .clone()
}

pub fn try_acquire_llm_slot() -> Result<OwnedSemaphorePermit, SmartModerationError> {
    llm_semaphore().try_acquire_owned().map_err(|_| {
        SmartModerationError::ConcurrencyLimit(format!(
            "LLM审核并发数超限(max={MAX_LLM_CONCURRENCY})"
        ))
    })
}

async fn decide_moderation(
    text: &str,
    profile: &ModerationProfile,
    http_client: &Client,
    env_map: &HashMap<String, String>,
) -> Result<SmartModerationResult, SmartModerationError> {
    if should_force_ai_review(profile) {
        let (local_evaluation, debug) =
            optional_local_evaluation_for_concurrency_fallback(text, profile);
        let inherited_debug = (!is_empty_debug(&debug)).then_some(debug);
        return run_ai_moderation_with_history(
            text,
            profile,
            http_client,
            env_map,
            local_evaluation,
            inherited_debug,
        )
        .await;
    }

    if profile.local_model_exists() {
        let (evaluation, debug) = evaluate_local_model_state(text, profile);
        if let Some(evaluation) = evaluation {
            if let Some(mut result) = evaluation.direct_result() {
                result.debug = Some(debug);
                return Ok(result);
            }

            return run_ai_moderation_with_history(
                text,
                profile,
                http_client,
                env_map,
                Some(evaluation),
                Some(debug),
            )
            .await;
        }

        return run_ai_moderation_with_history(
            text,
            profile,
            http_client,
            env_map,
            None,
            Some(debug),
        )
        .await;
    }

    run_ai_moderation_with_history(text, profile, http_client, env_map, None, None).await
}

fn should_use_concurrency_limit_fallback(profile: &ModerationProfile) -> bool {
    profile.config.probability.enable_concurrency_limit_fallback
}

fn fallback_threshold(profile: &ModerationProfile) -> f64 {
    let low = profile.config.probability.low_risk_threshold;
    let high = profile.config.probability.high_risk_threshold;
    (low + high) / 2.0
}

fn is_empty_debug(debug: &ModerationDebugInfo) -> bool {
    debug.local_model_source.is_none()
        && debug.local_model_confidence.is_none()
        && debug.local_model_latency_ms.is_none()
        && debug.local_model_decision.is_none()
        && !debug.llm_reviewed
        && debug.llm_result.is_none()
        && debug.llm_latency_ms.is_none()
        && debug.llm_error.is_none()
}

fn optional_local_evaluation_for_concurrency_fallback(
    text: &str,
    profile: &ModerationProfile,
) -> (Option<LocalModelEvaluation>, ModerationDebugInfo) {
    if should_use_concurrency_limit_fallback(profile) && profile.local_model_exists() {
        return evaluate_local_model_state(text, profile);
    }

    (None, ModerationDebugInfo::default())
}

fn evaluate_local_model(
    text: &str,
    profile: &ModerationProfile,
) -> (Option<SmartModerationResult>, ModerationDebugInfo) {
    let (evaluation, debug) = evaluate_local_model_state(text, profile);
    (evaluation.and_then(|item| item.direct_result()), debug)
}

fn evaluate_local_model_state(
    text: &str,
    profile: &ModerationProfile,
) -> (Option<LocalModelEvaluation>, ModerationDebugInfo) {
    let started = Instant::now();
    let (probability, source) = match profile.config.local_model_type.as_str() {
        "fasttext" => match fasttext::predict_proba(text, profile) {
            Ok(Some(probability)) => (probability, "fasttext_model"),
            Ok(None) | Err(_) => {
                return (
                    None,
                    ModerationDebugInfo {
                        local_model_source: Some("fasttext_model".to_string()),
                        local_model_decision: Some("skipped".to_string()),
                        ..Default::default()
                    },
                )
            }
        },
        "hashlinear" => match hashlinear::predict_proba(text, profile) {
            Ok(Some(probability)) => (probability, "hashlinear_model"),
            Ok(None) | Err(_) => {
                return (
                    None,
                    ModerationDebugInfo {
                        local_model_source: Some("hashlinear_model".to_string()),
                        local_model_decision: Some("skipped".to_string()),
                        ..Default::default()
                    },
                )
            }
        },
        _ => match bow::predict_proba(text, profile) {
            Ok(Some(probability)) => (probability, "bow_model"),
            Ok(None) | Err(_) => {
                return (
                    None,
                    ModerationDebugInfo {
                        local_model_source: Some("bow_model".to_string()),
                        local_model_decision: Some("skipped".to_string()),
                        ..Default::default()
                    },
                )
            }
        },
    };
    let low = profile.config.probability.low_risk_threshold;
    let high = profile.config.probability.high_risk_threshold;
    let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
    let decision = if probability < low {
        LocalDecision::Allow
    } else if probability > high {
        LocalDecision::Block
    } else {
        LocalDecision::Uncertain
    };
    let evaluation = LocalModelEvaluation {
        source: source.to_string(),
        probability,
        decision,
        latency_ms,
    };
    let debug = evaluation.debug_info();
    (Some(evaluation), debug)
}

fn should_force_ai_review(profile: &ModerationProfile) -> bool {
    let rate = profile.config.probability.ai_review_rate;
    if rate <= 0.0 {
        return false;
    }
    if rate >= 1.0 {
        return true;
    }

    next_random_unit() < rate
}

async fn llm_moderate(
    text: &str,
    profile: &ModerationProfile,
    http_client: &Client,
    env_map: &HashMap<String, String>,
) -> Result<SmartModerationResult, SmartModerationError> {
    let _permit = try_acquire_llm_slot()?;

    let api_key_env = profile.config.ai.api_key_env.as_str();
    let api_key = env_map
        .get(api_key_env)
        .cloned()
        .or_else(|| std::env::var(api_key_env).ok())
        .ok_or_else(|| anyhow!("environment variable {api_key_env} not set"))?;
    let models = profile.config.ai.get_model_candidates();
    if models.is_empty() {
        return Err(anyhow!("AI config model is empty").into());
    }

    let prompt = profile.render_prompt(text);
    let timeout = Duration::from_secs(profile.config.ai.timeout.max(1));
    let max_retries = profile.config.ai.max_retries as usize;
    let endpoint = format!(
        "{}/chat/completions",
        profile.config.ai.base_url.trim_end_matches('/')
    );

    // NOTE:
    // - Always request a streaming response (SSE) so timing out will cancel the in-flight body read,
    //   which causes the underlying HTTP connection to be dropped instead of being left hanging.
    // - Enforce timeout with `tokio::time::timeout` so it cannot "sometimes not timeout".
    async fn parse_llm_response(
        response: reqwest::Response,
    ) -> Result<SmartModerationResult, SmartModerationError> {
        let response = response
            .error_for_status()
            .map_err(|err| SmartModerationError::Other(anyhow!(err), None))?;

        let is_sse = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.starts_with("text/event-stream"))
            .unwrap_or(false);

        if is_sse {
            let content = read_openai_chat_sse_content(response)
                .await
                .map_err(|err| SmartModerationError::Other(err, None))?;
            parse_moderation_content(&content)
        } else {
            let payload = response
                .json::<Value>()
                .await
                .map_err(|err| SmartModerationError::Other(anyhow!(err), None))?;
            parse_openai_moderation_response(payload)
        }
    }

    let mut attempted_models = Vec::new();
    let mut last_error: Option<SmartModerationError> = None;
    let started = Instant::now();
    for _attempt in 0..=max_retries {
        let model = pick_model_for_attempt(&models, &attempted_models);
        attempted_models.push(model.clone());

        let response = tokio_timeout(timeout, async {
            let response = http_client
                .post(&endpoint)
                .bearer_auth(&api_key)
                .json(&json!({
                    "model": &model,
                    "messages": [{
                        "role": "user",
                        "content": prompt
                    }],
                    "temperature": 0,
                    "stream": true
                }))
                .send()
                .await
                .map_err(|err| SmartModerationError::Other(anyhow!(err), None))?;

            parse_llm_response(response).await
        })
        .await;

        match response {
            Ok(Ok(mut parsed)) => {
                parsed.debug = Some(ModerationDebugInfo {
                    llm_reviewed: true,
                    llm_result: Some(if parsed.violation {
                        "block".to_string()
                    } else {
                        "allow".to_string()
                    }),
                    llm_latency_ms: Some(started.elapsed().as_secs_f64() * 1000.0),
                    ..Default::default()
                });
                return Ok(parsed);
            }
            Ok(Err(err)) => {
                // 保留 LlmUnparseable 语义:路由层要靠它区分"读不懂"与
                // "调不通",重试循环本身不改写错误类别。
                last_error = Some(match err {
                    SmartModerationError::ConcurrencyLimit(message) => {
                        SmartModerationError::Other(
                            anyhow!(message).context("llm moderation request failed"),
                            None,
                        )
                    }
                    SmartModerationError::LlmUnparseable(err, _) => {
                        SmartModerationError::LlmUnparseable(
                            err.context("llm moderation request failed"),
                            None,
                        )
                    }
                    SmartModerationError::Other(err, _) => SmartModerationError::Other(
                        err.context("llm moderation request failed"),
                        None,
                    ),
                });
            }
            Err(_) => {
                last_error = Some(SmartModerationError::Other(
                    anyhow!(
                        "llm moderation request timed out after {}s (connection aborted)",
                        timeout.as_secs().max(1)
                    ),
                    None,
                ));
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        SmartModerationError::Other(anyhow!("llm moderation request failed: unknown error"), None)
    }))
}

async fn read_openai_chat_sse_content(response: reqwest::Response) -> Result<String> {
    use futures_util::StreamExt;

    let mut stream = response.bytes_stream();
    let mut buffer: Vec<u8> = Vec::new();
    let mut content = String::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buffer.extend_from_slice(&chunk);

        while let Some(newline_idx) = buffer.iter().position(|b| *b == b'\n') {
            let mut line = buffer.drain(..=newline_idx).collect::<Vec<u8>>();
            while matches!(line.last(), Some(b'\n' | b'\r')) {
                line.pop();
            }

            if line.is_empty() {
                continue;
            }

            let Ok(line) = std::str::from_utf8(&line) else {
                continue;
            };
            let trimmed = line.trim();
            let Some(data) = trimmed.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() {
                continue;
            }
            if data == "[DONE]" {
                return Ok(content);
            }

            let Ok(payload) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            if let Some(delta) = extract_openai_chat_stream_delta(&payload) {
                content.push_str(&delta);
            }
        }
    }

    Ok(content)
}

fn extract_openai_chat_stream_delta(payload: &Value) -> Option<String> {
    let choice = payload
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())?;

    if let Some(delta) = choice.get("delta") {
        if let Some(content) = delta.get("content") {
            let text = extract_content_text(content);
            if !text.is_empty() {
                return Some(text);
            }
        }
    }

    // Some upstreams may return a nonstandard streaming shape.
    if let Some(message) = choice.get("message") {
        if let Some(content) = message.get("content") {
            let text = extract_content_text(content);
            if !text.is_empty() {
                return Some(text);
            }
        }
    }

    None
}

async fn run_ai_moderation_with_history(
    text: &str,
    profile: &ModerationProfile,
    http_client: &Client,
    env_map: &HashMap<String, String>,
    local_evaluation: Option<LocalModelEvaluation>,
    inherited_debug: Option<ModerationDebugInfo>,
) -> Result<SmartModerationResult, SmartModerationError> {
    if let Some(mut result) = load_history_result(text, profile).await? {
        let mut debug = inherited_debug.unwrap_or_default();
        debug.llm_reviewed = true;
        debug.llm_result = Some(if result.violation {
            "block".to_string()
        } else {
            "allow".to_string()
        });
        result.debug = Some(debug);
        return Ok(result);
    }
    let mut result = match llm_moderate(text, profile, http_client, env_map).await {
        Ok(result) => result,
        Err(SmartModerationError::ConcurrencyLimit(message))
            if should_use_concurrency_limit_fallback(profile) && local_evaluation.is_some() =>
        {
            let mut result = local_evaluation
                .as_ref()
                .expect("local evaluation checked above")
                .fallback_result(profile);
            let mut debug = inherited_debug.clone().unwrap_or_default();
            debug.llm_reviewed = true;
            debug.llm_result = Some("error".to_string());
            debug.llm_error = Some(truncate_header_value(&message));
            result.debug = Some(debug);
            return Ok(result);
        }
        Err(err) => {
            let mut debug = inherited_debug.clone().unwrap_or_default();
            debug.llm_reviewed = true;
            debug.llm_result = Some("error".to_string());
            debug.llm_error = Some(truncate_header_value(&match &err {
                SmartModerationError::ConcurrencyLimit(message) => message.clone(),
                SmartModerationError::LlmUnparseable(error, _) => format!("{error:#}"),
                SmartModerationError::Other(error, _) => format!("{error:#}"),
            }));
            return Err(match err {
                SmartModerationError::ConcurrencyLimit(message) => {
                    SmartModerationError::Other(anyhow!(message), Some(debug))
                }
                SmartModerationError::LlmUnparseable(error, _) => {
                    SmartModerationError::LlmUnparseable(error, Some(debug))
                }
                SmartModerationError::Other(error, _) => {
                    SmartModerationError::Other(error, Some(debug))
                }
            });
        }
    };
    let mut debug = inherited_debug.unwrap_or_default();
    let latency = result.debug.as_ref().and_then(|value| value.llm_latency_ms);
    debug.llm_reviewed = true;
    debug.llm_result = Some(if result.violation {
        "block".to_string()
    } else {
        "allow".to_string()
    });
    debug.llm_latency_ms = latency;
    result.debug = Some(debug);
    save_history_result(text, profile, &result).await?;
    Ok(result)
}

#[cfg(feature = "storage-debug")]
async fn load_history_result(
    text: &str,
    profile: &ModerationProfile,
) -> Result<Option<SmartModerationResult>, SmartModerationError> {
    let text = text.to_string();
    let profile = profile.clone();
    tokio::task::spawn_blocking(move || {
        let storage_lock = history_storage_lock(&profile.profile_name);
        let _guard = storage_lock.lock().expect("history storage lock");
        let storage = match SampleStorage::open_read_only(profile.history_rocks_path()) {
            Ok(storage) => storage,
            Err(_) => return Ok(None),
        };
        let Some(sample) = storage.find_by_text(&text)? else {
            return Ok(None);
        };

        Ok(Some(SmartModerationResult {
            violation: sample
                .value
                .get("label")
                .and_then(Value::as_i64)
                .unwrap_or_default()
                != 0,
            category: sample
                .value
                .get("category")
                .and_then(Value::as_str)
                .map(ToString::to_string),
            reason: sample
                .value
                .get("created_at")
                .and_then(Value::as_str)
                .map(|created_at| format!("From DB: {created_at}")),
            source: "ai".to_string(),
            confidence: None,
            debug: None,
        }))
    })
    .await
    .map_err(|err| {
        SmartModerationError::Other(anyhow!("history storage read task failed: {err}"), None)
    })?
}

#[cfg(not(feature = "storage-debug"))]
async fn load_history_result(
    _text: &str,
    _profile: &ModerationProfile,
) -> Result<Option<SmartModerationResult>, SmartModerationError> {
    Ok(None)
}

#[cfg(feature = "storage-debug")]
async fn save_history_result(
    text: &str,
    profile: &ModerationProfile,
    result: &SmartModerationResult,
) -> Result<(), SmartModerationError> {
    let text = text.to_string();
    let profile = profile.clone();
    let result = result.clone();
    tokio::task::spawn_blocking(move || {
        let storage_lock = history_storage_lock(&profile.profile_name);
        let _guard = storage_lock.lock().expect("history storage lock");
        let storage = SampleStorage::open_read_write(profile.history_rocks_path())?;
        let label = if result.violation { 1 } else { 0 };
        storage.save_sample(&text, label, result.category.as_deref())?;
        Ok(())
    })
    .await
    .map_err(|err| {
        SmartModerationError::Other(anyhow!("history storage write task failed: {err}"), None)
    })?
}

#[cfg(not(feature = "storage-debug"))]
async fn save_history_result(
    _text: &str,
    _profile: &ModerationProfile,
    _result: &SmartModerationResult,
) -> Result<(), SmartModerationError> {
    Ok(())
}

fn parse_openai_moderation_response(
    payload: Value,
) -> Result<SmartModerationResult, SmartModerationError> {
    let content = payload
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .map(extract_content_text)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| anyhow!("llm moderation response missing choices[0].message.content"))?;

    parse_moderation_content(&content)
}

fn extract_content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.to_string(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| {
                if part.get("type").and_then(Value::as_str) == Some("text") {
                    part.get("text")
                        .and_then(Value::as_str)
                        .map(ToString::to_string)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// 解析复核返回内容:先严格提取,失败后**就地重试一次**宽松提取。
///
/// 可行性研究(tv-research)实测 glm 复核约 4.7% 的返回解析失败,其中
/// 绝大多数是包裹噪声(markdown 代码块、前后缀说明文字、正文里散落的
/// 花括号)而非模型拒答。宽松提取是对**同一响应**的确定性重解析,不发
/// 起额外的 LLM 调用(复核请求 temperature=0,重发大概率得到同样输出,
/// 只会白白增加延迟)。两条路径都失败才判定为不可解析(LlmUnparseable),
/// 由路由层降级处理。
fn parse_moderation_content(content: &str) -> Result<SmartModerationResult, SmartModerationError> {
    let unparseable = |detail: &str| {
        SmartModerationError::LlmUnparseable(
            anyhow!("llm moderation content is not parseable JSON: {detail}"),
            None,
        )
    };

    // 第一次(严格):整串就是 JSON,或首尾大括号截取(兼容 markdown
    // 代码块包裹——围栏字符不是大括号,首尾截取天然跳过)。
    if let Some(result) = extract_json_object(content).and_then(decode_moderation_json) {
        return Ok(result);
    }

    // 重试一次(宽松):扫描平衡的 {...} 对,取第一个含 "violation" 键的
    // 对象。可容忍:代码块包裹、JSON 前后的说明文字、说明文字里出现
    // 花括号、JSON 之后跟着另一段带花括号的内容等。
    if let Some(result) = extract_balanced_object_with_violation(content)
        .and_then(decode_moderation_json)
    {
        return Ok(result);
    }

    Err(unparseable(&format!(
        "len={} chars, strict and lenient extraction both failed",
        content.chars().count()
    )))
}

/// 把 JSON 对象文本解码为复核结论;解析失败或缺布尔 violation 键返回 None
/// (调用方统一按"这一条提取路径失败"处理)。
fn decode_moderation_json(json_text: &str) -> Option<SmartModerationResult> {
    let data: Value = serde_json::from_str(json_text).ok()?;
    let violation = data.get("violation").and_then(Value::as_bool)?;
    Some(SmartModerationResult {
        violation,
        category: data
            .get("category")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        reason: data
            .get("reason")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        source: "ai".to_string(),
        confidence: data.get("confidence").and_then(Value::as_f64),
        debug: None,
    })
}

/// 扫描内容中所有**平衡**的顶层 `{...}` 对,返回第一个其文本包含
/// `"violation"` 键的对象切片。字符串字面量里的花括号不参与配对。
fn extract_balanced_object_with_violation(content: &str) -> Option<&str> {
    let bytes = content.as_bytes();
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (idx, &byte) in bytes.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => {
                if depth == 0 {
                    start = idx;
                }
                depth += 1;
            }
            b'}' => {
                if depth > 0 {
                    depth -= 1;
                    if depth == 0 {
                        let candidate = &content[start..=idx];
                        if candidate.contains("\"violation\"") {
                            return Some(candidate);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    None
}

fn extract_json_object(content: &str) -> Option<&str> {
    let start = content.find('{')?;
    let end = content.rfind('}')?;
    (end > start).then_some(&content[start..=end])
}

#[derive(Debug, Clone)]
struct ModerationCacheEntry {
    result: SmartModerationResult,
    written_at: Instant,
}

fn check_cache(profile_name: &str, text: &str) -> Option<SmartModerationResult> {
    let cache = MODERATION_CACHE.get_or_init(|| Mutex::new(IndexMap::new()));
    let key = cache_key(text);
    let mut guard = cache.lock().expect("moderation cache");
    let profile_cache = guard.get_mut(profile_name)?;
    let entry = profile_cache.shift_remove(&key)?;
    if entry.written_at.elapsed() > CACHE_TTL {
        return None;
    }
    profile_cache.insert(key, entry.clone());
    let mut result = entry.result;
    result.debug = None;
    Some(result)
}

fn save_cache(profile_name: &str, text: &str, result: &SmartModerationResult) {
    let cache = MODERATION_CACHE.get_or_init(|| Mutex::new(IndexMap::new()));
    let key = cache_key(text);
    let mut guard = cache.lock().expect("moderation cache");
    if !guard.contains_key(profile_name) && guard.len() >= MAX_CACHE_PROFILES {
        guard.shift_remove_index(0);
    }
    let profile_cache = guard
        .entry(profile_name.to_string())
        .or_insert_with(IndexMap::new);
    profile_cache.shift_remove(&key);
    let mut cached = result.clone();
    cached.debug = None;
    profile_cache.insert(
        key,
        ModerationCacheEntry {
            result: cached,
            written_at: Instant::now(),
        },
    );
    while profile_cache.len() > CACHE_SIZE {
        profile_cache.shift_remove_index(0);
    }
}

fn truncate_header_value(value: &str) -> String {
    value.chars().take(160).collect()
}

fn cache_key(text: &str) -> String {
    format!("{:x}", md5::compute(text.as_bytes()))
}

fn next_random_unit() -> f64 {
    #[cfg(test)]
    if let Some(value) = take_test_random_value() {
        return value.clamp(0.0, 1.0 - f64::EPSILON);
    }

    let state = REQUEST_RANDOM_STATE.get_or_init(|| AtomicU64::new(initial_random_seed()));
    let mut current = state.load(Ordering::Relaxed);
    loop {
        let mut next = current;
        next ^= next >> 12;
        next ^= next << 25;
        next ^= next >> 27;
        next = next.wrapping_mul(0x2545_F491_4F6C_DD1D);
        match state.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return ((next >> 11) as f64) / ((1u64 << 53) as f64),
            Err(observed) => current = observed,
        }
    }
}

fn initial_random_seed() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    nanos ^ 0xA076_1D64_78BD_642F
}

fn pick_model_for_attempt(models: &[String], attempted_models: &[String]) -> String {
    let remaining = models
        .iter()
        .filter(|model| !attempted_models.iter().any(|attempted| attempted == *model))
        .map(String::as_str)
        .collect::<Vec<_>>();
    let pool = if remaining.is_empty() {
        models.iter().map(String::as_str).collect::<Vec<_>>()
    } else {
        remaining
    };
    let index = ((next_random_unit() * pool.len() as f64).floor() as usize).min(pool.len() - 1);
    pool[index].to_string()
}

#[cfg(feature = "storage-debug")]
fn history_storage_lock(profile_name: &str) -> Arc<Mutex<()>> {
    let locks = HISTORY_STORAGE_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = locks.lock().expect("history storage locks");
    guard
        .entry(profile_name.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

#[cfg(test)]
fn take_test_random_value() -> Option<f64> {
    let queue = TEST_RANDOM_VALUES.get_or_init(|| Mutex::new(VecDeque::new()));
    queue.lock().expect("test random values").pop_front()
}

#[cfg(test)]
pub fn set_test_random_values(values: &[f64]) {
    let queue = TEST_RANDOM_VALUES.get_or_init(|| Mutex::new(VecDeque::new()));
    let mut guard = queue.lock().expect("test random values");
    guard.clear();
    guard.extend(values.iter().copied());
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;

    use indexmap::IndexMap;

    use crate::profile::{ModerationProfile, ProfileConfig};

    use super::{
        evaluate_local_model, fallback_threshold, pick_model_for_attempt, save_cache,
        set_test_random_values, should_force_ai_review, SmartModerationResult, MODERATION_CACHE,
    };

    fn test_profile() -> ModerationProfile {
        ModerationProfile {
            profile_name: "test".to_string(),
            base_dir: PathBuf::from("/tmp/smart-test-profile"),
            config: ProfileConfig::default(),
        }
    }

    fn sample_result() -> SmartModerationResult {
        SmartModerationResult {
            violation: true,
            category: Some("test".to_string()),
            reason: Some("cached".to_string()),
            source: "ai".to_string(),
            confidence: Some(0.9),
            debug: None,
        }
    }

    fn clear_cache() {
        let cache = MODERATION_CACHE.get_or_init(|| Mutex::new(IndexMap::new()));
        cache.lock().expect("moderation cache").clear();
    }

    /// set_test_random_values 是进程级共享队列:使用它的测试必须互斥,
    /// 否则并行测试会互抢对方的抽样值。
    fn random_state_lock() -> &'static Mutex<()> {
        static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn ai_review_rate_uses_fresh_request_time_random_draws() {
        let _random_guard = random_state_lock().lock().expect("random state lock");
        let mut profile = test_profile();
        profile.config.probability.ai_review_rate = 0.5;

        set_test_random_values(&[0.9, 0.1]);

        assert!(!should_force_ai_review(&profile));
        assert!(should_force_ai_review(&profile));
    }

    #[test]
    fn retry_model_selection_prefers_untried_models_before_repeats() {
        let _random_guard = random_state_lock().lock().expect("random state lock");
        let models = vec![
            "model-a".to_string(),
            "model-b".to_string(),
            "model-c".to_string(),
        ];
        let mut attempted = Vec::new();

        set_test_random_values(&[0.60, 0.40, 0.95, 0.10]);

        let first = pick_model_for_attempt(&models, &attempted);
        attempted.push(first.clone());
        let second = pick_model_for_attempt(&models, &attempted);
        attempted.push(second.clone());
        let third = pick_model_for_attempt(&models, &attempted);
        attempted.push(third.clone());
        let fourth = pick_model_for_attempt(&models, &attempted);

        assert_eq!(first, "model-b");
        assert_eq!(second, "model-a");
        assert_eq!(third, "model-c");
        assert_eq!(fourth, "model-a");
    }

    #[test]
    fn moderation_cache_limits_profile_bucket_count() {
        clear_cache();

        for idx in 0..=50 {
            save_cache(&format!("profile-{idx}"), "same text", &sample_result());
        }

        let cache = MODERATION_CACHE.get_or_init(|| Mutex::new(IndexMap::new()));
        let guard = cache.lock().expect("moderation cache");
        let profiles = guard.keys().cloned().collect::<Vec<_>>();
        assert_eq!(profiles.len(), 50);
        assert!(!profiles.iter().any(|name| name == "profile-0"));
        assert!(profiles.iter().any(|name| name == "profile-50"));

        drop(guard);
        clear_cache();
    }

    #[test]
    fn concurrency_limit_fallback_uses_probability_midpoint() {
        let mut profile = test_profile();
        profile.config.probability.low_risk_threshold = 0.2;
        profile.config.probability.high_risk_threshold = 0.8;

        let threshold = fallback_threshold(&profile);
        assert_eq!(threshold, 0.5);
    }

    #[test]
    fn evaluate_local_model_returns_skipped_debug_when_runtime_is_missing() {
        let profile = test_profile();
        let (result, debug) = evaluate_local_model("anything", &profile);

        assert!(result.is_none());
        assert_eq!(debug.local_model_decision.as_deref(), Some("skipped"));
    }

    #[test]
    fn route_for_score_covers_three_state_boundaries() {
        use super::{route_for_score, ModerationRoute};
        // anomaly 取向:block 线 0.5 > divert 线 0.0
        let block = 0.5_f64;
        let divert = 0.0_f64;

        // score > block 线 → 拦截
        assert_eq!(
            route_for_score(block + 1e-9, block, divert),
            ModerationRoute::Block
        );
        // score == block 线恰好不拦(严格大于),落入存疑档
        assert_eq!(route_for_score(block, block, divert), ModerationRoute::Divert);
        // 存疑档中段
        assert_eq!(route_for_score(0.25, block, divert), ModerationRoute::Divert);
        // score == divert 线恰好不切(严格大于)
        assert_eq!(route_for_score(divert, block, divert), ModerationRoute::Allow);
        assert_eq!(route_for_score(-1.0, block, divert), ModerationRoute::Allow);

        // 阈值配反时由调用方 max() 兜底;纯函数按传入值判定
        assert_eq!(
            route_for_score(0.9, -1.0, -2.0),
            ModerationRoute::Block,
            "score above block line still blocks even if thresholds are inverted"
        );
    }

    #[test]
    fn review_verdict_maps_to_route_like_decision_sim() {
        use super::{route_after_review, ModerationRoute, ReviewVerdict};
        use crate::profile::OneClassConfig;

        let mut cfg = OneClassConfig::default(); // 0.85 / 0.80 / "self"
        let (block, divert) = (0.5, 0.0);

        let v = |violation: bool, conf: f64| {
            if violation {
                ReviewVerdict::Violation { confidence: conf }
            } else {
                ReviewVerdict::Cleared { confidence: conf }
            }
        };

        // violation 置信分档:>=0.85 拦,低于 → 存疑
        assert_eq!(
            route_after_review(&v(true, 0.85), 0.9, block, divert, &cfg),
            ModerationRoute::Block
        );
        assert_eq!(
            route_after_review(&v(true, 0.99), 0.9, block, divert, &cfg),
            ModerationRoute::Block
        );
        assert_eq!(
            route_after_review(&v(true, 0.84), 0.9, block, divert, &cfg),
            ModerationRoute::Divert
        );
        // 违规缺 confidence 按 0 处理 → 存疑
        assert_eq!(
            route_after_review(&v(true, 0.0), 0.9, block, divert, &cfg),
            ModerationRoute::Divert
        );

        // cleared 高置信豁免:即便形态分极异常(>divert 甚至 >block)也走主上游
        assert_eq!(
            route_after_review(&v(false, 0.80), 0.9, block, divert, &cfg),
            ModerationRoute::Allow,
            "cleared & conf==0.80 (not < C_GRAY) must be exempt even with anomaly 0.9"
        );
        assert_eq!(
            route_after_review(&v(false, 0.99), 0.6, block, divert, &cfg),
            ModerationRoute::Allow
        );
        // cleared 低置信 + 形态异常 → 存疑;形态不异常 → 主上游
        assert_eq!(
            route_after_review(&v(false, 0.79), 0.9, block, divert, &cfg),
            ModerationRoute::Divert
        );
        assert_eq!(
            route_after_review(&v(false, 0.79), 0.0, block, divert, &cfg),
            ModerationRoute::Allow,
            "cleared low-conf but anomaly == divert line must not divert"
        );

        // Unparseable 策略
        assert_eq!(
            route_after_review(&ReviewVerdict::Unparseable, 0.9, block, divert, &cfg),
            ModerationRoute::Allow,
            "default policy self must fail-open to primary"
        );
        cfg.review_unparseable_policy = "divert".to_string();
        assert_eq!(
            route_after_review(&ReviewVerdict::Unparseable, 0.1, block, divert, &cfg),
            ModerationRoute::Divert
        );
        cfg.review_unparseable_policy = "block".to_string();
        assert_eq!(
            route_after_review(&ReviewVerdict::Unparseable, 0.1, block, divert, &cfg),
            ModerationRoute::Block
        );
        cfg.review_unparseable_policy = "fallthrough".to_string();
        assert_eq!(
            route_after_review(&ReviewVerdict::Unparseable, 0.25, block, divert, &cfg),
            ModerationRoute::Divert,
            "fallthrough must defer to score-based routing"
        );
        assert_eq!(
            route_after_review(&ReviewVerdict::Unparseable, -0.5, block, divert, &cfg),
            ModerationRoute::Allow
        );
    }

    fn review_sampling_forced_prefixes_and_base_rate() {
        let _random_guard = random_state_lock().lock().expect("random state lock");
        use super::should_review_model;
        use crate::profile::OneClassConfig;

        let mut cfg = OneClassConfig::default();
        cfg.review_base_rate = 0.0;
        cfg.review_forced_model_prefixes = vec!["Claude-Opus".to_string(), "grok".to_string()];

        // 高危前缀强制,大小写不敏感
        assert!(should_review_model("claude-opus-4-5", &cfg));
        assert!(should_review_model("GROK-4-FAST", &cfg));
        // 非高危 + 基础采样率 0 → 不采样
        assert!(!should_review_model("gpt-4.1-mini", &cfg));

        // 基础采样率生效:抽签值低于阈值才采样
        cfg.review_forced_model_prefixes.clear();
        cfg.review_base_rate = 0.5;
        set_test_random_values(&[0.4, 0.6]);
        assert!(should_review_model("gpt-4.1-mini", &cfg));
        assert!(!should_review_model("gpt-4.1-mini", &cfg));

        // 采样率 >= 1 全量
        cfg.review_base_rate = 1.0;
        assert!(should_review_model("any-model", &cfg));
    }

    #[test]
    fn parse_moderation_content_strict_path_handles_bare_and_wrapped_json() {
        use super::parse_moderation_content;

        // 裸 JSON
        let bare = parse_moderation_content("{\"violation\": true, \"reason\": \"r\"}")
            .expect("bare json parses");
        assert!(bare.violation);

        // markdown 代码块包裹(首尾大括号截取天然兼容)
        let fenced =
            parse_moderation_content("```json\n{\"violation\": false, \"reason\": \"ok\"}\n```")
                .expect("fenced json parses");
        assert!(!fenced.violation);
    }

    #[test]
    fn parse_moderation_content_lenient_retry_rescues_noisy_wrappers() {
        use super::parse_moderation_content;

        // 前缀说明文字带花括号 + 代码块包裹:严格首尾截取会跨到说明文字的
        // 花括号而失败;宽松提取(平衡对象扫描)应救回。
        let noisy_prefix = "分析如下{请注意}:\n```json\n{\"violation\": false, \"reason\": \"clean\"}\n```\n完毕";
        let parsed = parse_moderation_content(noisy_prefix)
            .expect("lenient retry must rescue noisy wrapper");
        assert!(!parsed.violation);
        assert_eq!(parsed.reason.as_deref(), Some("clean"));

        // JSON 之后跟着带花括号的备注
        let noisy_suffix = "{\"violation\": true, \"category\": \"jailbreak\"}\n备注:{略}";
        let parsed = parse_moderation_content(noisy_suffix)
            .expect("lenient retry must rescue trailing braces");
        assert!(parsed.violation);
        assert_eq!(parsed.category.as_deref(), Some("jailbreak"));

        // 字符串字面量里的花括号不得破坏配对
        let braces_in_string = "废话{a}\n{\"violation\": false, \"reason\": \"包含}花括号{测试\"}";
        let parsed = parse_moderation_content(braces_in_string)
            .expect("braces inside string literals must not break pairing");
        assert!(!parsed.violation);

        // 多个平衡对象:取第一个含 violation 键的
        let multi = "前置{\"foo\": 1}后置{\"violation\": true}";
        let parsed = parse_moderation_content(multi)
            .expect("must pick the object containing the violation key");
        assert!(parsed.violation);
    }

    #[test]
    fn parse_moderation_content_reports_unparseable_after_both_attempts() {
        use super::{parse_moderation_content, SmartModerationError};

        // 纯垃圾文本:两条提取路径都失败 → LlmUnparseable(供路由层降级存疑)
        for garbage in [
            "GARBAGE-VERDICT not-json-text without braces +++",
            "",
            "{\"foo\": 1}",                       // 有 JSON 但缺 violation 布尔键
            "{\"violation\": \"yes\"}",           // violation 非布尔
            "只有前半个 { 括号",
        ] {
            match parse_moderation_content(garbage) {
                Err(SmartModerationError::LlmUnparseable(..)) => {}
                other => panic!("expected LlmUnparseable for {garbage:?}, got {other:?}"),
            }
        }
    }
}
