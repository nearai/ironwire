//! Anthropic Messages → OpenAI Chat Completions (the request half).
//!
//! This is the translated lane's outbound direction. Everything the target
//! cannot represent is dropped **deliberately and namedly** — the dropped list
//! is returned to the caller so a route can explain what it cost, rather than
//! degrading silently.

use serde_json::{Map, Value, json};

use crate::tool_ids;

/// What the translation could not carry across.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dropped {
    /// Signed/redacted thinking blocks. The target never validates them and the
    /// originating API drops rather than rejects them on the way back.
    pub thinking_blocks: usize,
    /// `cache_control` breakpoints. OpenAI-compatible endpoints cache
    /// automatically or not at all; there is nothing to map them onto.
    pub cache_breakpoints: usize,
    /// Image blocks, when the target is text-only. The gate refuses this route
    /// when images are present, so a non-zero count here is a bug.
    pub images: usize,
    /// Content-block types this build does not recognise, by name.
    ///
    /// Previously these fell through a `_ => {}` and vanished — silently, and
    /// without appearing in this struct at all, which made the module's own
    /// promise ("dropped deliberately and namedly") untrue for exactly the case
    /// that matters most. Anthropic ships new block types regularly; a
    /// `document` a user asked a question about would have been discarded, and
    /// the model would have answered as though it were never sent.
    ///
    /// A non-empty list makes the *route* ineligible rather than degrading the
    /// request: we cannot tell whether an unrecognised block was load-bearing,
    /// and the native lane handles it perfectly. Waiting a turn for same-family
    /// capacity beats an answer about a document the model never saw.
    pub unknown_blocks: Vec<String>,
}

impl Dropped {
    /// Whether anything was lost.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Translate an Anthropic Messages request body into a Chat Completions body.
///
/// `model` replaces the client's model string; the caller has already chosen it.
#[must_use]
pub fn anthropic_to_chat_completions(body: &Value, model: &str, stream: bool) -> (Value, Dropped) {
    let mut dropped = Dropped::default();
    let mut messages: Vec<Value> = Vec::new();

    if let Some(system) = body.get("system") {
        let text = flatten_system(system, &mut dropped);
        if !text.is_empty() {
            messages.push(json!({"role": "system", "content": text}));
        }
    }

    if let Some(turns) = body.get("messages").and_then(Value::as_array) {
        for turn in turns {
            translate_turn(turn, &mut messages, &mut dropped);
        }
    }

    let mut request = Map::new();
    request.insert("model".into(), json!(model));
    request.insert("messages".into(), Value::Array(messages));
    request.insert("stream".into(), json!(stream));
    if stream {
        // Without this the final chunk carries no usage and we lose all
        // accounting for every cross-family turn.
        request.insert("stream_options".into(), json!({"include_usage": true}));
    }
    if let Some(max_tokens) = body.get("max_tokens") {
        request.insert("max_tokens".into(), max_tokens.clone());
    }
    if let Some(stop) = body.get("stop_sequences") {
        request.insert("stop".into(), stop.clone());
    }
    if let Some(temperature) = body.get("temperature") {
        request.insert("temperature".into(), temperature.clone());
    }

    if let Some(tools) = body.get("tools").and_then(Value::as_array)
        && !tools.is_empty()
    {
        request.insert(
            "tools".into(),
            Value::Array(tools.iter().map(translate_tool).collect()),
        );
        if let Some(choice) = body.get("tool_choice").and_then(translate_tool_choice) {
            request.insert("tool_choice".into(), choice);
        }
    }

    (Value::Object(request), dropped)
}

/// Anthropic's `system` is a string or a list of blocks; Chat Completions wants
/// one string.
fn flatten_system(system: &Value, dropped: &mut Dropped) -> String {
    match system {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                if block.get("cache_control").is_some() {
                    dropped.cache_breakpoints += 1;
                }
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    parts.push(text);
                }
            }
            parts.join("\n\n")
        }
        _ => String::new(),
    }
}

/// One Anthropic turn can become several Chat Completions messages: a user turn
/// carrying tool results becomes one `role: "tool"` message per result.
fn translate_turn(turn: &Value, out: &mut Vec<Value>, dropped: &mut Dropped) {
    let role = turn.get("role").and_then(Value::as_str).unwrap_or("user");
    let content = turn.get("content");

    // The simple case: content is a bare string.
    if let Some(text) = content.and_then(Value::as_str) {
        out.push(json!({"role": role, "content": text}));
        return;
    }
    let Some(blocks) = content.and_then(Value::as_array) else {
        return;
    };

    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut tool_results: Vec<Value> = Vec::new();

    for block in blocks {
        if block.get("cache_control").is_some() {
            dropped.cache_breakpoints += 1;
        }
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    text_parts.push(text.to_string());
                }
            }
            // Provider-private reasoning state. The target never validates it,
            // so dropping it is correct rather than lossy-but-tolerated.
            Some("thinking" | "redacted_thinking") => dropped.thinking_blocks += 1,
            Some("image") => dropped.images += 1,
            Some("tool_use") => {
                let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                tool_calls.push(json!({
                    "id": tool_ids::to_foreign(id),
                    "type": "function",
                    "function": {
                        "name": name,
                        // Chat Completions carries arguments as a JSON *string*.
                        "arguments": serde_json::to_string(&input).unwrap_or_else(|_| "{}".into()),
                    },
                }));
            }
            Some("tool_result") => {
                let id = block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                tool_results.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_ids::to_foreign(id),
                    "content": flatten_tool_result(block.get("content")),
                }));
            }
            // A block we do not model. Named rather than swallowed — see
            // `Dropped::unknown_blocks`.
            other => {
                let name = other.unwrap_or("<no type field>");
                if !dropped.unknown_blocks.iter().any(|seen| seen == name) {
                    dropped.unknown_blocks.push(name.to_string());
                }
            }
        }
    }

    // Tool results are their own messages and must precede nothing else in the
    // turn; Anthropic packs them into a user turn, Chat Completions does not.
    out.extend(tool_results);

    let text = text_parts.join("\n");
    if !tool_calls.is_empty() {
        let mut message = Map::new();
        message.insert("role".into(), json!("assistant"));
        // OpenAI requires the key even when there is no prose alongside a call.
        message.insert(
            "content".into(),
            if text.is_empty() {
                Value::Null
            } else {
                json!(text)
            },
        );
        message.insert("tool_calls".into(), Value::Array(tool_calls));
        out.push(Value::Object(message));
    } else if !text.is_empty() {
        out.push(json!({"role": role, "content": text}));
    }
}

/// A tool result's content is a string or a list of blocks.
fn flatten_tool_result(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn translate_tool(tool: &Value) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.get("name").cloned().unwrap_or(Value::Null),
            "description": tool.get("description").cloned().unwrap_or(json!("")),
            "parameters": tool
                .get("input_schema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
        },
    })
}

fn translate_tool_choice(choice: &Value) -> Option<Value> {
    match choice.get("type").and_then(Value::as_str)? {
        "auto" => Some(json!("auto")),
        "any" => Some(json!("required")),
        "none" => Some(json!("none")),
        "tool" => choice
            .get("name")
            .and_then(Value::as_str)
            .map(|name| json!({"type": "function", "function": {"name": name}})),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Claude Code turn with everything the translation has to handle.
    fn claude_code_body() -> Value {
        json!({
            "model": "claude-opus-4-6",
            "max_tokens": 8192,
            "stream": true,
            "system": [
                {"type": "text", "text": "You are Claude Code.",
                 "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "Working directory: /repo."}
            ],
            "thinking": {"type": "enabled", "budget_tokens": 4096},
            "tools": [{
                "name": "Bash",
                "description": "run a command",
                "input_schema": {"type": "object", "properties": {"command": {"type": "string"}}}
            }],
            "messages": [
                {"role": "user", "content": "fix the failing test"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "let me look", "signature": "sig-abc"},
                    {"type": "text", "text": "Running the tests."},
                    {"type": "tool_use", "id": "toolu_01ABC", "name": "Bash",
                     "input": {"command": "cargo test"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_01ABC",
                     "content": "assertion failed", "is_error": true}
                ]},
                {"role": "assistant", "content": [{"type": "text", "text": "Off-by-one."}]},
                {"role": "user", "content": "go ahead and fix it"}
            ]
        })
    }

    #[test]
    fn a_claude_code_turn_becomes_a_valid_chat_completions_request() {
        let (out, _) = anthropic_to_chat_completions(&claude_code_body(), "near-x", true);
        assert_eq!(out["model"], "near-x");
        assert_eq!(out["stream"], true);
        assert_eq!(out["max_tokens"], 8192);
        assert_eq!(out["stream_options"]["include_usage"], true);

        let messages = out["messages"].as_array().expect("messages");
        // system, user, assistant(+call), tool, assistant, user
        assert_eq!(messages[0]["role"], "system");
        assert!(
            messages[0]["content"]
                .as_str()
                .expect("system text")
                .contains("You are Claude Code.")
        );
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[5]["content"], "go ahead and fix it");
    }

    #[test]
    fn both_system_blocks_survive_the_flattening() {
        let (out, _) = anthropic_to_chat_completions(&claude_code_body(), "near-x", false);
        let system = out["messages"][0]["content"].as_str().expect("system");
        assert!(system.contains("Claude Code"));
        assert!(system.contains("/repo"), "second block lost: {system}");
    }

    #[test]
    fn tool_calls_carry_arguments_as_a_json_string() {
        // The single most common way this translation is written wrong.
        let (out, _) = anthropic_to_chat_completions(&claude_code_body(), "near-x", false);
        let call = &out["messages"][2]["tool_calls"][0];
        assert_eq!(call["type"], "function");
        assert_eq!(call["function"]["name"], "Bash");
        let args = call["function"]["arguments"].as_str().expect("a string");
        let parsed: Value = serde_json::from_str(args).expect("valid JSON in the string");
        assert_eq!(parsed["command"], "cargo test");
    }

    #[test]
    fn a_tool_result_becomes_its_own_tool_message_keyed_by_call_id() {
        let (out, _) = anthropic_to_chat_completions(&claude_code_body(), "near-x", false);
        let result = &out["messages"][3];
        assert_eq!(result["role"], "tool");
        assert_eq!(result["tool_call_id"], "toolu_01ABC");
        assert_eq!(result["content"], "assertion failed");
    }

    #[test]
    fn thinking_and_cache_breakpoints_are_dropped_and_counted() {
        // Dropping silently is the failure mode this struct exists to prevent.
        let (out, dropped) = anthropic_to_chat_completions(&claude_code_body(), "near-x", false);
        assert_eq!(dropped.thinking_blocks, 1);
        assert_eq!(dropped.cache_breakpoints, 1);
        assert_eq!(dropped.images, 0);
        assert!(!dropped.is_empty());
        assert!(
            !out.to_string().contains("sig-abc"),
            "a signature leaked into the foreign request"
        );
    }

    #[test]
    fn tools_translate_to_openai_function_shape() {
        let (out, _) = anthropic_to_chat_completions(&claude_code_body(), "near-x", false);
        let tool = &out["tools"][0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["function"]["name"], "Bash");
        assert_eq!(tool["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn tool_choice_maps_across() {
        let choices = [
            (json!({"type": "auto"}), json!("auto")),
            (json!({"type": "any"}), json!("required")),
            (json!({"type": "none"}), json!("none")),
            (
                json!({"type": "tool", "name": "Bash"}),
                json!({"type": "function", "function": {"name": "Bash"}}),
            ),
        ];
        for (anthropic, expected) in choices {
            let body = json!({
                "tools": [{"name": "Bash", "input_schema": {}}],
                "tool_choice": anthropic,
                "messages": [],
            });
            let (out, _) = anthropic_to_chat_completions(&body, "m", false);
            assert_eq!(out["tool_choice"], expected);
        }
    }

    #[test]
    fn an_assistant_turn_with_only_a_tool_call_still_carries_the_content_key() {
        // OpenAI-compatible servers commonly reject an assistant message with
        // no `content` key at all.
        let body = json!({"messages": [{"role": "assistant", "content": [
            {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {}}
        ]}]});
        let (out, _) = anthropic_to_chat_completions(&body, "m", false);
        assert!(out["messages"][0].get("content").is_some());
        assert!(out["messages"][0]["content"].is_null());
    }

    #[test]
    fn an_empty_conversation_produces_a_well_formed_request() {
        let (out, dropped) = anthropic_to_chat_completions(&json!({}), "m", false);
        assert_eq!(out["model"], "m");
        assert!(out["messages"].as_array().expect("array").is_empty());
        assert!(dropped.is_empty());
        assert!(out.get("tools").is_none());
    }
}
