pub mod basic;
pub mod bow;
pub mod extract;
pub mod fasttext;
pub mod hashlinear;
pub mod smart;

/// 对外统一拦截文案:不区分拦截层、不含类别与命中词,避免向调用方提供
/// 可用于迭代绕过的差异反馈;详细信息仅进入服务端日志。
pub const MODERATION_BLOCKED_MESSAGE: &str =
    "请求未通过内容安全审核,已被拦截。如认为存在误判,请联系站点管理员。";
