//! Safe diagnostic vocabulary for upstream 4xx responses. Vendor prose can
//! echo credentials, prompts or account data; only fixed labels cross the wire.

use serde_json::{Value, json};

#[derive(Default, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub kind: Option<&'static str>,
    pub code: Option<&'static str>,
    pub param: Option<String>,
    pub reason: Option<&'static str>,
}

fn known<'a>(value: &str, choices: &'a [&'static str]) -> Option<&'static str> {
    choices.iter().copied().find(|choice| *choice == value)
}

fn kind(value: &str) -> Option<&'static str> {
    known(value, &[
        "invalid_request_error", "authentication_error", "permission_error",
        "not_found_error", "resource_not_found_error", "request_too_large",
        "rate_limit_error", "billing_error", "invalid_request", "insufficient_quota",
    ])
}

fn code(value: &str) -> Option<&'static str> {
    known(value, &[
        "claude_code_version_too_old", "invalid_encrypted_content", "context_length_exceeded",
        "model_not_found", "unsupported_value", "invalid_value", "invalid_type",
        "missing_required_parameter", "unknown_parameter", "unsupported_parameter",
        "invalid_api_key", "insufficient_quota", "credits_required", "rate_limit_exceeded",
        "moderation_blocked", "content_policy_violation", "invalid_image", "image_too_large",
    ])
}

fn param(value: &str) -> Option<String> {
    const ROOTS: &[&str] = &[
        "model", "messages", "tools", "tool_choice", "temperature", "top_p", "top_k",
        "max_tokens", "max_completion_tokens", "response_format", "stream", "thinking",
        "reasoning_effort", "input", "instructions", "reasoning", "include", "store", "text",
        "prompt_cache_key", "parallel_tool_calls", "service_tier", "max_output_tokens",
        "system", "output_config", "metadata", "stop_sequences", "anthropic-beta", "anthropic-version",
    ];
    const FIELDS: &[&str] = &[
        "role", "content", "type", "text", "name", "input", "id", "tool_use_id", "tool_calls",
        "function", "arguments", "encrypted_content", "source", "data", "url", "media_type",
        "input_schema", "description", "cache_control", "budget_tokens", "effort", "format",
        "required", "properties", "schema", "signature", "thinking", "is_error",
    ];
    if value.len() > 96 { return None; }
    let end_field = |s: &str| s.find(['.', '[']).unwrap_or(s.len());
    let (root, mut rest) = value.split_at(end_field(value));
    known(root, ROOTS)?;
    let mut out = root.to_string();
    while !rest.is_empty() {
        if let Some(field) = rest.strip_prefix('.') {
            let (field, tail) = field.split_at(end_field(field));
            known(field, FIELDS)?;
            out.push('.');
            out.push_str(field);
            rest = tail;
        } else if let Some(index) = rest.strip_prefix('[') {
            let (index, tail) = index.split_once(']')?;
            if index.is_empty() || index.len() > 6 || !index.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            out.push('[');
            out.push_str(index);
            out.push(']');
            rest = tail;
        } else {
            return None;
        }
    }
    Some(out)
}

const REASONS: &[(&str, &str)] = &[
    ("client_version", "The upstream requires a newer Claude Code version"),
    ("context_length", "The request exceeds the model context limit"),
    ("tool_pairing", "The upstream rejected the tool call and result pairing"),
    ("tool_schema", "The upstream rejected the tool schema"),
    ("extra_usage", "The upstream requires extra usage for this request"),
    ("insufficient_balance", "The upstream reported insufficient balance"),
    ("unsupported_parameter", "The upstream does not support a request parameter"),
    ("missing_parameter", "The upstream requires a missing parameter"),
    ("invalid_encrypted_content", "The upstream rejected encrypted content"),
    ("invalid_max_tokens", "The upstream rejected the max_tokens setting"),
    ("invalid_thinking", "The upstream rejected the thinking configuration"),
    ("invalid_messages", "The upstream rejected the messages structure"),
    ("beta_header", "The upstream rejected an Anthropic beta header"),
    ("not_classified", "No safe diagnostic matched the upstream message"),
];

fn classify_message(message: &str, code: Option<&str>) -> Option<&'static str> {
    let m = message.to_ascii_lowercase();
    Some(if code == Some("claude_code_version_too_old") || (m.contains("claude code") && m.contains("or newer is required")) {
        "client_version"
    } else if code == Some("context_length_exceeded") || m.contains("prompt is too long") || m.contains("maximum context length") {
        "context_length"
    } else if m.contains("tool_use") && m.contains("tool_result") {
        "tool_pairing"
    } else if m.contains("input_schema") || m.contains("tool schema") {
        "tool_schema"
    } else if m.contains("extra usage") {
        "extra_usage"
    } else if m.contains("insufficient balance") || m.contains("balance is insufficient") || m.contains("credit balance is too low") {
        "insufficient_balance"
    } else if code == Some("unsupported_parameter") || m.contains("unsupported parameter") {
        "unsupported_parameter"
    } else if code == Some("missing_required_parameter") || m.contains("missing required parameter") {
        "missing_parameter"
    } else if code == Some("invalid_encrypted_content") {
        "invalid_encrypted_content"
    } else if m.contains("anthropic-beta") {
        "beta_header"
    } else if m.contains("thinking") && (m.contains("budget_tokens") || m.contains("thinking is not supported")) {
        "invalid_thinking"
    } else if m.contains("max_tokens") {
        "invalid_max_tokens"
    } else if m.starts_with("messages[") || m.starts_with("messages:") {
        "invalid_messages"
    } else if !m.trim().is_empty() {
        "not_classified"
    } else {
        return None;
    })
}

impl Diagnostic {
    pub fn from_body(body: &str) -> Self {
        let Ok(value) = serde_json::from_str::<Value>(body) else { return Self::default() };
        let e = value.get("error").filter(|e| e.is_object()).unwrap_or(&value);
        let code = [e["code"].as_str(), e["details"]["error_code"].as_str()]
            .into_iter().flatten().find_map(code);
        let message = e["message"].as_str().unwrap_or_default();
        let path = e["param"].as_str().and_then(param).or_else(|| {
            // Anthropic often puts the field path before a colon, not in `param`.
            message.split_once(':').and_then(|(path, _)| param(path.trim()))
        });
        Self { kind: e["type"].as_str().and_then(kind), code, param: path, reason: classify_message(message, code) }
    }

    /// Text format understood by existing gateway versions. Validate again on
    /// receipt: a seller is not a trusted source of diagnostic labels.
    pub fn from_summary(status: u16, summary: &str) -> Self {
        let mut words = summary.split_ascii_whitespace();
        if words.next().and_then(|s| s.parse::<u16>().ok()) != Some(status) {
            return Self::default();
        }
        let mut diagnostic = Self::default();
        for word in words {
            if let Some(value) = word.strip_prefix("code=") {
                diagnostic.code = diagnostic.code.or_else(|| code(value));
            } else if let Some(value) = word.strip_prefix("param=") {
                diagnostic.param = diagnostic.param.or_else(|| param(value));
            } else if let Some(value) = word.strip_prefix("reason=") {
                diagnostic.reason = diagnostic.reason.or_else(|| known_reason(value));
            } else {
                diagnostic.kind = diagnostic.kind.or_else(|| kind(word));
                diagnostic.code = diagnostic.code.or_else(|| code(word));
            }
        }
        diagnostic
    }

    pub fn summary(&self, status: u16) -> String {
        let mut out = status.to_string();
        for (prefix, value) in [("", self.kind), ("code=", self.code), ("param=", self.param.as_deref()), ("reason=", self.reason)] {
            if let Some(value) = value {
                out.push(' ');
                out.push_str(prefix);
                out.push_str(value);
            }
        }
        out
    }

    pub fn message(&self) -> Option<&'static str> {
        REASONS.iter().find_map(|(reason, message)| (Some(*reason) == self.reason).then_some(*message))
    }

    pub fn fields(&self, status: u16) -> Value {
        json!({"status": status, "type": self.kind, "code": self.code,
            "param": self.param, "message": self.message(), "reason": self.reason})
    }
}

fn known_reason(value: &str) -> Option<&'static str> {
    REASONS.iter().find_map(|(reason, _)| (*reason == value).then_some(*reason))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_path_and_reason_survive_without_prose() {
        let diagnostic = Diagnostic::from_body(r#"{"error":{"type":"invalid_request_error","message":"messages[12].content[0]: tool_result did not match tool_use sk-secret"}}"#);
        assert_eq!(diagnostic.param.as_deref(), Some("messages[12].content[0]"));
        assert_eq!(diagnostic.reason, Some("tool_pairing"));
        let summary = diagnostic.summary(400);
        assert!(!summary.contains("sk-secret"));
        assert_eq!(Diagnostic::from_summary(400, &summary), diagnostic);
    }

    #[test]
    fn rejects_untrusted_fields_in_json_and_legacy_summaries() {
        let diagnostic = Diagnostic::from_body(r#"{"error":{"type":"secretWord","code":"tok-secret","param":"input[0].secretWord","message":"name@example.invalid private content"}}"#);
        assert_eq!(diagnostic.summary(400), "400 reason=not_classified");
        let legacy = Diagnostic::from_summary(400, "400 invalid_request_error code=tok-secret param=input[0].secretWord reason=secretWord");
        assert_eq!(legacy.summary(400), "400 invalid_request_error");
        assert_eq!(Diagnostic::from_summary(429, &legacy.summary(400)), Diagnostic::default());
    }
}
