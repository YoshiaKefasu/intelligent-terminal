//! `wta pi-acp` — a native Rust ACP agent that drives Pi's RPC mode.
//!
//! # Why this exists
//!
//! Pi (the `pi` coding CLI) does not speak ACP. Until now WTA reached it
//! through the Node.js adapter `pi-acp`, which meant every agent pane paid for
//! a full Node runtime (`npx` bootstrap + V8) just to shuttle JSONL between ACP
//! on stdio and `pi --mode rpc`. That adapter was a measurable CPU and memory
//! cost, and `npx` additionally made Pi's availability depend on Node.
//!
//! This module replaces it: `wta pi-acp` speaks ACP on stdio and drives
//! `pi --mode rpc` over piped JSONL, so Pi works with no Node.js installed at
//! all. `agent_registry` points Pi's `acp_launch_command` here.
//!
//! # Layout
//!
//! * [`rpc`] — one `pi` child, JSONL request/response correlation, event fan-out.
//! * [`translate`] — pure Pi→ACP shape conversion (unit-tested without a child).
//! * [`session`] — per-session turn state machine and event pump.
//! * this module — the ACP `Agent` handlers and the session registry.
//!
//! # Threading and ordering
//!
//! The ACP SDK runs handlers on a single dispatch loop that blocks while a
//! handler runs. `session/prompt` therefore must **not** await the turn: doing
//! so would also block the `session/cancel` notification, and a user pressing
//! Stop could never reach Pi. The prompt handler spawns the turn and responds
//! when it settles; `session/cancel` stays free to abort it.
//!
//! # Deliberate limits
//!
//! * `session_capabilities` advertises `list` only. `delete` would unlink Pi's
//!   transcript file, and `close` is not advertised either, so the client never
//!   asks this bridge to tear a session down. A Pi child for a closed tab
//!   therefore lives until the shared bridge exits (when WTA's master retires
//!   the agent pool, or Windows Terminal exits). Reclaiming it would need a
//!   `close` capability; keeping the advertised surface minimal was the
//!   project's explicit choice.
//! * Pi's interactive extension UI (`confirm`/`select`/`input`/`editor`) is not
//!   bridged to ACP permissions. `notify` output is surfaced as an agent
//!   message; a dialog is answered `cancelled: true` (and the user told why),
//!   because Pi blocks the calling extension until that answer arrives —
//!   ignoring it would stall the turn. Bridging them through
//!   `request_permission` would need a round-trip the event pump cannot await
//!   without blocking the dispatch loop.
//! * `session/list` ignores the request's `cwd` filter and returns every host
//!   Pi session. Pi's own adapter narrowed to the last-used cwd, which hid
//!   other projects' sessions from the picker; returning everything is both
//!   simpler and closer to what the sessions view wants.

mod rpc;
mod session;
mod translate;

use std::collections::HashMap;
use std::sync::Arc;

use acp::schema::v1;
use agent_client_protocol as acp;
use anyhow::{Context, Result};
use tokio::sync::Mutex;

use session::{OutgoingUpdate, PiSession, UpdateSender};

/// Agent name reported in `initialize`.
const AGENT_NAME: &str = "wta-pi-acp";
/// Agent title reported in `initialize` (human-readable fallback).
const AGENT_TITLE: &str = "Pi (wta native ACP bridge)";

/// Pi's own session id, kept alongside the ACP-facing id.
///
/// They normally match; they differ only when a client loads a session under an
/// id Pi reports differently, and the ACP id must stay the client's so WTA's
/// registry key keeps resolving.
struct SessionEntry {
    session: Arc<PiSession>,
    /// Pi's reported `sessionFile`, when known. Retained for diagnostics.
    #[allow(dead_code)]
    session_file: Option<String>,
}

/// Everything the handlers share.
struct Bridge {
    sessions: Mutex<HashMap<String, SessionEntry>>,
    updates: UpdateSender,
}

impl Bridge {
    async fn get(&self, session_id: &acp::schema::v1::SessionId) -> Option<Arc<PiSession>> {
        self.sessions
            .lock()
            .await
            .get(session_id.0.as_ref())
            .map(|entry| Arc::clone(&entry.session))
    }
}

/// Run the ACP agent over stdio until the client disconnects.
pub(crate) async fn run() -> Result<()> {
    let (update_tx, mut update_rx) = tokio::sync::mpsc::unbounded_channel::<OutgoingUpdate>();
    let bridge = Arc::new(Bridge {
        sessions: Mutex::new(HashMap::new()),
        updates: update_tx,
    });

    acp::Agent
        .builder()
        .name("wta-pi-acp")
        .on_receive_request(
            {
                let bridge = Arc::clone(&bridge);
                move |request: v1::ClientRequest,
                      responder: acp::Responder<serde_json::Value>,
                      cx: acp::ConnectionTo<acp::Client>| {
                    let bridge = Arc::clone(&bridge);
                    async move {
                        use v1::AgentResponse as R;
                        use v1::ClientRequest as Q;
                        match request {
                            Q::InitializeRequest(args) => respond(responder, Ok(initialize(args))),
                            Q::NewSessionRequest(args) => {
                                let bridge = Arc::clone(&bridge);
                                let result = new_session(&bridge, args).await;
                                respond(responder, result.map(R::NewSessionResponse))
                            }
                            Q::LoadSessionRequest(args) => {
                                let bridge = Arc::clone(&bridge);
                                match load_session(&bridge, args).await {
                                    Ok(response) => {
                                        respond(responder, Ok(R::LoadSessionResponse(response)))
                                    }
                                    Err(error) => responder.respond_with_error(error),
                                }
                            }
                            Q::ListSessionsRequest(_) => respond(
                                responder,
                                list_sessions().await.map(R::ListSessionsResponse),
                            ),
                            Q::SetSessionConfigOptionRequest(args) => {
                                let bridge = Arc::clone(&bridge);
                                let result = set_config_option(&bridge, args).await;
                                respond(responder, result.map(R::SetSessionConfigOptionResponse))
                            }
                            Q::PromptRequest(args) => prompt(&bridge, args, responder, &cx).await,
                            // Pi exposes no authentication, mode, fork, resume or
                            // close surface, so those stay unimplemented rather
                            // than silently pretending to work.
                            _ => responder.respond_with_error(acp::Error::method_not_found()),
                        }
                    }
                }
            },
            acp::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let bridge = Arc::clone(&bridge);
                move |notification: v1::ClientNotification, _cx| {
                    let bridge = Arc::clone(&bridge);
                    async move {
                        if let v1::ClientNotification::CancelNotification(cancel) = notification {
                            if let Some(session) = bridge.get(&cancel.session_id).await {
                                session.cancel().await;
                            }
                        }
                        Ok(())
                    }
                }
            },
            acp::on_receive_notification!(),
        )
        .connect_with(acp::Stdio::new(), async move |cx| {
            // One forwarder owns the update queue so `session/update`
            // notifications reach the client in the order they were produced.
            let forwarder = {
                let cx = cx.clone();
                tokio::spawn(async move {
                    while let Some(update) = update_rx.recv().await {
                        let notification =
                            v1::SessionNotification::new(update.session_id.clone(), update.update);
                        if let Err(error) = cx.send_notification(notification) {
                            tracing::debug!(
                                target: "pi_acp",
                                %error,
                                "dropping session update: client is gone"
                            );
                            break;
                        }
                    }
                })
            };
            // Serve until the client closes the transport.
            cx.incoming_closed().await;
            forwarder.abort();
            // Every Pi child belongs to this bridge: when the client goes away
            // nothing can consume their output, so stop them rather than
            // leaving orphaned processes behind.
            let sessions: Vec<Arc<PiSession>> = {
                let mut sessions = bridge.sessions.lock().await;
                sessions.drain().map(|(_, entry)| entry.session).collect()
            };
            for session in sessions {
                session.shutdown().await;
            }
            Ok(())
        })
        .await
        .context("pi-acp ACP connection failed")
}

/// Respond with a typed result, mapping a failure to an ACP error.
fn respond<T: serde::Serialize>(
    responder: acp::Responder<serde_json::Value>,
    result: Result<T>,
) -> acp::Result<()> {
    match result {
        Ok(value) => match serde_json::to_value(value) {
            Ok(value) => responder.respond(value),
            Err(error) => responder.respond_with_error(acp::Error::into_internal_error(error)),
        },
        Err(error) => {
            responder.respond_with_error(acp::Error::internal_error().data(format!("{error:#}")))
        }
    }
}

fn initialize(args: v1::InitializeRequest) -> v1::InitializeResponse {
    v1::InitializeResponse::new(args.protocol_version)
        .agent_info(
            v1::Implementation::new(AGENT_NAME, env!("CARGO_PKG_VERSION")).title(AGENT_TITLE),
        )
        .agent_capabilities(
            v1::AgentCapabilities::new()
                .load_session(true)
                .prompt_capabilities(
                    v1::PromptCapabilities::new()
                        .image(true)
                        // Embedded resources are inlined into the prompt text.
                        .embedded_context(true),
                )
                .session_capabilities(
                    // `list` only; see the module docs for why `delete`/`close`
                    // are not advertised.
                    v1::SessionCapabilities::new().list(v1::SessionListCapabilities::new()),
                ),
        )
}

/// Validate the `cwd` a session request carries.
///
/// Pi resolves its project directory from this path, so a relative or empty
/// value would silently attach the conversation to the wrong project.
fn validated_cwd(cwd: &std::path::Path) -> Result<&std::path::Path> {
    anyhow::ensure!(
        cwd.is_absolute(),
        "cwd must be an absolute path: {}",
        cwd.display()
    );
    Ok(cwd)
}

async fn new_session(
    bridge: &Bridge,
    args: v1::NewSessionRequest,
) -> Result<v1::NewSessionResponse> {
    let cwd = validated_cwd(&args.cwd)?.to_path_buf();
    // The receiver comes straight out of `spawn`, so it exists before the stdout
    // reader starts: Pi emits events while this bridge is still awaiting
    // `get_state` and the model catalog, and an `extension_ui_request` among them
    // blocks Pi's extension until it is answered. A subscription opened after
    // `spawn` returned would drop whatever landed in that window. The unbounded
    // channel buffers until the pump drains it.
    let (process, events) = rpc::PiRpcProcess::spawn(&cwd, None).await?;
    let process = Arc::new(process);
    let state = process
        .get_state()
        .await
        .context("pi did not answer get_state")?;
    let session_id = state
        .get("sessionId")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow::anyhow!("pi reported no session id"))?
        .to_string();
    let session_file = state
        .get("sessionFile")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);

    let models = process.get_available_models().await.unwrap_or_default();
    let thinking = process
        .get_available_thinking_levels()
        .await
        .unwrap_or_default();
    let (models, thinking, config_options) = build_configuration(&state, &models, &thinking);

    let session = Arc::new(PiSession::new(
        v1::SessionId::new(session_id.clone()),
        cwd,
        Arc::clone(&process),
        models,
        thinking,
    ));
    session.start_event_pump(events, bridge.updates.clone());
    bridge.sessions.lock().await.insert(
        session_id.clone(),
        SessionEntry {
            session: Arc::clone(&session),
            session_file,
        },
    );

    publish_side_channels(&session, &bridge.updates);
    Ok(v1::NewSessionResponse::new(session_id).config_options(config_options))
}

/// Handle `session/load`.
///
/// Split from [`resume_session`] so the two failure classes can carry the right
/// JSON-RPC code: a session with no transcript is the client naming something
/// that does not exist (`invalid_params`), while a Pi that will not start or
/// will not answer is this bridge's problem (`internal_error`).
async fn load_session(
    bridge: &Bridge,
    args: v1::LoadSessionRequest,
) -> acp::Result<v1::LoadSessionResponse> {
    let requested_id = args.session_id.0.to_string();

    // Resume by exact transcript path: Pi resolves a bare id against the
    // *current* project only, so a session from another folder would silently
    // start a brand new conversation under the requested id instead. A missing
    // transcript is therefore an error, not a reason to fall back to the id.
    let transcript = tokio::task::spawn_blocking({
        let requested_id = requested_id.clone();
        move || crate::pi_disk_sessions::find_session_file(&requested_id)
    })
    .await
    .map_err(acp::Error::into_internal_error)?
    .ok_or_else(|| {
        acp::Error::invalid_params().data(format!("session transcript not found: {requested_id}"))
    })?;

    resume_session(bridge, args, &transcript)
        .await
        .map_err(|error| acp::Error::into_internal_error(&*error))
}

/// Spawn Pi against an already-located transcript and register the session.
async fn resume_session(
    bridge: &Bridge,
    args: v1::LoadSessionRequest,
    transcript: &std::path::Path,
) -> Result<v1::LoadSessionResponse> {
    let cwd = validated_cwd(&args.cwd)?.to_path_buf();
    let requested_id = args.session_id.0.to_string();
    let resume_target = transcript.to_string_lossy().into_owned();

    // Retire a previous child for this id *before* starting its replacement.
    // Both would resume the same transcript, and two `pi` processes appending to
    // one transcript file risks a corrupted session — worse than losing the old
    // child if the spawn below fails, which the client can simply retry. It also
    // matters that the removal precedes the insert: once the map entry is
    // replaced, the old process is unreachable and nothing could stop it.
    let previous = bridge.sessions.lock().await.remove(&requested_id);
    if let Some(previous) = previous {
        previous.session.shutdown().await;
    }

    let (process, events) = rpc::PiRpcProcess::spawn(&cwd, Some(&resume_target)).await?;
    let process = Arc::new(process);
    // The receiver already exists from `spawn` (see [`new_session`]), so nothing
    // Pi emits during startup — including the transcript replay below — is lost.
    let state = process
        .get_state()
        .await
        .context("pi did not answer get_state while loading a session")?;
    let session_file = state
        .get("sessionFile")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);

    let models = process.get_available_models().await.unwrap_or_default();
    let thinking = process
        .get_available_thinking_levels()
        .await
        .unwrap_or_default();
    let (models, thinking, config_options) = build_configuration(&state, &models, &thinking);

    let session = Arc::new(PiSession::new(
        v1::SessionId::new(requested_id.clone()),
        cwd,
        Arc::clone(&process),
        models,
        thinking,
    ));

    // Replay the transcript before the pump starts so replayed chunks and live
    // events cannot interleave out of order.
    if let Ok(messages) = process.get_messages().await {
        for update in translate::history_updates(&messages) {
            let _ = bridge.updates.send(OutgoingUpdate {
                session_id: session.session_id.clone(),
                update,
            });
        }
    }

    session.start_event_pump(events, bridge.updates.clone());
    bridge.sessions.lock().await.insert(
        requested_id,
        SessionEntry {
            session: Arc::clone(&session),
            session_file,
        },
    );

    publish_side_channels(&session, &bridge.updates);
    Ok(v1::LoadSessionResponse::new().config_options(config_options))
}

/// Build the advertised config selectors from Pi's raw state and catalogs.
fn build_configuration(
    state: &serde_json::Value,
    models: &serde_json::Value,
    thinking: &serde_json::Value,
) -> (
    Vec<translate::ConfigChoice>,
    Vec<translate::ConfigChoice>,
    Vec<v1::SessionConfigOption>,
) {
    let models = translate::model_choices(models);
    let thinking = translate::thinking_level_choices(thinking);
    let current_model = translate::current_model_id(state);
    let current_thinking = state
        .get("thinkingLevel")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let options = translate::to_acp_config_options(&translate::config_selections(
        &models,
        current_model.as_deref(),
        &thinking,
        current_thinking.as_deref(),
    ));
    (models, thinking, options)
}

/// Publish the updates that are not part of the session response.
///
/// These are best-effort: the session is already usable without them, so a Pi
/// that cannot answer `get_commands` or `get_session_stats` just yields a
/// session with no command list and no context-usage gauge.
fn publish_side_channels(session: &Arc<PiSession>, updates: &UpdateSender) {
    let session = Arc::clone(session);
    let updates = updates.clone();
    tokio::spawn(async move {
        if let Ok(commands) = session.process.get_commands().await {
            let available = translate_commands(&commands);
            if !available.is_empty() {
                let _ = updates.send(OutgoingUpdate {
                    session_id: session.session_id.clone(),
                    update: v1::SessionUpdate::AvailableCommandsUpdate(
                        v1::AvailableCommandsUpdate::new(available),
                    ),
                });
            }
        }
        if let Ok(stats) = session.process.get_session_stats().await {
            if let Some(update) = context_usage(&stats) {
                let _ = updates.send(OutgoingUpdate {
                    session_id: session.session_id.clone(),
                    update: v1::SessionUpdate::UsageUpdate(update),
                });
            }
        }
    });
}

/// Pi's `get_commands` payload → ACP available commands.
///
/// Only commands Pi itself handles are advertised: WTA renders this list as
/// slash commands and sends the typed command back as a prompt, so advertising
/// something Pi would forward to the model would be misleading.
fn translate_commands(data: &serde_json::Value) -> Vec<v1::AvailableCommand> {
    let Some(commands) = data.get("commands").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    commands
        .iter()
        .filter_map(|command| {
            let name = command
                .get("name")
                .and_then(serde_json::Value::as_str)?
                .trim()
                .trim_start_matches('/');
            if name.is_empty() || !seen.insert(name.to_ascii_lowercase()) {
                return None;
            }
            let description = command
                .get("description")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|description| !description.is_empty())
                .unwrap_or("Pi command");
            Some(v1::AvailableCommand::new(name, description))
        })
        .collect()
}

/// Pi's `get_session_stats` payload → an ACP context-usage update.
///
/// Returns `None` when Pi reports no usable context window, which keeps the
/// client from rendering a 0/0 gauge.
fn context_usage(stats: &serde_json::Value) -> Option<v1::UsageUpdate> {
    let usage = stats.get("contextUsage")?;
    let used = usage.get("tokens").and_then(serde_json::Value::as_u64)?;
    let size = usage
        .get("contextWindow")
        .and_then(serde_json::Value::as_u64)
        .filter(|size| *size > 0)?;
    Some(v1::UsageUpdate::new(used, size))
}

async fn list_sessions() -> Result<v1::ListSessionsResponse> {
    // Pi's transcripts are read straight off disk: the sessions view wants rows
    // before any Pi child has started, and this is the same scanner WTA's master
    // already seeds from, so the identities reconcile exactly.
    let rows = tokio::task::spawn_blocking(crate::pi_disk_sessions::scan_pi_disk_sessions)
        .await
        .context("pi session scan task failed")?;
    Ok(v1::ListSessionsResponse::new(
        rows.into_iter()
            .map(|row| crate::session_registry::to_acp_session_info(&row))
            .collect(),
    ))
}

async fn set_config_option(
    bridge: &Bridge,
    args: v1::SetSessionConfigOptionRequest,
) -> Result<v1::SetSessionConfigOptionResponse> {
    let session = bridge
        .get(&args.session_id)
        .await
        .ok_or_else(|| anyhow::anyhow!("unknown session: {}", args.session_id.0))?;
    let config_id = args.config_id.0.as_ref();
    let value = args
        .value
        .as_value_id()
        .map(|value| value.0.to_string())
        .ok_or_else(|| anyhow::anyhow!("config option {config_id} expects a string value"))?;

    match config_id {
        translate::MODEL_CONFIG_ID => session.set_model(&value).await?,
        translate::THOUGHT_LEVEL_CONFIG_ID => session.set_thinking_level(&value).await?,
        other => anyhow::bail!("unknown config option: {other}"),
    }

    // ACP requires the full post-change option set in the response.
    let config_options = session.refresh_config().await;
    if config_id == translate::THOUGHT_LEVEL_CONFIG_ID {
        if let Some(level) = session.current_thinking_level().await {
            let _ = bridge.updates.send(OutgoingUpdate {
                session_id: session.session_id.clone(),
                update: v1::SessionUpdate::CurrentModeUpdate(v1::CurrentModeUpdate::new(level)),
            });
        }
    }
    Ok(v1::SetSessionConfigOptionResponse::new(config_options))
}

/// Start a turn without blocking the ACP dispatch loop.
///
/// `session/cancel` is a notification on the same loop, so awaiting the turn
/// here would make Stop unreachable for its whole duration. The turn runs as a
/// spawned task and answers the responder when it settles.
async fn prompt(
    bridge: &Bridge,
    args: v1::PromptRequest,
    responder: acp::Responder<serde_json::Value>,
    cx: &acp::ConnectionTo<acp::Client>,
) -> acp::Result<()> {
    let Some(session) = bridge.get(&args.session_id).await else {
        return responder.respond_with_error(
            acp::Error::invalid_params().data(format!("unknown session: {}", args.session_id.0)),
        );
    };
    let (message, images) = translate::prompt_to_pi(&args.prompt);
    let updates = bridge.updates.clone();
    cx.spawn(async move {
        let stop_reason = session.prompt(&message, &images, &updates).await;
        match serde_json::to_value(v1::PromptResponse::new(stop_reason)) {
            Ok(value) => responder.respond(value),
            Err(error) => responder.respond_with_error(acp::Error::into_internal_error(error)),
        }
    })
}
