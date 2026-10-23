//! Per-ACP-session state machine: one Pi child, one prompt turn at a time.
//!
//! This is the Rust port of `pi-acp`'s `PiAcpSession`. It owns the mapping from
//! Pi's event stream to ACP `session/update` notifications and decides when a
//! `session/prompt` request resolves.
//!
//! ## Turn completion
//!
//! Pi emits `turn_end` and `agent_end` **multiple times per user prompt**
//! (retries, compaction and queued continuations each produce their own
//! turns). Only `agent_settled` means "the agent loop is finished", so that is
//! the one event that resolves the pending `session/prompt`. Resolving on
//! `agent_end` would hand control back to the client mid-turn, and Pi would
//! then reject the next prompt with "Agent is already processing".
//!
//! ## Why the signal is a counter, not a channel
//!
//! A turn may be cancelled while Pi is still settling. A stale `agent_settled`
//! from the aborted turn must not resolve the *next* prompt, so each turn
//! records the settle count when it starts and waits for that count to
//! advance, rather than consuming a queued notification.
//!
//! ## Tool call rendering
//!
//! Pi's `bash` tool is emitted as a plain `ToolCall` with the command as its
//! title and the output as content, updated in place as
//! `tool_execution_update` arrives. `pi-acp` instead invented an ACP
//! `terminal/*` handshake for the same effect; WTA renders a tool call's
//! content directly, so the simpler shape loses no information and avoids a
//! round-trip the client would have to service.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use agent_client_protocol::schema::v1 as acp;
use serde_json::Value;
use tokio::sync::{mpsc, Mutex, Notify};

use super::rpc::{PiEvent, PiRpcProcess};
use super::translate::{self, ConfigChoice};

/// An update queued for the ACP client.
pub(crate) struct OutgoingUpdate {
    pub(crate) session_id: acp::SessionId,
    pub(crate) update: acp::SessionUpdate,
}

/// Shared handle a session uses to publish updates to the ACP connection.
pub(crate) type UpdateSender = mpsc::UnboundedSender<OutgoingUpdate>;

/// How long the post-turn context-usage lookup may take.
///
/// Matches the Node adapter's `SESSION_STATS_TIMEOUT_MS`. The gauge is cosmetic,
/// so it must never hold the `session/prompt` answer hostage.
const USAGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// Monotonic `agent_settled` counter with a wakeup, used to end a turn.
#[derive(Default)]
struct TurnSignal {
    settled: AtomicU64,
    /// Set once no further `agent_settled` can arrive because the Pi child's
    /// event stream ended.
    ///
    /// Without this a turn whose child dies mid-prompt would wait for a settle
    /// that never comes, and the ACP `session/prompt` request would never be
    /// answered.
    closed: AtomicBool,
    notify: Notify,
}

impl TurnSignal {
    /// Number of turns Pi has settled so far — the baseline for a new turn.
    fn count(&self) -> u64 {
        self.settled.load(Ordering::Acquire)
    }

    fn signal(&self) {
        self.settled.fetch_add(1, Ordering::Release);
        self.notify.notify_waiters();
    }

    /// End every current and future wait: the event source is gone.
    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    /// Wait until Pi has settled more turns than `baseline`.
    ///
    /// Returns `false` when the signal closed first — the caller must treat
    /// that as "the turn is over, but not because Pi said so".
    async fn wait_past(&self, baseline: u64) -> bool {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            // Register interest *before* re-checking. `notify_waiters` only
            // wakes already-registered waiters, so a settle landing between the
            // check and the await would otherwise be lost and hang the turn.
            notified.as_mut().enable();
            if self.count() > baseline {
                return true;
            }
            if self.closed.load(Ordering::Acquire) {
                return false;
            }
            notified.await;
        }
    }
}

/// Live state for one ACP session.
pub(crate) struct PiSession {
    pub(crate) session_id: acp::SessionId,
    pub(crate) cwd: std::path::PathBuf,
    pub(crate) process: Arc<PiRpcProcess>,
    /// Serializes turns; Pi accepts one prompt at a time.
    turn_gate: Mutex<()>,
    cancel_requested: AtomicBool,
    turn_signal: Arc<TurnSignal>,
    /// Advertised model catalog, kept for bare-id `set_model` resolution.
    models: Mutex<Vec<ConfigChoice>>,
    /// Advertised thinking levels, for `set_thinking_level` validation.
    thinking: Mutex<Vec<ConfigChoice>>,
}
impl PiSession {
    pub(crate) fn new(
        session_id: acp::SessionId,
        cwd: std::path::PathBuf,
        process: Arc<PiRpcProcess>,
        models: Vec<ConfigChoice>,
        thinking: Vec<ConfigChoice>,
    ) -> Self {
        Self {
            session_id,
            cwd,
            process,
            turn_gate: Mutex::new(()),
            cancel_requested: AtomicBool::new(false),
            turn_signal: Arc::new(TurnSignal::default()),
            models: Mutex::new(models),
            thinking: Mutex::new(thinking),
        }
    }

    /// Spawn the long-lived event pump that translates Pi events into ACP
    /// updates for this session.
    ///
    /// `receiver` must be the one [`PiRpcProcess::spawn`] returned, not one
    /// opened here: Pi emits events while this bridge is still awaiting its
    /// startup requests (`get_state`, `get_available_models`, …), and an
    /// `extension_ui_request` among them is a dialog Pi's extension blocks on
    /// until an answer arrives. A receiver created later would drop those
    /// events, and a lost dialog leaves that extension stuck for the rest of the
    /// session. The unbounded channel buffers whatever arrives before this pump
    /// starts draining it.
    ///
    /// Exactly one pump runs per session for the session's whole lifetime; it
    /// exits when the Pi child's event channel closes.
    pub(crate) fn start_event_pump(
        self: &Arc<Self>,
        mut receiver: mpsc::UnboundedReceiver<PiEvent>,
        updates: UpdateSender,
    ) {
        let session = Arc::clone(self);
        tokio::spawn(async move {
            let mut tool_calls: HashMap<String, acp::ToolCallStatus> = HashMap::new();
            while let Some(event) = receiver.recv().await {
                session.handle_event(&event, &updates, &mut tool_calls);
            }
            // The child is gone, so no `agent_settled` can arrive for the turn
            // in flight: close the signal to release it instead of hanging.
            session.turn_signal.close();
        });
    }

    /// Translate one Pi event into zero or more ACP updates.
    fn handle_event(
        &self,
        event: &Value,
        updates: &UpdateSender,
        tool_calls: &mut HashMap<String, acp::ToolCallStatus>,
    ) {
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "message_update" => self.handle_message_update(event, updates, tool_calls),
            "tool_execution_start" => {
                let Some(tool_call_id) = event.get("toolCallId").and_then(Value::as_str) else {
                    return;
                };
                let tool_name = event
                    .get("toolName")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                let args = event.get("args");
                let title = translate::bash_command(args).unwrap_or_else(|| tool_name.clone());
                let already_announced = tool_calls.contains_key(tool_call_id);
                tool_calls.insert(tool_call_id.to_string(), acp::ToolCallStatus::InProgress);
                if already_announced {
                    // The streaming `toolcall_*` events already introduced this
                    // call. Re-sending `tool_call` for the same id would make the
                    // client render a duplicate card, so only the transition to
                    // `in_progress` (and the now-known command title) is sent.
                    let mut fields = acp::ToolCallUpdateFields::new()
                        .status(acp::ToolCallStatus::InProgress)
                        .title(title);
                    if let Some(args) = args {
                        fields = fields
                            .raw_input(args.clone())
                            .locations(translate::tool_locations(Some(args), &self.cwd, None));
                    }
                    self.emit(
                        updates,
                        acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                            tool_call_id.to_string(),
                            fields,
                        )),
                    );
                    return;
                }
                let mut call = acp::ToolCall::new(tool_call_id.to_string(), title)
                    .kind(translate::tool_kind(&tool_name))
                    .status(acp::ToolCallStatus::InProgress)
                    .locations(translate::tool_locations(args, &self.cwd, None));
                if let Some(args) = args {
                    call = call.raw_input(args.clone());
                }
                self.emit(updates, acp::SessionUpdate::ToolCall(call));
            }
            "tool_execution_update" => {
                let Some(tool_call_id) = event.get("toolCallId").and_then(Value::as_str) else {
                    return;
                };
                let text = translate::tool_result_text(event.get("partialResult"));
                let mut fields =
                    acp::ToolCallUpdateFields::new().status(acp::ToolCallStatus::InProgress);
                if !text.is_empty() {
                    fields = fields.content(vec![acp::ToolCallContent::Content(
                        acp::Content::new(acp::ContentBlock::from(text)),
                    )]);
                }
                self.emit(
                    updates,
                    acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                        tool_call_id.to_string(),
                        fields,
                    )),
                );
            }
            "tool_execution_end" => {
                let Some(tool_call_id) = event.get("toolCallId").and_then(Value::as_str) else {
                    return;
                };
                let is_error = event
                    .get("isError")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let status = if is_error {
                    acp::ToolCallStatus::Failed
                } else {
                    acp::ToolCallStatus::Completed
                };
                let text = translate::tool_result_text(event.get("result"));
                let mut fields = acp::ToolCallUpdateFields::new().status(status);
                if !text.is_empty() {
                    fields = fields.content(vec![acp::ToolCallContent::Content(
                        acp::Content::new(acp::ContentBlock::from(text)),
                    )]);
                }
                // The id is done: keeping it would grow this map for the whole
                // session. `tool_execution_end` is the last event a call emits,
                // so forgetting it here cannot re-announce the same card.
                tool_calls.remove(tool_call_id);
                self.emit(
                    updates,
                    acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                        tool_call_id.to_string(),
                        fields,
                    )),
                );
            }
            "extension_ui_request" => self.handle_extension_ui(event, updates),
            "auto_retry_start" => self.emit_text(updates, format_auto_retry(event)),
            "auto_retry_end" => self.emit_text(updates, "Retry finished, resuming.".to_string()),
            "auto_compaction_start" => self.emit_text(
                updates,
                "Context nearing limit, running automatic compaction...".to_string(),
            ),
            "auto_compaction_end" => self.emit_text(
                updates,
                "Automatic compaction finished; context was summarized to continue the \
                 session."
                    .to_string(),
            ),
            // The one event that ends a turn; see the module docs.
            "agent_settled" => self.turn_signal.signal(),
            _ => {}
        }
    }

    fn handle_message_update(
        &self,
        event: &Value,
        updates: &UpdateSender,
        tool_calls: &mut HashMap<String, acp::ToolCallStatus>,
    ) {
        let Some(assistant) = event.get("assistantMessageEvent") else {
            return;
        };
        let kind = assistant.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "text_delta" => {
                if let Some(delta) = assistant.get("delta").and_then(Value::as_str) {
                    if !delta.is_empty() {
                        self.emit(
                            updates,
                            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                                acp::ContentBlock::from(delta),
                            )),
                        );
                    }
                }
            }
            "thinking_delta" => {
                if let Some(delta) = assistant.get("delta").and_then(Value::as_str) {
                    if !delta.is_empty() {
                        self.emit(
                            updates,
                            acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(
                                acp::ContentBlock::from(delta),
                            )),
                        );
                    }
                }
            }
            "toolcall_start" | "toolcall_delta" | "toolcall_end" => {
                self.handle_toolcall_stream(assistant, updates, tool_calls);
            }
            _ => {}
        }
    }

    /// Announce a tool call as soon as Pi starts streaming its arguments.
    ///
    /// Pi sends `toolcall_start` with the id and name, then `toolcall_delta`
    /// chunks carrying the JSON arguments, then `toolcall_end` with the parsed
    /// call. Announcing early is what lets the client show "running …" before
    /// the tool actually executes. Status is never downgraded: a late
    /// `toolcall_*` for an already-executing call must not push the client back
    /// to `pending`.
    fn handle_toolcall_stream(
        &self,
        assistant: &Value,
        updates: &UpdateSender,
        tool_calls: &mut HashMap<String, acp::ToolCallStatus>,
    ) {
        // `toolcall_end` carries the complete call; the streaming events carry
        // an id and name only.
        let complete = assistant.get("toolCall");
        let tool_call_id = complete
            .and_then(|call| call.get("id"))
            .or_else(|| assistant.get("id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if tool_call_id.is_empty() {
            return;
        }
        let tool_name = complete
            .and_then(|call| call.get("name"))
            .or_else(|| assistant.get("toolName"))
            .and_then(Value::as_str)
            .unwrap_or("tool")
            .to_string();
        let args = complete.and_then(|call| call.get("arguments"));
        let existing = tool_calls.get(&tool_call_id).copied();

        if existing.is_some() {
            // Already announced: refine the title now that the arguments exist,
            // and refresh the input. `toolcall_start` arrives before Pi has
            // streamed any arguments, so the first announcement can only use the
            // bare tool name; this update is what turns "bash" into the command.
            let mut fields = acp::ToolCallUpdateFields::new();
            if let Some(args) = args {
                fields = fields.raw_input(args.clone());
                if let Some(command) = translate::bash_command(Some(args)) {
                    fields = fields.title(command);
                }
            }
            self.emit(
                updates,
                acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(tool_call_id, fields)),
            );
            return;
        }

        tool_calls.insert(tool_call_id.clone(), acp::ToolCallStatus::Pending);
        let title = translate::bash_command(args).unwrap_or_else(|| tool_name.clone());
        let mut call = acp::ToolCall::new(tool_call_id, title)
            .kind(translate::tool_kind(&tool_name))
            .status(acp::ToolCallStatus::Pending)
            .locations(translate::tool_locations(args, &self.cwd, None));
        if let Some(args) = args {
            call = call.raw_input(args.clone());
        }
        self.emit(updates, acp::SessionUpdate::ToolCall(call));
    }

    /// Surface Pi's extension UI messages.
    ///
    /// `notify` carries user-visible output (for example `/todos` prints "No
    /// todos yet."), so it becomes an agent message. The dialog methods
    /// (`select`, `confirm`, `input`, `editor`) block the calling extension
    /// until Pi receives a matching `extension_ui_response`, and WTA has no UI
    /// to render them in — so they are answered as cancelled rather than left
    /// unanswered, which would stall the turn forever. `Note:` bridging them
    /// through `request_permission` is the future revision; it needs a
    /// round-trip this pump cannot await without blocking the dispatch loop.
    fn handle_extension_ui(&self, event: &Value, updates: &UpdateSender) {
        let method = event.get("method").and_then(Value::as_str).unwrap_or("");
        match method {
            "notify" => {
                if let Some(message) = event.get("message").and_then(Value::as_str) {
                    if !message.trim().is_empty() {
                        self.emit_text(updates, message.to_string());
                    }
                }
            }
            // Fire-and-forget methods: Pi does not wait for an answer, so
            // replying would only add noise to the stream.
            "setStatus" | "setTitle" | "setWidget" | "set_editor_text" => {}
            other => self.cancel_extension_ui(event, other, updates),
        }
    }

    /// Answer one dialog `extension_ui_request` with `cancelled: true`.
    ///
    /// The write is spawned: this runs on the event pump, and awaiting a stdin
    /// write there would stall every later event behind it. Pi correlates the
    /// answer by `id`, so completing it out of order is harmless.
    fn cancel_extension_ui(&self, event: &Value, method: &str, updates: &UpdateSender) {
        let Some(id) = event.get("id").and_then(Value::as_str) else {
            tracing::debug!(
                target: "pi_acp",
                method,
                "ignoring a pi extension UI request without an id"
            );
            return;
        };
        tracing::debug!(
            target: "pi_acp",
            method,
            "cancelling an extension UI request this bridge cannot render"
        );
        let process = Arc::clone(&self.process);
        let id = id.to_string();
        tokio::spawn(async move {
            if let Err(error) = process.send_extension_ui_response(&id, true).await {
                tracing::debug!(
                    target: "pi_acp",
                    %error,
                    "could not cancel a pi extension UI request"
                );
            }
        });
        // Tell the user why the extension got no answer, otherwise a prompt
        // that was auto-declined looks like the agent silently ignored them.
        let title = event
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty());
        let note = match title {
            Some(title) => {
                format!("Pi asked \"{title}\" ({method}); this bridge cannot render that prompt, so it was cancelled.")
            }
            None => format!(
                "Pi requested an interactive {method} prompt; this bridge cannot render it, so it was cancelled."
            ),
        };
        self.emit_text(updates, note);
    }

    fn emit_text(&self, updates: &UpdateSender, text: String) {
        self.emit(
            updates,
            acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::from(
                text,
            ))),
        );
    }

    fn emit(&self, updates: &UpdateSender, update: acp::SessionUpdate) {
        let _ = updates.send(OutgoingUpdate {
            session_id: self.session_id.clone(),
            update,
        });
    }

    /// Send a prompt and wait for the turn to settle.
    ///
    /// Returns the ACP stop reason. A failure to reach Pi is reported to the
    /// user as an agent message and still ends the turn, so the client is never
    /// left with a prompt that never resolves — including the case where the
    /// child exits mid-turn.
    pub(crate) async fn prompt(
        &self,
        message: &str,
        images: &[Value],
        updates: &UpdateSender,
    ) -> acp::StopReason {
        let _turn = self.turn_gate.lock().await;
        self.cancel_requested.store(false, Ordering::Release);
        let baseline = self.turn_signal.count();

        if let Err(error) = self.process.prompt(message, images).await {
            self.emit_text(updates, format!("pi rejected the prompt: {error:#}"));
            return self.stop_reason();
        }

        if !self.turn_signal.wait_past(baseline).await {
            // Pi died before settling. The turn cannot continue, but it must
            // still resolve or the client would wait forever.
            self.emit_text(updates, "Pi exited before the turn finished.".to_string());
        } else {
            self.publish_usage(updates).await;
        }
        self.stop_reason()
    }

    /// The stop reason for the turn that just ended.
    ///
    /// ACP requires `Cancelled` whenever the client sent `session/cancel`,
    /// even when the turn ended for another reason first.
    fn stop_reason(&self) -> acp::StopReason {
        if self.cancel_requested.load(Ordering::Acquire) {
            acp::StopReason::Cancelled
        } else {
            acp::StopReason::EndTurn
        }
    }

    /// Tell the client how full Pi's context window is.
    ///
    /// Best effort: a child that cannot answer `get_session_stats` just leaves
    /// the client's gauge where it was, which is not worth failing a turn over.
    /// The short timeout matters because this runs *before* the turn is
    /// answered — a wedged child must not delay the stop reason by the full
    /// [`REQUEST_TIMEOUT`](super::rpc) window.
    async fn publish_usage(&self, updates: &UpdateSender) {
        let stats = tokio::time::timeout(USAGE_TIMEOUT, self.process.get_session_stats()).await;
        let Ok(Ok(stats)) = stats else {
            return;
        };
        if let Some(usage) = super::context_usage(&stats) {
            self.emit(updates, acp::SessionUpdate::UsageUpdate(usage));
        }
    }

    /// Abort the in-flight turn. Returns once Pi has accepted the abort.
    pub(crate) async fn cancel(&self) {
        self.cancel_requested.store(true, Ordering::Release);
        if let Err(error) = self.process.abort().await {
            tracing::warn!(target: "pi_acp", %error, "pi abort failed");
        }
    }

    /// Apply an ACP `set_config_option` for the model selector.
    pub(crate) async fn set_model(&self, requested: &str) -> anyhow::Result<()> {
        let catalog = self.models.lock().await.clone();
        let (provider, model_id) = translate::resolve_model_target(requested, &catalog)
            .ok_or_else(|| anyhow::anyhow!("unknown model: {requested}"))?;
        self.process.set_model(&provider, &model_id).await
    }

    /// Apply an ACP `set_config_option` for the thinking level.
    pub(crate) async fn set_thinking_level(&self, level: &str) -> anyhow::Result<()> {
        if level.trim().is_empty() {
            anyhow::bail!("a thinking level must not be empty");
        }
        self.process.set_thinking_level(level).await
    }

    /// Re-read the advertised config selectors from Pi.
    ///
    /// Used after a model or thinking change so the response carries the
    /// post-change state (ACP requires the full option set in
    /// `SetSessionConfigOptionResponse`).
    pub(crate) async fn refresh_config(&self) -> Vec<acp::SessionConfigOption> {
        let state = self.process.get_state().await.unwrap_or(Value::Null);
        let models_data = self
            .process
            .get_available_models()
            .await
            .unwrap_or(Value::Null);
        let levels_data = self
            .process
            .get_available_thinking_levels()
            .await
            .unwrap_or(Value::Null);

        let models = translate::model_choices(&models_data);
        let thinking = translate::thinking_level_choices(&levels_data);
        let current_model = translate::current_model_id(&state);
        let current_thinking = state
            .get("thinkingLevel")
            .and_then(Value::as_str)
            .map(str::to_string);

        *self.models.lock().await = models.clone();
        *self.thinking.lock().await = thinking.clone();

        translate::to_acp_config_options(&translate::config_selections(
            &models,
            current_model.as_deref(),
            &thinking,
            current_thinking.as_deref(),
        ))
    }

    /// The current thinking level Pi reports, for `current_mode_update`.
    pub(crate) async fn current_thinking_level(&self) -> Option<String> {
        let state = self.process.get_state().await.ok()?;
        state
            .get("thinkingLevel")
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    /// Stop this session's Pi child.
    pub(crate) async fn shutdown(&self) {
        self.process.shutdown().await;
    }
}

/// Human-readable text for Pi's `auto_retry_start` event.
fn format_auto_retry(event: &Value) -> String {
    let attempt = event.get("attempt").and_then(Value::as_i64);
    let max_attempts = event.get("maxAttempts").and_then(Value::as_i64);
    let delay_ms = event.get("delayMs").and_then(Value::as_i64);
    match (attempt, max_attempts, delay_ms) {
        (Some(attempt), Some(max_attempts), Some(delay_ms)) => {
            // Round up so a sub-second delay does not read as "waiting 0s".
            let delay_seconds = ((delay_ms + 999) / 1000).max(1);
            format!("Retrying (attempt {attempt}/{max_attempts}, waiting {delay_seconds}s)...")
        }
        _ => "Retrying...".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_retry_message_names_the_attempt_and_never_reads_as_zero_seconds() {
        assert_eq!(
            format_auto_retry(&serde_json::json!({
                "attempt": 2, "maxAttempts": 5, "delayMs": 1200
            })),
            "Retrying (attempt 2/5, waiting 2s)..."
        );
        assert_eq!(
            format_auto_retry(&serde_json::json!({
                "attempt": 1, "maxAttempts": 3, "delayMs": 200
            })),
            "Retrying (attempt 1/3, waiting 1s)..."
        );
        assert_eq!(format_auto_retry(&serde_json::json!({})), "Retrying...");
    }

    /// A settle that lands before the wait starts must still be observed —
    /// this is the race the counter exists to close.
    #[tokio::test]
    async fn turn_signal_is_not_lost_when_it_settles_before_the_wait() {
        let signal = TurnSignal::default();
        let baseline = signal.count();
        signal.signal();
        let settled = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            signal.wait_past(baseline),
        )
        .await
        .expect("an already-settled turn must not block");
        assert!(settled, "a real settle must report success");
    }

    /// The child can die mid-turn; the wait must report "not settled" instead
    /// of hanging until the client gives up on the prompt.
    #[tokio::test]
    async fn closing_the_signal_releases_a_waiting_turn() {
        let signal = Arc::new(TurnSignal::default());
        let baseline = signal.count();
        let waiter = {
            let signal = Arc::clone(&signal);
            tokio::spawn(async move { signal.wait_past(baseline).await })
        };
        signal.close();
        let settled = tokio::time::timeout(std::time::Duration::from_millis(100), waiter)
            .await
            .expect("a closed signal must not block")
            .expect("waiter task must not panic");
        assert!(!settled, "a close is not a settle");
    }

    /// A turn started after the child died must fail immediately rather than
    /// waiting for a settle that can never arrive.
    #[tokio::test]
    async fn a_wait_started_after_the_close_returns_at_once() {
        let signal = TurnSignal::default();
        signal.close();
        let settled = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            signal.wait_past(signal.count()),
        )
        .await
        .expect("a closed signal must not block");
        assert!(!settled);
    }

    /// A stale settle from a previous turn must not resolve the next one.
    #[tokio::test]
    async fn turn_signal_ignores_settles_from_before_the_baseline() {
        let signal = Arc::new(TurnSignal::default());
        signal.signal();
        let baseline = signal.count();
        let waiter = {
            let signal = Arc::clone(&signal);
            tokio::spawn(async move { signal.wait_past(baseline).await })
        };
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), waiter)
                .await
                .is_err(),
            "the waiter must still be blocked before a new settle"
        );
        signal.signal();
    }

    #[tokio::test]
    async fn concurrent_waits_all_observe_the_next_settle() {
        let signal = Arc::new(TurnSignal::default());
        let baseline = signal.count();
        let first = {
            let signal = Arc::clone(&signal);
            tokio::spawn(async move { signal.wait_past(baseline).await })
        };
        let second = {
            let signal = Arc::clone(&signal);
            tokio::spawn(async move { signal.wait_past(baseline).await })
        };
        signal.signal();
        for waiter in [first, second] {
            let settled = tokio::time::timeout(std::time::Duration::from_millis(100), waiter)
                .await
                .expect("every waiter must wake")
                .expect("waiter task must not panic");
            assert!(settled);
        }
    }
}
