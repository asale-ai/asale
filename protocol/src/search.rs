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
        "openrouter_native" => openrouter_native_search(m),
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

/// The families OpenRouter runs `engine: "native"` search for. Every other
/// model answers that plugin with a `404` — 50 `seed-2-1-turbo` requests on
/// 2026-09-15, when this was `true` for all of them.
///
/// Its catalog API has no flag for it: `web_search_options` is listed by 19 of
/// 445 models and by no Claude, Gemini or Grok. So this follows the list in its
/// docs, and the gateway's platform search covers whatever it leaves out:
/// <https://openrouter.ai/docs/guides/features/server-tools/web-search#native-search-providers>
fn openrouter_native_search(m: &str) -> bool {
    // Image models share their family's name but not its search.
    if m.contains("image") {
        return false;
    }
    let num = |s: &str| -> u32 {
        s.split(|c: char| !c.is_ascii_digit()).find(|n| !n.is_empty()).and_then(|n| n.parse().ok()).unwrap_or(0)
    };
    if let Some(r) = m.strip_prefix("claude-") {
        // 3.5 Haiku, 3.7 Sonnet, then everything from 4.
        let dotted = |a: &str, b: &str| r.contains(&format!("{a}.{b}")) || r.contains(&format!("{a}-{b}"));
        return num(r) >= 4 || dotted("3", "7") || (dotted("3", "5") && r.contains("haiku"));
    }
    if let Some(r) = m.strip_prefix("gpt-") {
        return r.starts_with("4.1") || num(r) >= 5;
    }
    if let Some(r) = m.strip_prefix("gemini-") {
        return num(r) >= 3;
    }
    if let Some(r) = m.strip_prefix("grok-") {
        return num(r) >= 4;
    }
    m == "o3" || m.starts_with("o3-pro") || m.starts_with("o4-mini") || m.starts_with("sonar")
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

    #[test]
    fn openrouter_searches_natively_only_for_the_families_it_documents() {
        for m in [
            "claude-opus-5", "claude-fable-5-1", "claude-sonnet-4.5", "claude-3.7-sonnet", "claude-3-5-haiku",
            "anthropic/claude-opus-5", "gpt-5.5", "gpt-6-astra", "gpt-4.1-mini", "o3", "o3-pro", "o4-mini",
            "gemini-3.8-flash", "gemini-3-pro", "grok-4.6", "sonar", "sonar-pro",
        ] {
            assert_eq!(adapter(Provider::Openrouter, "", m), "openrouter_native", "{m}");
        }
        for m in [
            "seed-2-1-turbo", "qwen3-max", "deepseek-v4-pro", "kimi-k2", "gpt-4o", "gpt-4o-mini", "o3-mini",
            "gemini-2.5-pro", "grok-3", "claude-3-haiku", "claude-3-5-sonnet", "gemini-3-pro-image", "gpt-5-image-mini",
        ] {
            assert_eq!(adapter(Provider::Openrouter, "", m), "", "{m}");
        }
    }
}
