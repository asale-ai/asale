//! Search ownership is separate from the transport: an OpenAI-shaped proxy
//! does not thereby provide OpenAI's search. Unknown endpoints stay unknown.
use crate::Provider;

pub fn adapter(provider: Provider, base: &str, model: &str) -> &'static str {
    let owner = match provider {
        Provider::Claude | Provider::ClaudeWork | Provider::ClaudeExtra => "anthropic",
        Provider::Gemini => "google",
        Provider::Deepseek => "deepseek",
        Provider::XaiApi => "xai",
        Provider::Qwen => "qwen",
        Provider::KimiApi => "kimi",
        Provider::Openrouter => "openrouter_native",
        Provider::Custom => {
            let Some(rest) = base.strip_prefix("https://") else {
                return "";
            };
            let host = rest.split('/').next().unwrap_or("");
            match host {
                "api.openai.com" => "openai",
                "api.anthropic.com" => "anthropic",
                "generativelanguage.googleapis.com" => "google",
                "api.deepseek.com" => "deepseek",
                "api.x.ai" => "xai",
                "api.moonshot.cn" | "api.moonshot.ai" => "kimi",
                "dashscope.aliyuncs.com" | "dashscope-intl.aliyuncs.com" => "qwen",
                "open.bigmodel.cn" => "glm",
                "ark.cn-beijing.volces.com" => "doubao",
                "api.hunyuan.cloud.tencent.com" => "hunyuan",
                "api.perplexity.ai" => "perplexity",
                _ => "",
            }
        }
        // Subscription/CLI entitlements are not API search entitlements.
        _ => "",
    };
    let m = model.rsplit('/').next().unwrap_or(model);
    let owns_model = match owner {
        "anthropic" => m.starts_with("claude-"),
        "google" => m.starts_with("gemini-"),
        "openai" => m.starts_with("gpt-") || m.starts_with("o3") || m.starts_with("o4"),
        "deepseek" => m.starts_with("deepseek-v4"),
        "xai" => m.starts_with("grok-4"),
        "qwen" => m.starts_with("qwen") || m.starts_with("qwq"),
        "kimi" => m.starts_with("kimi-"),
        "glm" => m.starts_with("glm-"),
        "doubao" => m.starts_with("doubao-"),
        "hunyuan" => m.starts_with("hunyuan-"),
        "perplexity" => m.starts_with("sonar"),
        "openrouter_native" => true,
        _ => false,
    };
    if !owns_model {
        return "";
    }
    // This family documents Responses web_search and returns structured
    // citations. Older Qwen models keep their supported Chat search API.
    if owner == "qwen" && m.starts_with("qwen3.8-max") {
        "qwen_responses"
    } else {
        owner
    }
}

/// A search downgrade needs a search-specific refusal, not a generic API error.
pub fn unavailable(message: &str) -> bool {
    let s = message.to_ascii_lowercase();
    let search = [
        "web_search",
        "$web_search",
        "google_search",
        "googlesearch",
        "enable_search",
        "web search",
        "联网搜索",
    ]
    .iter()
    .any(|v| s.contains(v));
    search
        && [
            "unsupported",
            "not support",
            "not available",
            "not enabled",
            "permission",
            "invalid tool",
            "unknown tool",
            "rate_limit",
            "unavailable",
            "overloaded",
            "timeout",
            "rate limit",
            "supported values",
            "不支持",
            "不可用",
        ]
        .iter()
        .any(|v| s.contains(v))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_proxy_or_other_vendors_model_is_not_official_search() {
        assert_eq!(
            adapter(
                Provider::Custom,
                "https://api.openai.com.evil.test/v1",
                "gpt-5.5"
            ),
            ""
        );
        assert_eq!(
            adapter(
                Provider::Custom,
                "https://api.openai.com@evil.test/v1",
                "gpt-5.5"
            ),
            ""
        );
        assert_eq!(adapter(Provider::Qwen, "", "deepseek-v4-pro"), "");
        assert_eq!(adapter(Provider::Codex, "", "gpt-5.5"), "");
        assert_eq!(
            adapter(Provider::Custom, "https://api.openai.com/v1", "gpt-5.5"),
            "openai"
        );
        assert_eq!(
            adapter(Provider::Deepseek, "", "deepseek-v4-pro"),
            "deepseek"
        );
    }
}
