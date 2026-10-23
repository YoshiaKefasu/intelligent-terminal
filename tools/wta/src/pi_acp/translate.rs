//! Pure translation from Pi RPC payloads to ACP wire types.
//!
//! Every function here is side-effect free so the wire-shape decisions can be
//! unit-tested without spawning `pi`. The ACP session state machine in
//! [`super::session`] owns ordering and lifecycle; this module owns *shape*.
//!
//! The Node.js adapter (`pi-acp`) is the reference implementation for these
//! mappings; the comments call out where this port deliberately differs.

use agent_client_protocol::schema::v1 as acp;
use serde_json::Value;

/// ACP config-option id for Pi's model selector.
pub(crate) const MODEL_CONFIG_ID: &str = "model";
/// ACP config-option id for Pi's thinking-level selector.
pub(crate) const THOUGHT_LEVEL_CONFIG_ID: &str = "thought_level";

/// One selectable value for an ACP config option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigChoice {
    pub(crate) value: String,
    pub(crate) name: String,
}

/// A resolved config option: its id, display name, current value and choices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigSelection {
    pub(crate) id: &'static str,
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
    pub(crate) current: String,
    pub(crate) choices: Vec<ConfigChoice>,
}

/// Pi's `get_available_models` data → `(provider, id, display name)` triples.
///
/// Pi identifies a model as `provider` + `id`; ACP carries one opaque string,
/// so the adapter joins them with `/` — the same encoding `pi-acp` uses, which
/// keeps model ids stable across the two implementations.
pub(crate) fn model_choices(data: &Value) -> Vec<ConfigChoice> {
    data.get("models")
        .and_then(Value::as_array)
        .map(|models| {
            models
                .iter()
                .filter_map(|model| {
                    let provider = model.get("provider").and_then(Value::as_str)?.trim();
                    let id = model.get("id").and_then(Value::as_str)?.trim();
                    if provider.is_empty() || id.is_empty() {
                        return None;
                    }
                    let name = model
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                        .unwrap_or(id);
                    Some(ConfigChoice {
                        value: format!("{provider}/{id}"),
                        name: format!("{provider}/{name}"),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The model id Pi reports as current in `get_state`.
///
/// `None` when the state carries no usable model, so the caller can fall back
/// to the first advertised choice.
pub(crate) fn current_model_id(state: &Value) -> Option<String> {
    let model = state.get("model")?;
    let provider = model.get("provider").and_then(Value::as_str)?.trim();
    let id = model.get("id").and_then(Value::as_str)?.trim();
    (!provider.is_empty() && !id.is_empty()).then(|| format!("{provider}/{id}"))
}

/// Split an ACP model id back into Pi's `(provider, modelId)` pair.
///
/// A bare id (no `/`) is matched against the advertised catalog by
/// [`resolve_model_target`] instead, because Pi requires both halves.
pub(crate) fn split_model_id(requested: &str) -> (Option<&str>, &str) {
    match requested.split_once('/') {
        Some((provider, rest)) if !provider.is_empty() && !rest.is_empty() => {
            (Some(provider), rest)
        }
        _ => (None, requested),
    }
}

/// Resolve a requested ACP model id to the exact `(provider, id)` Pi accepts.
///
/// A qualified id (`provider/id`) is used verbatim. A bare id is looked up in
/// the catalog, which is what lets a caller pass just Pi's model id.
pub(crate) fn resolve_model_target(
    requested: &str,
    catalog: &[ConfigChoice],
) -> Option<(String, String)> {
    let (provider, model_id) = split_model_id(requested);
    if let Some(provider) = provider {
        return Some((provider.to_string(), model_id.to_string()));
    }
    catalog.iter().find_map(|choice| {
        let (choice_provider, choice_id) = choice.value.split_once('/')?;
        (choice_id == model_id).then(|| (choice_provider.to_string(), choice_id.to_string()))
    })
}

/// Pi's `get_available_thinking_levels` data → the advertised level names.
///
/// An empty or malformed list yields no choices, which the caller treats as
/// "do not advertise a thinking selector" rather than fabricating levels.
pub(crate) fn thinking_level_choices(data: &Value) -> Vec<ConfigChoice> {
    data.get("levels")
        .and_then(Value::as_array)
        .map(|levels| {
            levels
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|level| !level.is_empty())
                .map(|level| ConfigChoice {
                    value: level.to_string(),
                    name: format!("Thinking: {level}"),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Build the full `configOptions` list advertised to the ACP client.
///
/// Order matters for the UI: the model selector comes first (it is the option
/// users change most), matching `pi-acp`. An empty model catalog omits the
/// model selector entirely; a Pi with no configured provider therefore
/// advertises only the thinking level instead of an unusable empty dropdown.
pub(crate) fn config_selections(
    models: &[ConfigChoice],
    current_model: Option<&str>,
    thinking: &[ConfigChoice],
    current_thinking: Option<&str>,
) -> Vec<ConfigSelection> {
    let mut selections = Vec::new();
    if let Some(current) = current_model
        .map(str::to_string)
        .or_else(|| models.first().map(|choice| choice.value.clone()))
    {
        if !models.is_empty() {
            selections.push(ConfigSelection {
                id: MODEL_CONFIG_ID,
                name: "Model",
                description: "Select the model for this session",
                current,
                choices: models.to_vec(),
            });
        }
    }
    if let Some(current) = current_thinking
        .map(str::to_string)
        .or_else(|| thinking.first().map(|choice| choice.value.clone()))
    {
        if !thinking.is_empty() {
            selections.push(ConfigSelection {
                id: THOUGHT_LEVEL_CONFIG_ID,
                name: "Thinking",
                description: "Set the reasoning effort for this session",
                current,
                choices: thinking.to_vec(),
            });
        }
    }
    selections
}

/// Convert [`ConfigSelection`]s into the ACP wire type.
pub(crate) fn to_acp_config_options(
    selections: &[ConfigSelection],
) -> Vec<acp::SessionConfigOption> {
    selections
        .iter()
        .map(|selection| {
            let category = match selection.id {
                MODEL_CONFIG_ID => acp::SessionConfigOptionCategory::Model,
                THOUGHT_LEVEL_CONFIG_ID => acp::SessionConfigOptionCategory::ThoughtLevel,
                _ => acp::SessionConfigOptionCategory::Other(selection.id.to_string()),
            };
            acp::SessionConfigOption::select(
                selection.id,
                selection.name,
                selection.current.clone(),
                selection
                    .choices
                    .iter()
                    .map(|choice| {
                        acp::SessionConfigSelectOption::new(
                            choice.value.clone(),
                            choice.name.clone(),
                        )
                    })
                    .collect::<Vec<_>>(),
            )
            .description(selection.description)
            .category(category)
        })
        .collect()
}

/// Pi tool name → ACP [`acp::ToolKind`].
pub(crate) fn tool_kind(tool_name: &str) -> acp::ToolKind {
    match tool_name {
        "read" => acp::ToolKind::Read,
        "write" | "edit" => acp::ToolKind::Edit,
        "bash" => acp::ToolKind::Execute,
        "grep" | "glob" | "search" => acp::ToolKind::Search,
        "web_fetch" | "fetch" => acp::ToolKind::Fetch,
        _ => acp::ToolKind::Other,
    }
}

/// The command string Pi's `bash` tool was invoked with.
///
/// Pi sends `{"command": "…"}` at the top level; the nested spellings are kept
/// because the Node adapter had to accept them across Pi versions.
pub(crate) fn bash_command(args: Option<&Value>) -> Option<String> {
    let args = args?;
    for pointer in [
        "/command",
        "/cmd",
        "/args/command",
        "/args/cmd",
        "/input/command",
        "/details/command",
    ] {
        if let Some(command) = args.pointer(pointer).and_then(Value::as_str) {
            let trimmed = command.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// Flatten a Pi tool result into display text.
///
/// Precedence mirrors `pi-acp`: a structured diff first (it is the most
/// informative), then text content blocks, then stdout/stderr with an exit
/// code, and finally the raw JSON.
pub(crate) fn tool_result_text(result: Option<&Value>) -> String {
    let Some(result) = result else {
        return String::new();
    };
    if let Some(diff) = result
        .pointer("/details/diff")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|diff| !diff.is_empty())
    {
        return diff.to_string();
    }
    if let Some(text) = content_text(result.get("content")) {
        return text;
    }
    let stdout = result
        .pointer("/details/stdout")
        .or_else(|| result.pointer("/stdout"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let stderr = result
        .pointer("/details/stderr")
        .or_else(|| result.pointer("/stderr"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !stdout.trim().is_empty() || !stderr.trim().is_empty() {
        let mut parts = Vec::new();
        if !stdout.trim().is_empty() {
            parts.push(stdout.to_string());
        }
        if !stderr.trim().is_empty() {
            parts.push(format!("stderr:\n{stderr}"));
        }
        if let Some(code) = result
            .pointer("/details/exitCode")
            .or_else(|| result.pointer("/exitCode"))
            .and_then(Value::as_i64)
        {
            parts.push(format!("exit code: {code}"));
        }
        return parts.join("\n\n").trim_end().to_string();
    }
    result.to_string()
}

/// Join Pi's `[{type:"text", text:"…"}]` content blocks.
fn content_text(content: Option<&Value>) -> Option<String> {
    let blocks = content?.as_array()?;
    let joined = blocks
        .iter()
        .filter_map(|block| {
            (block.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| block.get("text").and_then(Value::as_str))
                .flatten()
        })
        .collect::<Vec<_>>()
        .join("");
    (!joined.is_empty()).then_some(joined)
}

/// The file path a tool call touches, if any.
pub(crate) fn tool_path(args: Option<&Value>) -> Option<String> {
    let args = args?;
    for key in ["path", "file_path"] {
        if let Some(path) = args.get(key).and_then(Value::as_str) {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// ACP tool-call locations for a tool call, resolved against the session cwd.
///
/// `line` is only supplied when the caller could pin a unique line (the edit
/// path does), so a plain read does not claim a bogus position.
pub(crate) fn tool_locations(
    args: Option<&Value>,
    cwd: &std::path::Path,
    line: Option<u32>,
) -> Vec<acp::ToolCallLocation> {
    let Some(path) = tool_path(args) else {
        return Vec::new();
    };
    let resolved = std::path::Path::new(&path);
    let resolved = if resolved.is_absolute() {
        resolved.to_path_buf()
    } else {
        cwd.join(resolved)
    };
    let location = acp::ToolCallLocation::new(resolved);
    vec![match line {
        Some(line) => location.line(line),
        None => location,
    }]
}

/// Convert a prompt's content blocks into Pi's `(message, images)` shape.
///
/// Pi accepts images as base64 blocks but has no audio support, and embedded
/// resources are inlined as text — the same conversions `pi-acp` performs.
pub(crate) fn prompt_to_pi(blocks: &[acp::ContentBlock]) -> (String, Vec<Value>) {
    let mut message = String::new();
    let mut images = Vec::new();
    for block in blocks {
        match block {
            acp::ContentBlock::Text(text) => message.push_str(&text.text),
            acp::ContentBlock::ResourceLink(link) => {
                message.push_str(&format!("\n[Context] {}", link.uri));
            }
            acp::ContentBlock::Image(image) => images.push(serde_json::json!({
                "type": "image",
                "mimeType": image.mime_type,
                "data": image.data,
            })),
            acp::ContentBlock::Resource(resource) => match &resource.resource {
                acp::EmbeddedResourceResource::TextResourceContents(text) => {
                    let mime = text.mime_type.as_deref().unwrap_or("text/plain");
                    message.push_str(&format!(
                        "\n[Embedded Context] {} ({mime})\n{}",
                        text.uri, text.text
                    ));
                }
                acp::EmbeddedResourceResource::BlobResourceContents(blob) => {
                    let mime = blob
                        .mime_type
                        .as_deref()
                        .unwrap_or("application/octet-stream");
                    message.push_str(&format!("\n[Embedded Context] {} ({mime})", blob.uri));
                }
                _ => message.push_str("\n[Embedded Context]"),
            },
            acp::ContentBlock::Audio(audio) => {
                message.push_str(&format!(
                    "\n[Audio] ({}) not supported by pi-acp",
                    audio.mime_type
                ));
            }
            _ => {}
        }
    }
    (message, images)
}

/// Replay Pi's stored messages as ACP session updates.
///
/// Used by `session/load`. Pi's `get_messages` returns the persisted roles
/// (`user`, `assistant`, `toolResult`); each becomes the chunk type the client
/// renders for that side of the conversation.
pub(crate) fn history_updates(messages: &Value) -> Vec<acp::SessionUpdate> {
    let Some(messages) = messages.get("messages").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut updates = Vec::new();
    for message in messages {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        match role {
            "user" => {
                if let Some(text) = content_text(message.get("content")) {
                    updates.push(acp::SessionUpdate::UserMessageChunk(
                        acp::ContentChunk::new(acp::ContentBlock::from(text)),
                    ));
                }
            }
            "assistant" => {
                if let Some(text) = content_text(message.get("content")) {
                    updates.push(acp::SessionUpdate::AgentMessageChunk(
                        acp::ContentChunk::new(acp::ContentBlock::from(text)),
                    ));
                }
            }
            "toolResult" => {
                let Some(tool_call_id) = message.get("toolCallId").and_then(Value::as_str) else {
                    continue;
                };
                let tool_name = message
                    .get("toolName")
                    .and_then(Value::as_str)
                    .unwrap_or("tool");
                let is_error = message
                    .get("isError")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let status = if is_error {
                    acp::ToolCallStatus::Failed
                } else {
                    acp::ToolCallStatus::Completed
                };
                let mut call = acp::ToolCall::new(tool_call_id.to_string(), tool_name.to_string())
                    .kind(tool_kind(tool_name))
                    .status(status);
                let text = tool_result_text(Some(message));
                if !text.is_empty() {
                    call = call.content(vec![acp::ToolCallContent::Content(acp::Content::new(
                        acp::ContentBlock::from(text),
                    ))]);
                }
                updates.push(acp::SessionUpdate::ToolCall(call));
            }
            _ => {}
        }
    }
    updates
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_choices_join_provider_and_id() {
        let data = serde_json::json!({
            "models": [
                {"provider": "anthropic", "id": "claude-sonnet-4-5", "name": "Sonnet"},
                {"provider": " ", "id": "skip-me"},
                {"provider": "openai", "id": "gpt-5"}
            ]
        });
        assert_eq!(
            model_choices(&data),
            vec![
                ConfigChoice {
                    value: "anthropic/claude-sonnet-4-5".into(),
                    name: "anthropic/Sonnet".into(),
                },
                // No `name` → the id is the label.
                ConfigChoice {
                    value: "openai/gpt-5".into(),
                    name: "openai/gpt-5".into(),
                },
            ]
        );
    }

    #[test]
    fn model_choices_tolerate_a_missing_or_malformed_catalog() {
        assert!(model_choices(&serde_json::json!({})).is_empty());
        assert!(model_choices(&serde_json::json!({"models": "nope"})).is_empty());
    }

    #[test]
    fn current_model_id_requires_both_halves() {
        assert_eq!(
            current_model_id(&serde_json::json!({"model": {"provider": "a", "id": "b"}})),
            Some("a/b".to_string())
        );
        assert_eq!(
            current_model_id(&serde_json::json!({"model": {"provider": "a"}})),
            None
        );
        assert_eq!(current_model_id(&serde_json::json!({})), None);
    }

    #[test]
    fn bare_model_ids_resolve_through_the_catalog() {
        let catalog = vec![ConfigChoice {
            value: "anthropic/claude-sonnet-4-5".into(),
            name: "Sonnet".into(),
        }];
        assert_eq!(
            resolve_model_target("claude-sonnet-4-5", &catalog),
            Some(("anthropic".into(), "claude-sonnet-4-5".into()))
        );
        assert_eq!(
            resolve_model_target("anthropic/claude-sonnet-4-5", &catalog),
            Some(("anthropic".into(), "claude-sonnet-4-5".into()))
        );
        assert_eq!(resolve_model_target("unknown", &catalog), None);
    }

    #[test]
    fn a_qualified_model_id_keeps_slashes_in_the_model_half() {
        assert_eq!(
            split_model_id("provider/model/with/slashes"),
            (Some("provider"), "model/with/slashes")
        );
        assert_eq!(split_model_id("bare"), (None, "bare"));
        assert_eq!(split_model_id("/leading"), (None, "/leading"));
    }

    #[test]
    fn thinking_levels_skip_blank_entries() {
        let data = serde_json::json!({"levels": ["low", " ", "high", 7]});
        assert_eq!(
            thinking_level_choices(&data),
            vec![
                ConfigChoice {
                    value: "low".into(),
                    name: "Thinking: low".into()
                },
                ConfigChoice {
                    value: "high".into(),
                    name: "Thinking: high".into()
                },
            ]
        );
    }

    #[test]
    fn config_selections_put_the_model_first_and_skip_empty_selectors() {
        let models = vec![ConfigChoice {
            value: "a/b".into(),
            name: "a/B".into(),
        }];
        let thinking = vec![ConfigChoice {
            value: "low".into(),
            name: "Thinking: low".into(),
        }];
        let selections = config_selections(&models, Some("a/b"), &thinking, Some("low"));
        assert_eq!(selections.len(), 2);
        assert_eq!(selections[0].id, MODEL_CONFIG_ID);
        assert_eq!(selections[1].id, THOUGHT_LEVEL_CONFIG_ID);

        // No catalog → no model selector, rather than an empty dropdown.
        assert_eq!(
            config_selections(&[], Some("a/b"), &thinking, None)
                .iter()
                .map(|selection| selection.id)
                .collect::<Vec<_>>(),
            vec![THOUGHT_LEVEL_CONFIG_ID]
        );
    }

    #[test]
    fn config_selections_fall_back_to_the_first_choice_for_the_current_value() {
        let models = vec![ConfigChoice {
            value: "a/first".into(),
            name: "a/First".into(),
        }];
        let selections = config_selections(&models, None, &[], None);
        assert_eq!(selections[0].current, "a/first");
    }

    #[test]
    fn acp_config_options_carry_model_and_thought_level_categories() {
        let options = to_acp_config_options(&config_selections(
            &[ConfigChoice {
                value: "a/b".into(),
                name: "a/B".into(),
            }],
            None,
            &[ConfigChoice {
                value: "high".into(),
                name: "Thinking: high".into(),
            }],
            None,
        ));
        assert_eq!(options.len(), 2);
        assert_eq!(
            options[0].category,
            Some(acp::SessionConfigOptionCategory::Model)
        );
        assert_eq!(
            options[1].category,
            Some(acp::SessionConfigOptionCategory::ThoughtLevel)
        );
        // Round-trips through the ACP model-selector extractor.
        let response = acp::NewSessionResponse::new("s").config_options(options);
        let (models, current) =
            crate::protocol::acp::model_select::models_from_new_session(&response);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "a/b");
        assert_eq!(current.as_deref(), Some("a/b"));
    }

    #[test]
    fn tool_kinds_cover_pis_builtin_tools() {
        assert_eq!(tool_kind("bash"), acp::ToolKind::Execute);
        assert_eq!(tool_kind("edit"), acp::ToolKind::Edit);
        assert_eq!(tool_kind("write"), acp::ToolKind::Edit);
        assert_eq!(tool_kind("read"), acp::ToolKind::Read);
        assert_eq!(tool_kind("grep"), acp::ToolKind::Search);
        assert_eq!(tool_kind("mystery"), acp::ToolKind::Other);
    }

    #[test]
    fn bash_command_reads_the_top_level_pi_shape() {
        assert_eq!(
            bash_command(Some(&serde_json::json!({"command": " echo hi "}))),
            Some("echo hi".to_string())
        );
        assert_eq!(
            bash_command(Some(&serde_json::json!({"command": "  "}))),
            None
        );
        assert_eq!(bash_command(None), None);
    }

    #[test]
    fn tool_result_text_prefers_a_structured_diff() {
        let result = serde_json::json!({
            "content": [{"type": "text", "text": "Successfully replaced"}],
            "details": {"diff": "-1 old\n+1 new"}
        });
        assert_eq!(tool_result_text(Some(&result)), "-1 old\n+1 new");
    }

    #[test]
    fn tool_result_text_joins_content_blocks_and_stdout() {
        let content = serde_json::json!({
            "content": [{"type": "text", "text": "X1\n"}]
        });
        assert_eq!(tool_result_text(Some(&content)), "X1\n");

        let shell = serde_json::json!({
            "content": [],
            "details": {"stdout": "out", "stderr": "err", "exitCode": 3}
        });
        assert_eq!(
            tool_result_text(Some(&shell)),
            "out\n\nstderr:\nerr\n\nexit code: 3"
        );
    }

    #[test]
    fn tool_result_text_falls_back_to_json() {
        let result = serde_json::json!({"content": [], "extra": 1});
        assert!(tool_result_text(Some(&result)).contains("\"extra\":1"));
        assert_eq!(tool_result_text(None), "");
    }

    #[test]
    fn tool_locations_resolve_relative_paths_against_the_session_cwd() {
        let cwd = std::path::Path::new(r"D:\repo");
        let locations = tool_locations(Some(&serde_json::json!({"path": "src/a.rs"})), cwd, None);
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].path, cwd.join("src/a.rs"));
        assert_eq!(locations[0].line, None);

        let absolute = std::path::Path::new(r"C:\elsewhere\b.rs");
        let locations = tool_locations(
            Some(&serde_json::json!({"path": absolute.to_string_lossy()})),
            cwd,
            Some(12),
        );
        assert_eq!(locations[0].path, absolute);
        assert_eq!(locations[0].line, Some(12));
        assert!(tool_locations(Some(&serde_json::json!({})), cwd, None).is_empty());
    }

    #[test]
    fn prompt_blocks_become_a_pi_message_and_images() {
        let (message, images) = prompt_to_pi(&[
            acp::ContentBlock::from("hello"),
            acp::ContentBlock::ResourceLink(acp::ResourceLink::new("name", "file:///tmp/x.txt")),
            acp::ContentBlock::Image(acp::ImageContent::new("BASE64", "image/png")),
        ]);
        assert!(message.starts_with("hello"));
        assert!(message.contains("[Context] file:///tmp/x.txt"));
        assert_eq!(
            images,
            vec![serde_json::json!({
                "type": "image",
                "mimeType": "image/png",
                "data": "BASE64",
            })]
        );
    }

    #[test]
    fn prompt_audio_is_reported_as_unsupported_text() {
        let (message, images) = prompt_to_pi(&[acp::ContentBlock::Audio(acp::AudioContent::new(
            "BASE64",
            "audio/wav",
        ))]);
        assert!(message.contains("not supported by pi-acp"));
        assert!(images.is_empty());
    }

    #[test]
    fn history_replay_uses_one_chunk_type_per_role() {
        let messages = serde_json::json!({
            "messages": [
                {"role": "system", "content": ""},
                {"role": "user", "content": [{"type": "text", "text": "hi"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "hello"}]},
                {"role": "toolResult", "toolCallId": "c1", "toolName": "bash",
                 "content": [{"type": "text", "text": "out"}]},
                {"role": "assistant", "content": []}
            ]
        });
        let updates = history_updates(&messages);
        assert_eq!(updates.len(), 3, "empty content must not emit a chunk");
        assert!(matches!(
            updates[0],
            acp::SessionUpdate::UserMessageChunk(_)
        ));
        assert!(matches!(
            updates[1],
            acp::SessionUpdate::AgentMessageChunk(_)
        ));
        match &updates[2] {
            acp::SessionUpdate::ToolCall(call) => {
                assert_eq!(call.tool_call_id.0.as_ref(), "c1");
                assert_eq!(call.kind, acp::ToolKind::Execute);
                assert_eq!(call.status, acp::ToolCallStatus::Completed);
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
    }

    #[test]
    fn history_replay_marks_failed_tool_results() {
        let updates = history_updates(&serde_json::json!({
            "messages": [{
                "role": "toolResult", "toolCallId": "c2", "toolName": "read",
                "isError": true, "content": [{"type": "text", "text": "boom"}]
            }]
        }));
        match &updates[0] {
            acp::SessionUpdate::ToolCall(call) => {
                assert_eq!(call.status, acp::ToolCallStatus::Failed)
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
    }

    #[test]
    fn history_replay_ignores_malformed_payloads() {
        assert!(history_updates(&serde_json::json!({})).is_empty());
        assert!(history_updates(&serde_json::json!({"messages": "nope"})).is_empty());
        // A toolResult without an id cannot be addressed by the client.
        assert!(history_updates(&serde_json::json!({
            "messages": [{"role": "toolResult", "toolName": "bash"}]
        }))
        .is_empty());
    }
}
