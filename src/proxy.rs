use std::collections::VecDeque;
use std::io::Read;
use std::pin::Pin;

use anyhow::Context;
use axum::body::{boxed, Body, BoxBody};
use axum::extract::{Path, State};
use axum::http::header::{ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH, HOST};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use encoding_rs::Encoding;
use futures_util::stream::{self, Stream, StreamExt};
use hyper::body::to_bytes;
use serde_json::json;
use serde_json::Value;

use crate::format::{process_request, RequestProcessError};
use crate::moderation::smart::ModerationDebugInfo;
use crate::moderation::{basic, extract, smart};
use crate::response::maybe_transform_json_response;
use crate::routes::{parse_url_config, ApiError, AppState};
use crate::streaming::StreamTranscoder;

const HEADER_DENYLIST: &[&str] = &[
    "content-length",
    "transfer-encoding",
    "content-encoding",
    "set-cookie",
    "strict-transport-security",
    "content-security-policy",
    "content-security-policy-report-only",
    "x-frame-options",
    "x-content-type-options",
    "x-xss-protection",
    "permissions-policy",
    "referrer-policy",
];

const MIN_VALID_RESPONSE_CHARS: usize = 1;

/// 双上游路由配置(来自 ENV_KEY 渠道配置 JSON 的 "router" 段)。
/// 影子模式默认开启:灰度期只加响应头/日志,不真正切流。
#[derive(Debug, Clone)]
struct RouterConfig {
    /// 可选:覆盖 URL 尾部编码的主上游。
    primary_upstream: Option<String>,
    /// 存疑(中置信)流量走次级上游;未配置则存疑也走主上游。
    secondary_upstream: Option<String>,
    /// 影子模式:存疑只打标(x-prismguard-route: shadow-secondary),实际仍走主上游。
    shadow_mode: bool,
    /// 次级上游失败(传输错误/5xx)时回退主上游(可用性优先)。
    fallback_to_primary: bool,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            primary_upstream: None,
            secondary_upstream: None,
            shadow_mode: true,
            fallback_to_primary: true,
        }
    }
}

impl RouterConfig {
    fn from_config_value(config: &Value) -> Self {
        let Some(router) = config.get("router").and_then(Value::as_object) else {
            return Self::default();
        };
        let defaults = Self::default();
        Self {
            primary_upstream: router
                .get("primary_upstream")
                .and_then(Value::as_str)
                .map(str::to_string),
            secondary_upstream: router
                .get("secondary_upstream")
                .and_then(Value::as_str)
                .map(str::to_string),
            shadow_mode: router
                .get("shadow_mode")
                .and_then(Value::as_bool)
                .unwrap_or(defaults.shadow_mode),
            fallback_to_primary: router
                .get("fallback_to_primary")
                .and_then(Value::as_bool)
                .unwrap_or(defaults.fallback_to_primary),
        }
    }
}

/// 上游目标:base(scheme://host[:port]) + 路径前缀 + query。
#[derive(Debug, Clone)]
struct UpstreamTarget {
    base: String,
    path: String,
    query: Option<String>,
}

fn parse_upstream_target(raw: &str) -> Result<UpstreamTarget, ApiError> {
    let upstream = reqwest::Url::parse(raw).map_err(|e| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("invalid upstream url: {e}"),
        )
        .with_code("CONFIG_PARSE_ERROR")
        .with_error_type("config_error")
    })?;
    let host = upstream.host_str().ok_or_else(|| {
        ApiError::new(StatusCode::BAD_REQUEST, "upstream host is missing")
            .with_code("CONFIG_PARSE_ERROR")
            .with_error_type("config_error")
    })?;
    let base = match upstream.port() {
        Some(port) => format!("{}://{}:{}", upstream.scheme(), host, port),
        None => format!("{}://{}", upstream.scheme(), host),
    };
    Ok(UpstreamTarget {
        base,
        path: if upstream.path().is_empty() {
            "/".to_string()
        } else {
            upstream.path().to_string()
        },
        query: upstream.query().map(str::to_string),
    })
}

/// 路由结果标签(响应头 x-prismguard-route 的值,也进日志)。
const ROUTE_HEADER: &str = "x-prismguard-route";
const ROUTE_BLOCKED: &str = "blocked";
const ROUTE_PRIMARY: &str = "primary";
const ROUTE_SECONDARY: &str = "secondary";
const ROUTE_SHADOW_SECONDARY: &str = "shadow-secondary";
const ROUTE_SECONDARY_FALLBACK: &str = "secondary-fallback-primary";

fn route_header(label: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(label) {
        headers.insert(HeaderName::from_static(ROUTE_HEADER), value);
    }
    headers
}

pub async fn proxy_entry(
    State(state): State<AppState>,
    Path(cfg_and_upstream): Path<String>,
    request: Request<Body>,
) -> Result<Response, ApiError> {
    proxy_entry_with_cfg(state, cfg_and_upstream, request).await
}

pub async fn proxy_entry_root(
    State(state): State<AppState>,
    request: Request<Body>,
) -> Result<Response, ApiError> {
    proxy_entry_with_cfg(state, String::new(), request).await
}

async fn proxy_entry_with_cfg(
    state: AppState,
    cfg_and_upstream: String,
    request: Request<Body>,
) -> Result<Response, ApiError> {
    let parsed = parse_url_config(&state.settings, &cfg_and_upstream)?;
    let router_cfg = RouterConfig::from_config_value(&parsed.config);

    // 主上游:router.primary_upstream(若配置)优先,否则 URL 尾部编码的上游
    let primary_target = parse_upstream_target(
        router_cfg
            .primary_upstream
            .as_deref()
            .unwrap_or(&parsed.upstream),
    )?;
    let upstream_base = primary_target.base.clone();
    let path = primary_target.path.clone();
    let query = primary_target.query.clone();

    let (parts, body) = request.into_parts();
    let raw_body = to_bytes(body).await.map_err(|e| {
        ApiError::new(StatusCode::BAD_REQUEST, format!("failed to read body: {e}"))
            .with_code("PROXY_ERROR")
    })?;
    let (request_json, decoded_body) =
        parse_request_json(&parts.method, &parts.headers, &raw_body)?;
    let source_format_hint = detect_source_format(&path, request_json.as_ref()).map(str::to_string);
    let mut request_body = request_json
        .clone()
        .unwrap_or_else(|| Value::Object(Default::default()));
    let header_pairs = parts
        .headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), value.to_string()))
        })
        .collect::<Vec<_>>();
    let request_plan = process_request(&parsed.config, &path, &header_pairs, request_body.clone())
        .map_err(|error| match error {
            RequestProcessError::StrictParse(message) | RequestProcessError::Transform(message) => {
                let moderation_details = moderation_details_for_message(&message);
                ApiError::moderation_blocked(
                    message,
                    source_format_hint.clone(),
                    moderation_details,
                )
            }
        })?;
    let estimated_prompt_tokens = estimate_prompt_tokens_for_stream(
        request_plan.target_format.or(request_plan.source_format),
        &request_plan.body,
    );
    let basic_mod_cfg = parsed
        .config
        .get("basic_moderation")
        .cloned()
        .unwrap_or(Value::Null);
    let basic_mod_enabled = basic_mod_cfg
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let smart_mod_cfg = parsed
        .config
        .get("smart_moderation")
        .cloned()
        .unwrap_or(Value::Null);
    let smart_mod_enabled = smart_mod_cfg
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut moderation_debug = HeaderMap::new();
    let moderation_context = if basic_mod_enabled || smart_mod_enabled {
        let moderation_format = request_plan
            .source_format
            .map(|format| format.as_str())
            .or_else(|| detect_source_format(&path, request_json.as_ref()))
            .unwrap_or("openai_chat")
            .to_string();
        let moderation_text = request_plan.moderation_text.clone().unwrap_or_else(|| {
            extract::extract_text_for_moderation(&request_plan.body, moderation_format.as_str())
        });
        Some((moderation_format, moderation_text))
    } else {
        None
    };
    // 词库命中不再必然硬拦:keyword_review=true 且 smart 层可用时,降级为
    // "必审信号"——强制送 glm 复核,由语义判定拦截/引流。背景:agent 客户端的
    // system prompt 会描述攻击特征(如自带防注入规则说明),子串匹配分不清
    // "引用"与"使用",2026-10-13 隔日校验实测 pi/OpenClaw 客户端被误伤。
    let mut keyword_hit = false;
    if basic_mod_enabled {
        let (_moderation_format, moderation_text) = moderation_context
            .as_ref()
            .expect("moderation context when basic moderation enabled");
        if let Some(blocked) =
            basic::basic_moderation(moderation_text, &basic_mod_cfg).map_err(ApiError::from)?
        {
            let soft_review = basic_mod_cfg
                .get("keyword_review")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                && smart_mod_enabled;
            if soft_review {
                keyword_hit = true;
                tracing::info!(
                    "keyword hit routed to review (keyword_review=true): {}",
                    blocked.reason
                );
            } else {
                return Err(ApiError::moderation_blocked(
                    blocked.reason,
                    Some(_moderation_format.as_str()),
                    None,
                )
                .with_extra_headers(route_header(ROUTE_BLOCKED)));
            }
        }
    }
    // 三态路由决策(oneclass 可用时接管 smart 层;不可用降级为现状决策链)
    let mut route_label: &'static str = ROUTE_PRIMARY;
    let mut divert_to_secondary = false;
    if smart_mod_enabled {
        let (moderation_format, moderation_text) = moderation_context
            .as_ref()
            .expect("moderation context when smart moderation enabled");
        let model_name = request_plan
            .body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let routed = smart::route_moderation(
            moderation_text,
            &model_name,
            &smart_mod_cfg,
            &state.settings.root_dir,
            &state.http_client,
            &state.settings.env_map,
            keyword_hit,
        )
        .await;
        match routed {
            Ok(Some(decision)) => {
                moderation_debug = moderation_debug_headers(Some(&decision.debug));
                match decision.route {
                    smart::ModerationRoute::Block => {
                        // 分数拦截:详情进服务端日志,对外统一文案
                        tracing::info!(
                            route = smart::ModerationRoute::Block.as_str(),
                            model = %decision.model_name,
                            score = decision.score,
                            raw_anomaly = decision.raw_anomaly,
                            baseline_group = %decision.baseline_group,
                            llm_reviewed = decision.llm_reviewed,
                            "commercial router blocked request"
                        );
                        let mut headers = moderation_debug.clone();
                        append_extra_headers(&mut headers, &route_header(ROUTE_BLOCKED));
                        return Err(ApiError::moderation_blocked(
                            crate::moderation::MODERATION_BLOCKED_MESSAGE,
                            Some(moderation_format.as_str()),
                            None,
                        )
                        .with_extra_headers(headers));
                    }
                    smart::ModerationRoute::Divert => {
                        let secondary_ready = router_cfg
                            .secondary_upstream
                            .as_deref()
                            .and_then(|raw| parse_upstream_target(raw).ok())
                            .is_some();
                        if !secondary_ready {
                            // 未配置(或非法)次级上游:可用性优先,走主上游
                            route_label = ROUTE_PRIMARY;
                        } else if router_cfg.shadow_mode {
                            // 影子模式:只打标,实际仍走主上游(灰度安全机制)
                            route_label = ROUTE_SHADOW_SECONDARY;
                        } else {
                            route_label = ROUTE_SECONDARY;
                            divert_to_secondary = true;
                        }
                    }
                    smart::ModerationRoute::Allow => {
                        route_label = ROUTE_PRIMARY;
                    }
                }
            }
            // oneclass 不可用 → 降级为现状决策链
            Ok(None) => {
                match smart::smart_moderation(
                    moderation_text,
                    &smart_mod_cfg,
                    &state.settings.root_dir,
                    &state.http_client,
                    &state.settings.env_map,
                )
                .await
                {
                    Ok(Some(result)) if result.violation => {
                        moderation_debug = moderation_debug_headers(result.debug.as_ref());
                        // 响应体不带 moderation_details(信息最小化);详情已由
                        // smart_moderation_error_message 记入服务端日志
                        let mut headers = moderation_debug.clone();
                        append_extra_headers(&mut headers, &route_header(ROUTE_BLOCKED));
                        return Err(ApiError::moderation_blocked(
                            smart_moderation_error_message(&result),
                            Some(moderation_format.as_str()),
                            None,
                        )
                        .with_extra_headers(headers));
                    }
                    Ok(Some(result)) => {
                        moderation_debug = moderation_debug_headers(result.debug.as_ref());
                    }
                    Ok(None) => {}
                    Err(smart::SmartModerationError::ConcurrencyLimit(message)) => {
                        let mut debug = HeaderMap::new();
                        debug.insert(
                            HeaderName::from_static("x-prismguard-llm-reviewed"),
                            HeaderValue::from_static("true"),
                        );
                        debug.insert(
                            HeaderName::from_static("x-prismguard-llm-result"),
                            HeaderValue::from_static("error"),
                        );
                        append_extra_headers(&mut debug, &route_header(ROUTE_BLOCKED));
                        return Err(ApiError::moderation_blocked(
                            message,
                            Some(moderation_format.as_str()),
                            Some(json!({
                                "source": "concurrency_limit"
                            })),
                        )
                        .with_extra_headers(debug));
                    }
                    Err(smart::SmartModerationError::Other(err, debug)) => {
                        return Err(ApiError::from(err)
                            .with_extra_headers(moderation_debug_headers(debug.as_ref())));
                    }
                    // 现状(legacy)链没有次级上游可引流,解析失败维持既有
                    // 行为:向上抛错(5xx)。存疑降级只属于三态路由链。
                    Err(smart::SmartModerationError::LlmUnparseable(err, debug)) => {
                        return Err(ApiError::from(err)
                            .with_extra_headers(moderation_debug_headers(debug.as_ref())));
                    }
                }
            }
            Err(err) => return Err(map_route_error(err, moderation_format.as_str())),
        }
    }
    let is_stream = request_plan.stream;
    let primary_url = build_upstream_url(
        &upstream_base,
        &request_plan.path,
        query.as_deref(),
        request_plan.target_format,
        is_stream,
    );
    let secondary_target = if divert_to_secondary {
        router_cfg
            .secondary_upstream
            .as_deref()
            .and_then(|raw| parse_upstream_target(raw).ok())
    } else {
        None
    };
    let secondary_url = secondary_target.as_ref().map(|target| {
        build_upstream_url(
            &target.base,
            &request_plan.path,
            target.query.as_deref().or(query.as_deref()),
            request_plan.target_format,
            is_stream,
        )
    });
    let final_url = secondary_url.clone().unwrap_or(primary_url.clone());
    request_body = request_plan.body;
    let forward_planned_json = should_forward_planned_json(&parts.method);
    let passthrough_raw_body = if request_plan.passthrough {
        decoded_body
    } else {
        None
    };

    // 预构建请求体字节,次级失败回退主上游时需要重放
    let upstream_body: Option<Vec<u8>> = if let Some(passthrough_raw_body) = passthrough_raw_body {
        Some(passthrough_raw_body)
    } else if forward_planned_json {
        Some(serde_json::to_vec(&request_body).map_err(|e| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("failed to encode transformed body: {e}"),
            )
            .with_code("PROXY_ERROR")
        })?)
    } else if !raw_body.is_empty() {
        Some(raw_body.to_vec())
    } else {
        None
    };

    let send_upstream = |url: String, body: Option<Vec<u8>>| {
        let mut upstream_request = state
            .http_client
            .request(
                parts.method.as_str().parse().expect("valid request method"),
                url,
            )
            .headers(filtered_request_headers(&parts.headers, forward_planned_json));
        if let Some(body) = body {
            upstream_request = upstream_request.body(body);
        }
        upstream_request.send()
    };

    let mut upstream_response = {
        match send_upstream(final_url, upstream_body.clone()).await {
            Ok(response) => response,
            Err(err) => {
                if divert_to_secondary && router_cfg.fallback_to_primary {
                    // 次级上游传输失败:回退主上游(可用性优先,风险由采样审计兜底)
                    tracing::warn!(
                        error = %err,
                        "secondary upstream failed; falling back to primary"
                    );
                    route_label = ROUTE_SECONDARY_FALLBACK;
                    send_upstream(primary_url.clone(), upstream_body.clone()).await.map_err(|e| {
                        ApiError::new(
                            StatusCode::BAD_GATEWAY,
                            format!("upstream request failed: {e}"),
                        )
                        .with_code("PROXY_ERROR")
                        .with_error_type("proxy_error")
                    })?
                } else {
                    return Err(ApiError::new(
                        StatusCode::BAD_GATEWAY,
                        format!("upstream request failed: {err}"),
                    )
                    .with_code("PROXY_ERROR")
                    .with_error_type("proxy_error"));
                }
            }
        }
    };

    // 次级上游返回 5xx 同样视为失败:回退主上游重放
    if divert_to_secondary
        && router_cfg.fallback_to_primary
        && upstream_response.status().is_server_error()
    {
        tracing::warn!(
            status = upstream_response.status().as_u16(),
            "secondary upstream returned 5xx; falling back to primary"
        );
        route_label = ROUTE_SECONDARY_FALLBACK;
        upstream_response = send_upstream(primary_url.clone(), upstream_body.clone())
            .await
            .map_err(|e| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                format!("upstream request failed: {e}"),
            )
            .with_code("PROXY_ERROR")
            .with_error_type("proxy_error")
        })?;
    }

    tracing::info!(
        method = %parts.method,
        upstream_base,
        upstream_path = %request_plan.path,
        stream = is_stream,
        src_format = request_plan.source_format.map(|f| f.as_str()).unwrap_or("unknown"),
        target_format = request_plan.target_format.map(|f| f.as_str()).unwrap_or("none"),
        status = upstream_response.status().as_u16(),
        route = route_label,
        config_source = %parsed.source,
        "proxied request"
    );

    let delay_stream_header = parsed
        .config
        .get("format_transform")
        .and_then(Value::as_object)
        .and_then(|cfg| cfg.get("delay_stream_header"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // 路由标签头随响应返回(含影子模式标记),便于灰度观测
    let mut response_extra_headers = moderation_debug;
    append_extra_headers(&mut response_extra_headers, &route_header(route_label));

    build_proxy_response(
        upstream_response,
        request_plan.source_format,
        request_plan.target_format,
        request_plan.passthrough,
        delay_stream_header,
        is_stream,
        estimated_prompt_tokens,
        &response_extra_headers,
    )
    .await
}

/// 三态决策链的错误映射(复核失败已在决策层吞掉,这里只剩配置类错误)。
fn map_route_error(err: smart::SmartModerationError, _source_format: &str) -> ApiError {
    match err {
        smart::SmartModerationError::ConcurrencyLimit(message) => ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            message,
        )
        .with_code("MODERATION_RATE_LIMITED")
        .with_error_type("moderation_error"),
        // LlmUnparseable 在 route_moderation 内已降级为存疑,不会走到这里;
        // 防御性映射为与其他错误一致的 5xx。
        smart::SmartModerationError::LlmUnparseable(error, _debug) => ApiError::from(error),
        smart::SmartModerationError::Other(error, _debug) => ApiError::from(error),
    }
}

fn parse_request_json(
    method: &Method,
    headers: &HeaderMap,
    raw_body: &[u8],
) -> Result<(Option<Value>, Option<Vec<u8>>), ApiError> {
    if raw_body.is_empty() {
        return Ok((None, None));
    }

    if !matches!(*method, Method::POST | Method::PUT) {
        return Ok((None, None));
    }

    match decompress_body(headers, raw_body) {
        Ok(decoded) => match serde_json::from_slice::<Value>(&decoded) {
            Ok(value) => Ok((Some(value), Some(decoded))),
            Err(_) => Ok((None, None)),
        },
        Err(_) => Ok((None, None)),
    }
}

fn decompress_body(headers: &HeaderMap, raw_body: &[u8]) -> Result<Vec<u8>, ApiError> {
    let encoding = headers
        .get(CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

    match encoding.as_str() {
        "" | "identity" => Ok(raw_body.to_vec()),
        "gzip" => {
            let mut decoder = flate2::read::GzDecoder::new(raw_body);
            let mut out = Vec::new();
            std::io::Read::read_to_end(&mut decoder, &mut out).map_err(|e| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!("failed to decode gzip request body: {e}"),
                )
            })?;
            Ok(out)
        }
        "deflate" => {
            let mut decoder = flate2::read::ZlibDecoder::new(raw_body);
            let mut out = Vec::new();
            std::io::Read::read_to_end(&mut decoder, &mut out).map_err(|e| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!("failed to decode deflate request body: {e}"),
                )
            })?;
            Ok(out)
        }
        "br" => {
            let mut decoder = brotli::Decompressor::new(raw_body, 4096);
            let mut out = Vec::new();
            decoder.read_to_end(&mut out).map_err(|e| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!("failed to decode brotli request body: {e}"),
                )
            })?;
            Ok(out)
        }
        other => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("unsupported content-encoding: {other}"),
        )),
    }
}

fn filtered_request_headers(
    headers: &HeaderMap,
    strip_content_encoding: bool,
) -> reqwest::header::HeaderMap {
    let mut out = reqwest::header::HeaderMap::new();
    for (name, value) in headers.iter() {
        if name == HOST || name == CONTENT_LENGTH || name == ACCEPT_ENCODING {
            continue;
        }
        if strip_content_encoding && name == CONTENT_ENCODING {
            continue;
        }
        if let (Some(name), Some(value)) = (
            reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes()).ok(),
            reqwest::header::HeaderValue::from_bytes(value.as_bytes()).ok(),
        ) {
            out.append(name, value);
        }
    }
    out.insert(
        reqwest::header::ACCEPT_ENCODING,
        reqwest::header::HeaderValue::from_static("identity"),
    );
    out
}

fn append_extra_headers(target: &mut HeaderMap, extra: &HeaderMap) {
    for (name, value) in extra.iter() {
        target.append(name.clone(), value.clone());
    }
}

fn format_optional_f64(value: Option<f64>) -> Option<HeaderValue> {
    value
        .map(|value| format!("{value:.3}"))
        .and_then(|value| HeaderValue::from_str(&value).ok())
}

fn moderation_debug_headers(debug: Option<&ModerationDebugInfo>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let Some(debug) = debug else {
        return headers;
    };

    if let Some(value) = debug
        .local_model_source
        .as_deref()
        .and_then(|v| HeaderValue::from_str(v).ok())
    {
        headers.insert(
            HeaderName::from_static("x-prismguard-local-model-source"),
            value,
        );
    }
    if let Some(value) = debug
        .local_model_decision
        .as_deref()
        .and_then(|v| HeaderValue::from_str(v).ok())
    {
        headers.insert(
            HeaderName::from_static("x-prismguard-local-model-decision"),
            value,
        );
    }
    if let Some(value) = format_optional_f64(debug.local_model_confidence) {
        headers.insert(
            HeaderName::from_static("x-prismguard-local-model-confidence"),
            value,
        );
    }
    if let Some(value) = format_optional_f64(debug.local_model_latency_ms) {
        headers.insert(
            HeaderName::from_static("x-prismguard-local-model-latency-ms"),
            value,
        );
    }
    headers.insert(
        HeaderName::from_static("x-prismguard-llm-reviewed"),
        HeaderValue::from_static(if debug.llm_reviewed { "true" } else { "false" }),
    );
    if let Some(value) = debug
        .llm_result
        .as_deref()
        .and_then(|v| HeaderValue::from_str(v).ok())
    {
        headers.insert(HeaderName::from_static("x-prismguard-llm-result"), value);
    }
    if let Some(value) = format_optional_f64(debug.llm_latency_ms) {
        headers.insert(
            HeaderName::from_static("x-prismguard-llm-latency-ms"),
            value,
        );
    }
    if let Some(value) = debug
        .llm_error
        .as_deref()
        .and_then(|v| HeaderValue::from_str(v).ok())
    {
        headers.insert(HeaderName::from_static("x-prismguard-llm-error"), value);
    }
    headers
}

fn should_forward_planned_json(method: &Method) -> bool {
    matches!(
        *method,
        Method::GET | Method::POST | Method::PUT | Method::DELETE
    )
}

fn build_upstream_url(
    upstream_base: &str,
    path: &str,
    query: Option<&str>,
    target_format: Option<crate::format::RequestFormat>,
    is_stream: bool,
) -> String {
    let mut url = format!("{upstream_base}{path}");
    if let Some(query) = query.filter(|query| !query.is_empty()) {
        let separator = if url.contains('?') { '&' } else { '?' };
        url.push(separator);
        url.push_str(query);
    }
    if target_format == Some(crate::format::RequestFormat::GeminiChat)
        && is_stream
        && path.contains("streamGenerateContent")
        && !url.contains("alt=sse")
    {
        let separator = if url.contains('?') { '&' } else { '?' };
        url.push(separator);
        url.push_str("alt=sse");
    }
    url
}

async fn build_proxy_response(
    upstream_response: reqwest::Response,
    client_format: Option<crate::format::RequestFormat>,
    upstream_format: Option<crate::format::RequestFormat>,
    passthrough: bool,
    delay_stream_header: bool,
    request_expects_stream: bool,
    estimated_prompt_tokens: Option<i64>,
    moderation_debug: &HeaderMap,
) -> Result<Response, ApiError> {
    let headers = filtered_response_headers(upstream_response.headers());
    let header_says_stream = is_stream_response(upstream_response.headers());
    if header_says_stream || request_expects_stream {
        return build_streaming_proxy_response(
            upstream_response,
            client_format,
            upstream_format,
            passthrough,
            delay_stream_header,
            header_says_stream,
            estimated_prompt_tokens,
            &headers,
            moderation_debug,
        )
        .await;
    }

    build_non_stream_proxy_response(
        upstream_response,
        client_format,
        upstream_format,
        passthrough,
        delay_stream_header,
        &headers,
        moderation_debug,
    )
    .await
}

type ReqByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

async fn build_streaming_proxy_response(
    upstream_response: reqwest::Response,
    client_format: Option<crate::format::RequestFormat>,
    upstream_format: Option<crate::format::RequestFormat>,
    passthrough: bool,
    delay_stream_header: bool,
    header_says_stream: bool,
    estimated_prompt_tokens: Option<i64>,
    headers: &HeaderMap,
    moderation_debug: &HeaderMap,
) -> Result<Response, ApiError> {
    let status = StatusCode::from_u16(upstream_response.status().as_u16())
        .expect("valid upstream status code");
    let upstream_content_type = upstream_response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let mut upstream = Box::pin(upstream_response.bytes_stream()) as ReqByteStream;
    let mut buffered = Vec::new();
    let mut stream_detected = header_says_stream;
    let mut precheck = if delay_stream_header && status == StatusCode::OK {
        Some(StreamPrecheck::new(stream_precheck_format(
            client_format,
            upstream_format,
        )))
    } else {
        None
    };

    if !stream_detected || precheck.is_some() {
        loop {
            match upstream.as_mut().next().await {
                Some(Ok(chunk)) => {
                    if !stream_detected {
                        buffered.push(chunk.clone());
                        stream_detected = looks_like_sse_prefix(&buffered);
                        if !stream_detected && looks_like_json_prefix(&buffered) {
                            break;
                        }
                    } else {
                        buffered.push(chunk.clone());
                    }

                    if let Some(checker) = precheck.as_mut() {
                        match checker.push_chunk(&chunk) {
                            StreamPrecheckStatus::Pending => continue,
                            StreamPrecheckStatus::Passed
                            | StreamPrecheckStatus::ForcePassProtectionLimit => break,
                        }
                    } else if stream_detected {
                        break;
                    }
                }
                Some(Err(err)) => {
                    return Err(ApiError::new(
                        StatusCode::BAD_GATEWAY,
                        format!("failed to read upstream stream: {err}"),
                    )
                    .with_code("PROXY_ERROR")
                    .with_error_type("proxy_error"));
                }
                None => {
                    if let Some(checker) = precheck.as_mut() {
                        if let Some(message) = checker.finish() {
                            return Err(ApiError::new(StatusCode::BAD_GATEWAY, message)
                                .with_code("UPSTREAM_STREAM_ERROR")
                                .with_error_type("upstream_stream_error"));
                        }
                    }
                    break;
                }
            }
        }
    }

    if !stream_detected {
        let body = collect_stream_bytes(buffered, upstream).await?;
        return build_non_stream_proxy_response_from_body(
            status,
            upstream_content_type.as_deref(),
            headers,
            body,
            client_format,
            upstream_format,
            passthrough,
            delay_stream_header,
            moderation_debug,
        );
    }

    let response_headers = if passthrough {
        passthrough_success_headers(headers, moderation_debug)
    } else {
        stream_success_headers(headers, moderation_debug)
    };
    if status != StatusCode::OK {
        return build_response(
            status,
            &response_headers,
            build_passthrough_stream_body(buffered, upstream),
        );
    }

    let body = if !passthrough
        && upstream_format
            .zip(client_format)
            .is_some_and(|(from, to)| from != to)
    {
        build_transformed_stream_body(
            buffered,
            upstream,
            upstream_format.expect("upstream format for transformed stream"),
            client_format.expect("client format for transformed stream"),
            estimated_prompt_tokens,
        )
    } else {
        build_passthrough_stream_body(buffered, upstream)
    };

    build_response(status, &response_headers, body)
}

async fn build_non_stream_proxy_response(
    upstream_response: reqwest::Response,
    client_format: Option<crate::format::RequestFormat>,
    upstream_format: Option<crate::format::RequestFormat>,
    passthrough: bool,
    delay_stream_header: bool,
    headers: &HeaderMap,
    moderation_debug: &HeaderMap,
) -> Result<Response, ApiError> {
    let status = StatusCode::from_u16(upstream_response.status().as_u16())
        .expect("valid upstream status code");
    let upstream_content_type = upstream_response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let body = upstream_response
        .bytes()
        .await
        .context("failed to read upstream body")
        .map_err(ApiError::from)?
        .to_vec();

    build_non_stream_proxy_response_from_body(
        status,
        upstream_content_type.as_deref(),
        headers,
        body,
        client_format,
        upstream_format,
        passthrough,
        delay_stream_header,
        moderation_debug,
    )
}

fn build_non_stream_proxy_response_from_body(
    status: StatusCode,
    upstream_content_type: Option<&str>,
    headers: &HeaderMap,
    body: Vec<u8>,
    client_format: Option<crate::format::RequestFormat>,
    upstream_format: Option<crate::format::RequestFormat>,
    passthrough: bool,
    delay_stream_header: bool,
    moderation_debug: &HeaderMap,
) -> Result<Response, ApiError> {
    if status == StatusCode::OK {
        if passthrough {
            return build_response(
                status,
                &passthrough_success_headers(headers, moderation_debug),
                boxed(Body::from(body)),
            );
        }
        if let Ok(json_body) = serde_json::from_slice::<Value>(&body) {
            let transformed = match maybe_transform_json_response(
                json_body.clone(),
                upstream_format,
                client_format,
            ) {
                Ok(transformed) => transformed,
                Err(_) => json_body,
            };
            if delay_stream_header {
                let check_format = stream_check_format(client_format, upstream_format);
                if let Some(message) = validate_response_content(&transformed, Some(check_format)) {
                    return Err(ApiError::new(StatusCode::BAD_REQUEST, message)
                        .with_code("EMPTY_RESPONSE")
                        .with_error_type("content_validation_error"));
                }
            }
            let encoded = serde_json::to_vec(&transformed).map_err(|e| {
                ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    format!("failed to encode transformed response body: {e}"),
                )
                .with_code("PROXY_ERROR")
            })?;
            return build_response(
                status,
                &json_success_headers(moderation_debug),
                boxed(Body::from(encoded)),
            );
        }
        let wrapped = serde_json::json!({
            "text": decode_response_text(&body, upstream_content_type),
            "status_code": status.as_u16(),
        });
        if delay_stream_header {
            let check_format = stream_check_format(client_format, upstream_format);
            if let Some(message) = validate_response_content(&wrapped, Some(check_format)) {
                return Err(ApiError::new(StatusCode::BAD_REQUEST, message)
                    .with_code("EMPTY_RESPONSE")
                    .with_error_type("content_validation_error"));
            }
        }
        let encoded = serde_json::to_vec(&wrapped).map_err(|e| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                format!("failed to encode wrapped text response body: {e}"),
            )
            .with_code("PROXY_ERROR")
        })?;
        return build_response(
            status,
            &json_success_headers(moderation_debug),
            boxed(Body::from(encoded)),
        );
    }

    build_response(status, headers, boxed(Body::from(body)))
}

fn build_passthrough_stream_body(buffered: Vec<Bytes>, upstream: ReqByteStream) -> BoxBody {
    let buffered = VecDeque::from(buffered);
    let stream = stream::unfold(
        (buffered, upstream),
        |(mut buffered, mut upstream)| async move {
            if let Some(chunk) = buffered.pop_front() {
                return Some((Ok::<Bytes, std::io::Error>(chunk), (buffered, upstream)));
            }

            match upstream.as_mut().next().await {
                Some(Ok(chunk)) => Some((Ok(chunk), (buffered, upstream))),
                Some(Err(err)) => Some((Err(stream_read_error(err)), (buffered, upstream))),
                None => None,
            }
        },
    );

    boxed(Body::wrap_stream(stream))
}

fn build_transformed_stream_body(
    buffered: Vec<Bytes>,
    upstream: ReqByteStream,
    from_format: crate::format::RequestFormat,
    to_format: crate::format::RequestFormat,
    estimated_prompt_tokens: Option<i64>,
) -> BoxBody {
    struct TransformState {
        buffered: VecDeque<Bytes>,
        upstream: ReqByteStream,
        transcoder: StreamTranscoder,
        ready: VecDeque<Bytes>,
        flushed: bool,
    }

    let state = TransformState {
        buffered: VecDeque::from(buffered),
        upstream,
        transcoder: StreamTranscoder::new(from_format, to_format, estimated_prompt_tokens),
        ready: VecDeque::new(),
        flushed: false,
    };

    let stream = stream::unfold(state, |mut state| async move {
        loop {
            if let Some(chunk) = state.ready.pop_front() {
                return Some((Ok::<Bytes, std::io::Error>(chunk), state));
            }

            if let Some(input) = state.buffered.pop_front() {
                let out = state.transcoder.feed_chunk(&input);
                if !out.is_empty() {
                    state.ready.push_back(Bytes::from(out));
                }
                continue;
            }

            if state.flushed {
                return None;
            }

            match state.upstream.as_mut().next().await {
                Some(Ok(chunk)) => {
                    let out = state.transcoder.feed_chunk(&chunk);
                    if !out.is_empty() {
                        state.ready.push_back(Bytes::from(out));
                    }
                }
                Some(Err(err)) => {
                    state.flushed = true;
                    return Some((Err(stream_read_error(err)), state));
                }
                None => {
                    state.flushed = true;
                    let tail = state.transcoder.flush();
                    if !tail.is_empty() {
                        state.ready.push_back(Bytes::from(tail));
                    }
                }
            }
        }
    });

    boxed(Body::wrap_stream(stream))
}

fn estimate_prompt_tokens_for_stream(
    format: Option<crate::format::RequestFormat>,
    body: &Value,
) -> Option<i64> {
    let format = format?;
    let request_format = format.as_str();
    let text = extract::extract_text_for_moderation(body, request_format);
    estimate_tokens_from_text(&text)
}

fn estimate_tokens_from_text(text: &str) -> Option<i64> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut ascii_chars = 0.0_f64;
    let mut cjk_chars = 0.0_f64;
    let mut spaces = 0.0_f64;

    for ch in trimmed.chars() {
        if ch.is_whitespace() {
            spaces += 1.0;
        } else if is_cjk_character(ch) {
            cjk_chars += 1.0;
        } else if ch.is_ascii() {
            ascii_chars += 1.0;
        } else {
            ascii_chars += 1.0;
        }
    }

    let estimated_tokens =
        (0.28 * ascii_chars + 1.4 * cjk_chars + 0.15 * spaces + 4.0).ceil() as i64;
    Some(estimated_tokens.max(1))
}

fn is_cjk_character(ch: char) -> bool {
    matches!(
        ch as u32,
        0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xF900..=0xFAFF
            | 0x20000..=0x2A6DF
            | 0x2A700..=0x2B73F
            | 0x2B740..=0x2B81F
            | 0x2B820..=0x2CEAF
            | 0x2CEB0..=0x2EBEF
            | 0x30000..=0x3134F
    )
}

async fn collect_stream_bytes(
    buffered: Vec<Bytes>,
    mut upstream: ReqByteStream,
) -> Result<Vec<u8>, ApiError> {
    let mut body = Vec::new();
    for chunk in buffered {
        body.extend_from_slice(&chunk);
    }
    while let Some(chunk) = upstream.as_mut().next().await {
        let chunk = chunk.map_err(|err| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                format!("failed to read upstream body: {err}"),
            )
            .with_code("PROXY_ERROR")
            .with_error_type("proxy_error")
        })?;
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn stream_check_format(
    client_format: Option<crate::format::RequestFormat>,
    upstream_format: Option<crate::format::RequestFormat>,
) -> crate::format::RequestFormat {
    if client_format.is_some() && client_format != upstream_format {
        client_format.expect("client format when set")
    } else {
        upstream_format.unwrap_or(crate::format::RequestFormat::OpenAiChat)
    }
}

fn stream_precheck_format(
    client_format: Option<crate::format::RequestFormat>,
    upstream_format: Option<crate::format::RequestFormat>,
) -> crate::format::RequestFormat {
    upstream_format
        .or(client_format)
        .unwrap_or(crate::format::RequestFormat::OpenAiChat)
}

fn stream_read_error(err: reqwest::Error) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, err.to_string())
}

fn build_response(
    status: StatusCode,
    headers: &HeaderMap,
    body: BoxBody,
) -> Result<Response, ApiError> {
    let mut response = Response::builder().status(status);
    if let Some(target_headers) = response.headers_mut() {
        for (name, value) in headers.iter() {
            target_headers.append(name.clone(), value.clone());
        }
    }
    response.body(body).map_err(|e| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            format!("failed to build response: {e}"),
        )
        .with_code("PROXY_ERROR")
    })
}

fn filtered_response_headers(headers: &reqwest::header::HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers.iter() {
        if HEADER_DENYLIST.contains(&name.as_str()) {
            continue;
        }
        if let Ok(header_name) = HeaderName::from_bytes(name.as_str().as_bytes()) {
            if let Ok(header_value) = HeaderValue::from_bytes(value.as_bytes()) {
                out.append(header_name, header_value);
            }
        }
    }
    out
}

fn json_success_headers(extra: &HeaderMap) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    append_extra_headers(&mut headers, extra);
    headers
}

fn passthrough_success_headers(headers: &HeaderMap, extra: &HeaderMap) -> HeaderMap {
    let mut headers = headers.clone();
    append_extra_headers(&mut headers, extra);
    headers
}

fn stream_success_headers(headers: &HeaderMap, extra: &HeaderMap) -> HeaderMap {
    let mut headers = headers.clone();
    headers.remove(axum::http::header::CONTENT_TYPE);
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    append_extra_headers(&mut headers, extra);
    headers
}

enum StreamPrecheckStatus {
    Pending,
    Passed,
    ForcePassProtectionLimit,
}

struct StreamPrecheck {
    format: crate::format::RequestFormat,
    total_bytes: usize,
    pending: Vec<u8>,
    content_chars: usize,
    has_tool_call: bool,
}

impl StreamPrecheck {
    fn new(format: crate::format::RequestFormat) -> Self {
        Self {
            format,
            total_bytes: 0,
            pending: Vec::new(),
            content_chars: 0,
            has_tool_call: false,
        }
    }

    fn push_chunk(&mut self, chunk: &[u8]) -> StreamPrecheckStatus {
        self.total_bytes += chunk.len();
        self.pending.extend_from_slice(chunk);
        self.consume_complete_frames();

        if self.has_tool_call || self.content_chars >= MIN_VALID_RESPONSE_CHARS {
            return StreamPrecheckStatus::Passed;
        }
        if self.total_bytes > 1048 {
            return StreamPrecheckStatus::ForcePassProtectionLimit;
        }
        StreamPrecheckStatus::Pending
    }

    fn finish(&mut self) -> Option<String> {
        self.consume_trailing_frame();
        if self.has_tool_call || self.content_chars >= MIN_VALID_RESPONSE_CHARS {
            return None;
        }
        if self.total_bytes == 0 {
            return Some(
                "Stream disconnected before valid content: Stream ended without any content"
                    .to_string(),
            );
        }

        Some(format!(
            "Stream disconnected before valid content: Stream content validation failed: received {} chars but content is insufficient",
            self.content_chars
        ))
    }

    fn consume_complete_frames(&mut self) {
        while let Some(end) = find_sse_frame_end(&self.pending) {
            let frame = self.pending.drain(..end).collect::<Vec<_>>();
            self.consume_frame_bytes(&frame);
        }
    }

    fn consume_trailing_frame(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let frame = std::mem::take(&mut self.pending);
        self.consume_frame_bytes(&frame);
    }

    fn consume_frame_bytes(&mut self, frame: &[u8]) {
        let text = String::from_utf8_lossy(frame);
        for raw_frame in text.split("\n\n") {
            let trimmed = raw_frame.trim();
            if trimmed.is_empty() {
                continue;
            }

            let data = trimmed
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .unwrap_or("");
            if data.is_empty() || data == "[DONE]" {
                continue;
            }

            let Ok(payload) = serde_json::from_str::<Value>(data) else {
                continue;
            };

            match self.format {
                crate::format::RequestFormat::OpenAiResponses => {
                    match payload.get("type").and_then(Value::as_str) {
                        Some("response.output_text.delta") => {
                            if let Some(delta) = payload.get("delta").and_then(Value::as_str) {
                                self.content_chars += delta.chars().count();
                            }
                        }
                        Some(
                            "response.reasoning_text.delta"
                            | "response.reasoning_summary_text.delta",
                        ) => {
                            if let Some(delta) = payload.get("delta").and_then(Value::as_str) {
                                self.content_chars += delta.chars().count();
                            }
                        }
                        Some(
                            "response.function_call_arguments.delta"
                            | "response.function_call.delta",
                        ) => {
                            self.has_tool_call = true;
                        }
                        Some("response.output_item.added") => {
                            if payload
                                .get("item")
                                .and_then(Value::as_object)
                                .and_then(|item| item.get("type"))
                                .and_then(Value::as_str)
                                == Some("function_call")
                            {
                                self.has_tool_call = true;
                            }
                        }
                        _ => {}
                    }
                }
                crate::format::RequestFormat::OpenAiChat => {
                    if let Some(choices) = payload.get("choices").and_then(Value::as_array) {
                        for choice in choices {
                            if let Some(delta) = choice.get("delta").and_then(Value::as_object) {
                                if let Some(text) = delta.get("content").and_then(Value::as_str) {
                                    self.content_chars += text.chars().count();
                                }
                                if let Some(text) =
                                    delta.get("reasoning_content").and_then(Value::as_str)
                                {
                                    self.content_chars += text.chars().count();
                                }
                                if delta.get("tool_calls").is_some() {
                                    self.has_tool_call = true;
                                }
                            }
                        }
                    }
                }
                crate::format::RequestFormat::ClaudeChat => {
                    match payload.get("type").and_then(Value::as_str) {
                        Some("content_block_delta") => {
                            if let Some(delta) = payload.get("delta").and_then(Value::as_object) {
                                match delta.get("type").and_then(Value::as_str) {
                                    Some("text_delta") => {
                                        if let Some(text) =
                                            delta.get("text").and_then(Value::as_str)
                                        {
                                            self.content_chars += text.chars().count();
                                        }
                                    }
                                    Some("thinking_delta") => {
                                        if let Some(text) =
                                            delta.get("thinking").and_then(Value::as_str)
                                        {
                                            self.content_chars += text.chars().count();
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Some("message_start") => {
                            if payload
                                .get("message")
                                .and_then(Value::as_object)
                                .and_then(|message| message.get("content"))
                                .and_then(Value::as_array)
                                .is_some_and(|items| {
                                    items.iter().any(|item| {
                                        item.get("type").and_then(Value::as_str) == Some("tool_use")
                                    })
                                })
                            {
                                self.has_tool_call = true;
                            }
                        }
                        Some("content_block_start") => {
                            if payload
                                .get("content_block")
                                .and_then(Value::as_object)
                                .and_then(|block| block.get("type"))
                                .and_then(Value::as_str)
                                == Some("tool_use")
                            {
                                self.has_tool_call = true;
                            }
                        }
                        _ => {}
                    }
                }
                crate::format::RequestFormat::GeminiChat => {
                    if let Some(candidates) = payload.get("candidates").and_then(Value::as_array) {
                        for candidate in candidates {
                            if let Some(parts) = candidate
                                .get("content")
                                .and_then(Value::as_object)
                                .and_then(|content| content.get("parts"))
                                .and_then(Value::as_array)
                            {
                                for part in parts {
                                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                                        self.content_chars += text.chars().count();
                                    }
                                    if part.get("functionCall").is_some() {
                                        self.has_tool_call = true;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn is_stream_response(headers: &reqwest::header::HeaderMap) -> bool {
    headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.starts_with("text/event-stream"))
        .unwrap_or(false)
}

fn looks_like_sse_body(body: &[u8]) -> bool {
    let text = String::from_utf8_lossy(body);
    text.lines()
        .any(|line| line.starts_with("data: ") || line.starts_with("event: "))
}

fn looks_like_sse_prefix(buffered: &[Bytes]) -> bool {
    let mut raw = Vec::new();
    for chunk in buffered {
        raw.extend_from_slice(chunk);
        if raw.len() >= 2048 {
            break;
        }
    }
    looks_like_sse_body(&raw)
}

fn looks_like_json_prefix(buffered: &[Bytes]) -> bool {
    let mut raw = Vec::new();
    for chunk in buffered {
        raw.extend_from_slice(chunk);
        if raw.len() >= 256 {
            break;
        }
    }
    let text = String::from_utf8_lossy(&raw);
    let trimmed = text.trim_start();
    trimmed.starts_with('{') || trimmed.starts_with('[')
}

fn find_sse_frame_end(buffer: &[u8]) -> Option<usize> {
    let mut idx = 0;
    while idx < buffer.len() {
        if idx + 1 < buffer.len() && buffer[idx] == b'\n' && buffer[idx + 1] == b'\n' {
            return Some(idx + 2);
        }
        if idx + 3 < buffer.len()
            && buffer[idx] == b'\r'
            && buffer[idx + 1] == b'\n'
            && buffer[idx + 2] == b'\r'
            && buffer[idx + 3] == b'\n'
        {
            return Some(idx + 4);
        }
        idx += 1;
    }
    None
}

fn detect_source_format(path: &str, body: Option<&Value>) -> Option<&'static str> {
    let object = body.and_then(Value::as_object);

    if path.contains("/v1/messages")
        || object.is_some_and(|body| body.contains_key("anthropic_version"))
    {
        return Some("claude_chat");
    }

    if path.contains("/v1/responses")
        || object.is_some_and(|body| {
            body.get("object").and_then(Value::as_str) == Some("response")
                || body.contains_key("input")
        })
    {
        return Some("openai_responses");
    }

    if path.contains("/v1beta/models/") || object.is_some_and(|body| body.contains_key("contents"))
    {
        return Some("gemini_chat");
    }

    if path.contains("/v1/chat/completions")
        || object.is_some_and(|body| body.contains_key("messages"))
    {
        return Some("openai_chat");
    }

    None
}

fn moderation_details_for_message(message: &str) -> Option<Value> {
    let normalized = message.to_ascii_lowercase();
    if normalized.contains("concurrency") {
        return Some(serde_json::json!({
            "source": "concurrency_limit"
        }));
    }
    None
}

fn smart_moderation_error_message(result: &smart::SmartModerationResult) -> String {
    // 对外统一中文文案;类别/置信度/来源只进服务端日志,不提供可被用于
    // 迭代绕过的差异反馈
    tracing::info!(
        source = %result.source,
        category = result.category.as_deref().unwrap_or(""),
        confidence = result.confidence.unwrap_or(0.0),
        reason = result.reason.as_deref().unwrap_or(""),
        "smart moderation blocked"
    );
    crate::moderation::MODERATION_BLOCKED_MESSAGE.to_string()
}

fn validate_response_content(
    response: &Value,
    format: Option<crate::format::RequestFormat>,
) -> Option<String> {
    let object = match response.as_object() {
        Some(object) => object,
        None => return None,
    };

    let mut accumulated_content = String::new();
    let mut has_tool_call = false;

    match format.unwrap_or(crate::format::RequestFormat::OpenAiChat) {
        crate::format::RequestFormat::GeminiChat => {
            if let Some(candidates) = object.get("candidates").and_then(Value::as_array) {
                for candidate in candidates {
                    let parts = candidate
                        .get("content")
                        .and_then(Value::as_object)
                        .and_then(|content| content.get("parts"))
                        .and_then(Value::as_array);
                    if let Some(parts) = parts {
                        for part in parts {
                            if let Some(text) = part.get("text").and_then(Value::as_str) {
                                accumulated_content.push_str(text);
                            }
                            if part.get("functionCall").is_some() {
                                has_tool_call = true;
                            }
                        }
                    }
                }
            }
        }
        crate::format::RequestFormat::OpenAiResponses => {
            if let Some(items) = object.get("output").and_then(Value::as_array) {
                for item in items {
                    match item.get("type").and_then(Value::as_str) {
                        Some("reasoning") => {
                            if let Some(parts) = item.get("summary").and_then(Value::as_array) {
                                for part in parts {
                                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                                        accumulated_content.push_str(text);
                                    }
                                }
                            }
                            if let Some(parts) = item.get("content").and_then(Value::as_array) {
                                for part in parts {
                                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                                        accumulated_content.push_str(text);
                                    }
                                }
                            }
                        }
                        Some("function_call" | "function_call_output" | "tool_result") => {
                            has_tool_call = true;
                        }
                        _ => {
                            let content = item.get("content");
                            if let Some(parts) = content.and_then(Value::as_array) {
                                for part in parts {
                                    if matches!(
                                        part.get("type").and_then(Value::as_str),
                                        Some("output_text" | "input_text" | "text")
                                    ) {
                                        if let Some(text) = part.get("text").and_then(Value::as_str)
                                        {
                                            accumulated_content.push_str(text);
                                        }
                                    }
                                }
                            } else if let Some(text) = content.and_then(Value::as_str) {
                                accumulated_content.push_str(text);
                            }
                        }
                    }
                }
            }
        }
        crate::format::RequestFormat::ClaudeChat => {
            if let Some(blocks) = object.get("content").and_then(Value::as_array) {
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                accumulated_content.push_str(text);
                            }
                        }
                        Some("thinking") => {
                            if let Some(text) = block.get("thinking").and_then(Value::as_str) {
                                accumulated_content.push_str(text);
                            }
                        }
                        Some("tool_use") => has_tool_call = true,
                        _ => {}
                    }
                }
            }
        }
        crate::format::RequestFormat::OpenAiChat => {
            if let Some(choices) = object.get("choices").and_then(Value::as_array) {
                for choice in choices {
                    if let Some(message) = choice.get("message").and_then(Value::as_object) {
                        if let Some(text) = message.get("content").and_then(Value::as_str) {
                            accumulated_content.push_str(text);
                        }
                        if let Some(text) = message.get("reasoning_content").and_then(Value::as_str)
                        {
                            accumulated_content.push_str(text);
                        }
                        if message.get("tool_calls").is_some() {
                            has_tool_call = true;
                        }
                    }
                }
            }
        }
    }

    if has_tool_call || accumulated_content.chars().count() >= MIN_VALID_RESPONSE_CHARS {
        return None;
    }

    Some(empty_response_message_with_stats(
        accumulated_content.chars().count(),
        has_tool_call,
        format,
    ))
}

fn validate_stream_content(
    raw: &[u8],
    format: Option<crate::format::RequestFormat>,
) -> Option<String> {
    if raw.len() > 1048 {
        return None;
    }

    if raw.iter().all(u8::is_ascii_whitespace) {
        return Some(
            "Stream disconnected before valid content: Stream ended without any content"
                .to_string(),
        );
    }

    let text = String::from_utf8_lossy(raw);
    let mut accumulated_content = String::new();
    let mut has_tool_call = false;

    for frame in text.split("\n\n") {
        let trimmed = frame.trim();
        if trimmed.is_empty() {
            continue;
        }

        let data = trimmed
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap_or("");
        if data.is_empty() || data == "[DONE]" {
            continue;
        }

        let Ok(payload) = serde_json::from_str::<Value>(data) else {
            continue;
        };

        match format.unwrap_or(crate::format::RequestFormat::OpenAiChat) {
            crate::format::RequestFormat::OpenAiResponses => {
                match payload.get("type").and_then(Value::as_str) {
                    Some("response.output_text.delta") => {
                        if let Some(delta) = payload.get("delta").and_then(Value::as_str) {
                            accumulated_content.push_str(delta);
                        }
                    }
                    Some(
                        "response.reasoning_text.delta" | "response.reasoning_summary_text.delta",
                    ) => {
                        if let Some(delta) = payload.get("delta").and_then(Value::as_str) {
                            accumulated_content.push_str(delta);
                        }
                    }
                    Some(
                        "response.function_call_arguments.delta" | "response.function_call.delta",
                    ) => {
                        has_tool_call = true;
                    }
                    Some("response.output_item.added") => {
                        if payload
                            .get("item")
                            .and_then(Value::as_object)
                            .and_then(|item| item.get("type"))
                            .and_then(Value::as_str)
                            == Some("function_call")
                        {
                            has_tool_call = true;
                        }
                    }
                    _ => {}
                }
            }
            crate::format::RequestFormat::OpenAiChat => {
                if let Some(choices) = payload.get("choices").and_then(Value::as_array) {
                    for choice in choices {
                        if let Some(delta) = choice.get("delta").and_then(Value::as_object) {
                            if let Some(text) = delta.get("content").and_then(Value::as_str) {
                                accumulated_content.push_str(text);
                            }
                            if let Some(text) =
                                delta.get("reasoning_content").and_then(Value::as_str)
                            {
                                accumulated_content.push_str(text);
                            }
                            if delta.get("tool_calls").is_some() {
                                has_tool_call = true;
                            }
                        }
                    }
                }
            }
            crate::format::RequestFormat::ClaudeChat => {
                match payload.get("type").and_then(Value::as_str) {
                    Some("content_block_delta") => {
                        if let Some(delta) = payload.get("delta").and_then(Value::as_object) {
                            match delta.get("type").and_then(Value::as_str) {
                                Some("text_delta") => {
                                    if let Some(text) = delta.get("text").and_then(Value::as_str) {
                                        accumulated_content.push_str(text);
                                    }
                                }
                                Some("thinking_delta") => {
                                    if let Some(text) =
                                        delta.get("thinking").and_then(Value::as_str)
                                    {
                                        accumulated_content.push_str(text);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    Some("message_start") => {
                        if payload
                            .get("message")
                            .and_then(Value::as_object)
                            .and_then(|message| message.get("content"))
                            .and_then(Value::as_array)
                            .is_some_and(|items| {
                                items.iter().any(|item| {
                                    item.get("type").and_then(Value::as_str) == Some("tool_use")
                                })
                            })
                        {
                            has_tool_call = true;
                        }
                    }
                    Some("content_block_start") => {
                        if payload
                            .get("content_block")
                            .and_then(Value::as_object)
                            .and_then(|block| block.get("type"))
                            .and_then(Value::as_str)
                            == Some("tool_use")
                        {
                            has_tool_call = true;
                        }
                    }
                    _ => {}
                }
            }
            crate::format::RequestFormat::GeminiChat => {
                if let Some(candidates) = payload.get("candidates").and_then(Value::as_array) {
                    for candidate in candidates {
                        if let Some(parts) = candidate
                            .get("content")
                            .and_then(Value::as_object)
                            .and_then(|content| content.get("parts"))
                            .and_then(Value::as_array)
                        {
                            for part in parts {
                                if let Some(text) = part.get("text").and_then(Value::as_str) {
                                    accumulated_content.push_str(text);
                                }
                                if part.get("functionCall").is_some() {
                                    has_tool_call = true;
                                }
                            }
                        }
                    }
                }
            }
        }

        if has_tool_call || accumulated_content.chars().count() >= MIN_VALID_RESPONSE_CHARS {
            return None;
        }
    }

    Some(format!(
        "Stream disconnected before valid content: Stream content validation failed: received {} chars but content is insufficient",
        accumulated_content.chars().count()
    ))
}

fn response_format_name(format: Option<crate::format::RequestFormat>) -> &'static str {
    match format.unwrap_or(crate::format::RequestFormat::OpenAiChat) {
        crate::format::RequestFormat::OpenAiChat => "openai_chat",
        crate::format::RequestFormat::OpenAiResponses => "openai_responses",
        crate::format::RequestFormat::ClaudeChat => "claude_chat",
        crate::format::RequestFormat::GeminiChat => "gemini_chat",
    }
}

fn empty_response_message_with_stats(
    chars: usize,
    has_tool_call: bool,
    format: Option<crate::format::RequestFormat>,
) -> String {
    format!(
        "Response content validation failed: accumulated {} chars (threshold: {}), has_tool_call: {}. The AI response appears to be empty or too short.Format name: {}",
        chars,
        MIN_VALID_RESPONSE_CHARS,
        has_tool_call,
        response_format_name(format)
    )
}

fn decode_response_text(body: &[u8], content_type: Option<&str>) -> String {
    let charset = content_type
        .and_then(extract_charset)
        .and_then(|label| Encoding::for_label(label.as_bytes()));

    if let Some(encoding) = charset {
        let (decoded, _, _) = encoding.decode(body);
        decoded.into_owned()
    } else {
        String::from_utf8_lossy(body).into_owned()
    }
}

fn extract_charset(content_type: &str) -> Option<&str> {
    content_type.split(';').skip(1).find_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        if name.trim().eq_ignore_ascii_case("charset") {
            Some(value.trim().trim_matches('"'))
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detect_source_format_uses_path_and_body_hints() {
        assert_eq!(
            detect_source_format(
                "/v1/messages",
                Some(&json!({
                    "messages": []
                }))
            ),
            Some("claude_chat")
        );
        assert_eq!(
            detect_source_format(
                "/proxy",
                Some(&json!({
                    "object": "response",
                    "output": []
                }))
            ),
            Some("openai_responses")
        );
    }
}
