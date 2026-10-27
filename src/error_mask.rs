//! 错误响应脱敏层(2026-10-28):只针对错误,高吞吐路径零开销。
//!
//! 上游(magpie)的错误文案自带内部信息——账号邮箱、容器地址、产品名。
//! 该层在响应回程上做纯替换修正:认得的模式(邮箱/URL/IP/产品名)换成占位,
//! 不认识的原样放行(fail-open:任何解析失败都退回原文,绝不因脱敏断流)。
//! 200 正常回复只在字节里出现"结构性错误帧"特征时才动手,普通内容帧
//! 一个字节不动。总开关 ERROR_MASK_ENABLED(默认开),容器环境变量控制。

use std::sync::LazyLock;

use regex::Regex;

/// 总开关:默认开启;置 ERROR_MASK_ENABLED=false 可整体关闭(回滚用)。
static ENABLED: LazyLock<bool> = LazyLock::new(|| {
    std::env::var("ERROR_MASK_ENABLED")
        .map(|value| !matches!(value.trim().to_ascii_lowercase().as_str(), "false" | "0" | "off"))
        .unwrap_or(true)
});

// 三个替换规则,只认结构性文本,误伤面极小:
// - 邮箱 → ***@***(账号身份不外泄;10-15 配额泄漏事故的主泄点)
// - URL(IP:端口 / 域名路径)→ [hidden](容器地址、上游地址)
// - 产品名 magpie(含所有格)→ the gateway(来源指纹)
static EMAIL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}").unwrap());
static URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"https?://[^\s"'<>\\]+"#).unwrap());
static BARE_ADDR: LazyLock<Regex> = LazyLock::new(|| {
    // 裸地址(不带 scheme 时 URL 规则不认):IP:端口、点分域名:端口、
    // docker 容器名:端口(字母开头)。数字开头非 IP 的(如时间 02:41)不认。
    Regex::new(
        r"\b(?:\d{1,3}(?:\.\d{1,3}){3}:\d{2,5}|[a-z0-9][a-z0-9\-]*(?:\.[a-z0-9\-]+)+:\d{2,5}|[a-z][a-z0-9\-]*:\d{2,5})\b",
    )
    .unwrap()
});
static MAGPIE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bmagpie('s)?\b").unwrap());

/// 就地脱敏一段字节。返回是否改动过(供日志观测)。
/// 非 UTF-8 内容直接放行(二进制错误体不常见,安全优先)。
pub fn mask_bytes(data: &mut Vec<u8>) -> bool {
    if !*ENABLED || data.is_empty() {
        return false;
    }
    let Ok(text) = std::str::from_utf8(data) else {
        return false;
    };
    let owned = text.to_owned();
    let masked = EMAIL.replace_all(&owned, "***@***");
    let masked = URL.replace_all(&masked, "[hidden]");
    let masked = BARE_ADDR.replace_all(&masked, "[hidden]");
    let masked = MAGPIE.replace_all(&masked, "the gateway");
    if masked == owned {
        return false;
    }
    let changed: String = masked.into_owned();
    data.clear();
    data.extend_from_slice(changed.as_bytes());
    true
}

/// 200 流里的"结构性错误帧"特征判断——只有命中才值得跑正则。
///
/// 三个特征串在 JSON 字符串值内部必然带反斜杠转义(\"error\":),
/// 原始字节里连续出现 `\"error\":`/`\"type\":\"error\"` 只可能是结构位置,
/// 客户正文里讨论 error/magpie 不会触发。
pub fn looks_like_error_frame(chunk: &[u8]) -> bool {
    contains_seq(chunk, b"\"error\":")
        || contains_seq(chunk, b"\"type\":\"error\"")
        || contains_line_prefixed_event_error(chunk)
}

fn contains_seq(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn contains_line_prefixed_event_error(chunk: &[u8]) -> bool {
    // Claude SSE: "event: error" 行首前缀(结构性事件名)
    let mut start = 0usize;
    while let Some(pos) = chunk[start..]
        .windows(12)
        .position(|w| w == b"event: error")
    {
        let abs = start + pos;
        if abs == 0 || chunk[abs - 1] == b'\n' {
            return true;
        }
        start = abs + 1;
    }
    false
}

/// 流式分块的脱敏闸门:错误流(mask_all)或结构性错误帧命中才动手,
/// 普通 200 内容帧零拷贝零正则原样返回。
pub fn maybe_mask_chunk(mask_all: bool, chunk: bytes::Bytes) -> bytes::Bytes {
    if !*ENABLED {
        return chunk;
    }
    if !mask_all && !looks_like_error_frame(&chunk) {
        return chunk;
    }
    let mut data = chunk.to_vec();
    if mask_bytes(&mut data) {
        tracing::info!(bytes = data.len(), "masked error stream chunk");
        return bytes::Bytes::from(data);
    }
    chunk
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mask(input: &str) -> String {
        let mut data = input.as_bytes().to_vec();
        mask_bytes(&mut data);
        String::from_utf8(data).unwrap()
    }

    #[test]
    fn masks_email_url_and_product_name() {
        let raw = "Codex (me@example.com) is at 92% of a usage window, past its 90% cap; raise it in magpie's settings. Upstream at http://127.0.0.1:41603/backend-api/codex answered 502.";
        let out = mask(raw);
        assert!(!out.contains("me@example.com"), "{out}");
        assert!(!out.contains("magpie"), "{out}");
        assert!(!out.contains("127.0.0.1:41603"), "{out}");
        assert!(!out.contains("http://"), "{out}");
        assert!(out.contains("***@***"), "{out}");
        assert!(out.contains("the gateway"), "{out}");
        // 非敏感部分保留(百分比等不在规则内)
        assert!(out.contains("92%"), "{out}");
    }

    #[test]
    fn masks_bare_host_port_without_scheme() {
        let out = mask("provider fake (fake/m1, sub.internal:8443/v1) answered 405");
        assert!(!out.contains("sub.internal:8443"), "{out}");
        assert!(out.contains("[hidden]"), "{out}");
    }

    #[test]
    fn leaves_plain_text_untouched() {
        let raw = "a normal reply about errors and retries, 42% done";
        assert_eq!(mask(raw), raw);
    }

    #[test]
    fn error_frame_gate_hits_structural_only() {
        assert!(looks_like_error_frame(b"data: {\"error\":{\"message\":\"x\"}}"));
        assert!(looks_like_error_frame(
            b"data: {\"type\":\"error\",\"code\":\"rate_limit\"}"
        ));
        assert!(looks_like_error_frame(b"event: error\ndata: {}\n\n"));
        // 正文中转义的 \"error\": 不会被误判(带反斜杠)
        assert!(!looks_like_error_frame(
            b"data: {\"delta\":{\"content\":\"he said \\\"error\\\": nope\"}}"
        ));
        assert!(!looks_like_error_frame(b"data: {\"delta\":{\"content\":\"talk about magpie and error events\"}}"));
    }

    #[test]
    fn non_utf8_passes_through() {
        let mut data = vec![0xff, 0xfe, 0x00, 0x01];
        assert!(!mask_bytes(&mut data));
        assert_eq!(data, vec![0xff, 0xfe, 0x00, 0x01]);
    }
}
