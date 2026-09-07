//! Moonshot's builtin search executes at the vendor but requires the caller
//! to return its arguments and continue. Keep all turns on the same account.
use crate::protocol::Usage;
use serde_json::{json, Value};

pub fn requested(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v["tools"].as_array().cloned())
        .is_some_and(|tools| {
            tools
                .iter()
                .any(|t| t["type"] == "builtin_function" && t["function"]["name"] == "$web_search")
        })
}

pub async fn complete(
    template: reqwest::RequestBuilder,
    mut request: Value,
    first: &[u8],
    budget: i64,
    max_searches: usize,
    progress: &std::sync::Mutex<Usage>,
) -> Result<Vec<u8>, String> {
    let mut response: Value =
        serde_json::from_slice(first).map_err(|_| "invalid builtin search response")?;
    let mut total = Usage::default();
    let mut searches = Vec::new();
    let mut search_tokens = 0_i64;
    let max_searches = max_searches.clamp(1, 10);
    for round in 0..=max_searches {
        let usage = crate::executor::usage_from_body(
            &serde_json::to_vec(&response).map_err(|e| e.to_string())?,
        );
        total.input_tokens = total.input_tokens.saturating_add(usage.input_tokens);
        total.output_tokens = total.output_tokens.saturating_add(usage.output_tokens);
        total.cache_read_tokens = total
            .cache_read_tokens
            .saturating_add(usage.cache_read_tokens);
        total.cache_write_tokens = total
            .cache_write_tokens
            .saturating_add(usage.cache_write_tokens);
        *progress.lock().unwrap() = total;
        let message = response
            .pointer("/choices/0/message")
            .cloned()
            .ok_or("missing builtin search message")?;
        let calls = message["tool_calls"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let builtins = calls
            .iter()
            .filter(|c| c["function"]["name"] == "$web_search")
            .count();
        if builtins > 0 && builtins != calls.len() {
            return Err("web_search unavailable: mixed client-tool continuation".into());
        }
        if builtins == 0 {
            response["usage"] = json!({"prompt_tokens":total.input_tokens + total.cache_read_tokens,
                "completion_tokens":total.output_tokens,"prompt_tokens_details":{"cached_tokens":total.cache_read_tokens}});
            response["asale_builtin_search"] =
                json!({"completed":!searches.is_empty(), "searches":searches});
            return serde_json::to_vec(&response).map_err(|e| e.to_string());
        }
        if round == max_searches || searches.len() + calls.len() > max_searches {
            return Err("web_search unavailable: continuation limit".into());
        }
        let messages = request["messages"]
            .as_array_mut()
            .ok_or("missing builtin search conversation")?;
        messages.push(message);
        for call in calls {
            let arguments = call["function"]["arguments"]
                .as_str()
                .ok_or("invalid builtin search arguments")?;
            let args: Value =
                serde_json::from_str(arguments).map_err(|_| "invalid builtin search arguments")?;
            search_tokens = search_tokens.saturating_add(
                args.pointer("/usage/total_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
                    .max(0),
            );
            searches.push(json!({"type":"server_tool_use","name":"web_search","input":args}));
            messages.push(json!({"role":"tool","tool_call_id":call["id"],"name":"$web_search","content":arguments}));
        }
        // The signed grant caps output, not prompt tokens. Search results
        // count as input; treating them as output would reject valid searches.
        let remaining = budget.saturating_sub(total.output_tokens);
        if remaining <= 0 || search_tokens > 65_536 {
            return Err("web_search unavailable: continuation budget exhausted".into());
        }
        let output_cap = request["max_tokens"]
            .as_i64()
            .unwrap_or(1024)
            .min(remaining);
        request["max_tokens"] = json!(output_cap);
        let reply = template
            .try_clone()
            .ok_or("search request cannot be cloned")?
            .json(&request)
            .send()
            .await
            .map_err(|_| "web_search unavailable: continuation transport failure")?;
        if !reply.status().is_success() {
            let status = reply.status();
            let body = reply.text().await.unwrap_or_default();
            if status.as_u16() != 401
                && status.as_u16() != 402
                && asale_protocol::search::unavailable(&body)
            {
                return Err("web_search unavailable: vendor rejected continuation search".into());
            }
            // Authentication, balance and unrelated request failures keep their
            // meaning; changing search engines must not disguise them.
            return Err(format!("upstream continuation HTTP {status}"));
        }
        response = reply
            .json()
            .await
            .map_err(|_| "invalid builtin search response")?;
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_the_vendor_builtin_is_executed_here() {
        assert!(requested(
            br#"{"tools":[{"type":"builtin_function","function":{"name":"$web_search"}}]}"#
        ));
        assert!(!requested(
            br#"{"tools":[{"type":"function","function":{"name":"web_search"}}]}"#
        ));
    }
    fn first_response() -> Value {
        json!({"choices":[{"message":{"role":"assistant","reasoning_content":"Search is needed.","tool_calls":[
            {"id":"search_1","type":"function","function":{"name":"$web_search","arguments":r#"{"query":"recent news","usage":{"total_tokens":3000}}"#}}
        ]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":100,"completion_tokens":12}})
    }
    #[tokio::test]
    async fn continuation_uses_the_same_request_and_accounts_for_every_turn() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut data = Vec::new();
            let mut buf = [0; 4096];
            let request: Value = loop {
                let n = socket.read(&mut buf).await.unwrap();
                assert!(n > 0);
                data.extend_from_slice(&buf[..n]);
                if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&data[..end]).to_lowercase();
                    assert!(headers.contains("authorization: bearer same-account"));
                    let size: usize = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .unwrap()
                        .trim()
                        .parse()
                        .unwrap();
                    if data.len() >= end + 4 + size {
                        break serde_json::from_slice(&data[end + 4..end + 4 + size]).unwrap();
                    }
                }
            };
            assert_eq!(request["model"], "kimi-k3");
            assert_eq!(request["tools"][0]["function"]["name"], "$web_search");
            assert_eq!(
                request["max_tokens"], 88,
                "only generated output spends the output grant"
            );
            assert_eq!(
                request["messages"][1]["reasoning_content"],
                "Search is needed."
            );
            assert_eq!(request["messages"][2]["name"], "$web_search");
            assert_eq!(
                request["messages"][2]["content"],
                first_response()["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"]
            );
            let body = json!({"choices":[{"message":{"role":"assistant","content":"Here is the answer."},"finish_reason":"stop"}],"usage":{"prompt_tokens":3200,"completion_tokens":20}}).to_string();
            let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let request = json!({"model":"kimi-k3","max_tokens":100,"messages":[{"role":"user","content":"news"}],"tools":[{"type":"builtin_function","function":{"name":"$web_search"}}]});
        let progress = std::sync::Mutex::new(Usage::default());
        let result = complete(
            reqwest::Client::new().post(url).bearer_auth("same-account"),
            request,
            &serde_json::to_vec(&first_response()).unwrap(),
            100,
            5,
            &progress,
        )
        .await
        .unwrap();
        server.await.unwrap();
        let result: Value = serde_json::from_slice(&result).unwrap();
        assert_eq!(result["usage"]["prompt_tokens"], 3300);
        assert_eq!(result["usage"]["completion_tokens"], 32);
        assert_eq!(result["asale_builtin_search"]["completed"], true);
        assert_eq!(progress.lock().unwrap().input_tokens, 3300);
    }
    #[tokio::test]
    async fn exhausted_output_stops_before_another_paid_call() {
        let progress = std::sync::Mutex::new(Usage::default());
        let result = complete(
            reqwest::Client::new().post("http://127.0.0.1:1"),
            json!({"messages":[]}),
            &serde_json::to_vec(&first_response()).unwrap(),
            12,
            5,
            &progress,
        )
        .await;
        assert!(result.unwrap_err().contains("budget exhausted"));
        assert_eq!(progress.lock().unwrap().output_tokens, 12);
    }
}
