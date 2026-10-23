//! `pi --mode rpc` child-process wrapper: JSONL request/response + event fan-out.
//!
//! This is the Rust replacement for the Node.js `pi-acp` adapter's
//! `PiRpcProcess` class. It owns one `pi` subprocess and speaks Pi's RPC
//! dialect on its piped stdin/stdout:
//!
//! * **Requests** — `{"id":"wta-<n>","type":"<command>",…}\n` on stdin; the
//!   matching `{"id":"wta-<n>","type":"response","success":…,"data":…}` comes
//!   back on stdout and is correlated by id.
//! * **Events** — every other stdout line (`message_update`,
//!   `tool_execution_*`, `agent_settled`, …) is an unsolicited event, fanned
//!   out to the receiver [`PiRpcProcess::spawn`] hands back to the caller.
//!
//! ## Why this exists
//!
//! The Node.js adapter burned a full Node runtime per agent pane just to
//! shuttle JSONL between ACP and Pi. `wta pi-acp` speaks ACP natively on stdio
//! and uses this module for the Pi side, so no Node.js is involved at all.
//!
//! ## Deliberate limits
//!
//! * Lines that are not JSON (Pi prints a human-readable banner before RPC
//!   starts, e.g. "Context/Skills/Extensions" info) are logged and dropped.
//!   The Node adapter collected them into `preludeLines` and rendered them into
//!   a startup message; WTA's own UI already shows agent/model state, so this
//!   port keeps the smaller behavior.
//! * Dialog `extension_ui_request`s are not rendered: WTA has no UI for them,
//!   so they are answered with `cancelled: true` (see
//!   [`PiRpcProcess::send_extension_ui_response`]) rather than left to block Pi.
//!   It never sends `streamingBehavior` (WTA queues prompts on its own side
//!   instead of asking Pi to steer).

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, Mutex};

/// How long a single Pi RPC request may take before it is abandoned.
///
/// Pi answers every control request immediately (`get_state`, `set_model`, …);
/// only a dead or wedged child takes longer, and hanging forever would wedge
/// the ACP session instead of surfacing an error.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Environment variable that overrides the Pi executable.
///
/// Mirrors the Node adapter's `PI_ACP_PI_COMMAND`. WTA's spawn path sets this
/// so a Bun-installed `pi.exe` (no `.cmd` shim) resolves through PATH.
const PI_COMMAND_ENV: &str = "PI_ACP_PI_COMMAND";

/// The default Pi executable name on this platform.
///
/// Windows installs Pi through Bun or npm, which produce `pi.cmd`/`pi.exe`
/// rather than an extensionless `pi`, so the bare name must be resolved
/// through PATH by the launcher below.
fn default_pi_command() -> &'static str {
    if cfg!(windows) {
        "pi.cmd"
    } else {
        "pi"
    }
}

/// Resolve the Pi executable, honoring [`PI_COMMAND_ENV`].
fn pi_command() -> String {
    std::env::var(PI_COMMAND_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default_pi_command().to_string())
}

/// Extensions tried when a bare Pi command has none, in PATHEXT preference order.
///
/// Windows `CreateProcess` resolves a bare name only as `<name>.exe`; it does
/// **not** consult PATHEXT. Rust's `Command` inherits that, so `pi` — which WTA
/// exports as `PI_ACP_PI_COMMAND` and which Bun/npm install as `pi.cmd` — would
/// fail with "program not found" even though a shell finds it fine. Resolving
/// explicitly keeps that env override working.
#[cfg(windows)]
const PI_EXECUTABLE_EXTENSIONS: &[&str] = &[".exe", ".cmd", ".bat"];

/// Resolve a bare command name through PATH and [`PI_EXECUTABLE_EXTENSIONS`].
///
/// Returns the command unchanged when it already has a path separator or an
/// extension (an explicit path is the caller's choice, not ours to reinterpret),
/// and on non-Windows where the shell semantics already work.
fn resolve_pi_program(command: &str) -> String {
    #[cfg(not(windows))]
    {
        let _ = command;
        return command.to_string();
    }
    #[cfg(windows)]
    {
        if command.contains('\\') || command.contains('/') {
            return command.to_string();
        }
        if std::path::Path::new(command).extension().is_some() {
            return command.to_string();
        }
        let Some(path) = std::env::var_os("PATH") else {
            return command.to_string();
        };
        for extension in PI_EXECUTABLE_EXTENSIONS {
            let candidate = format!("{command}{extension}");
            if std::env::split_paths(&path).any(|directory| directory.join(&candidate).is_file()) {
                return candidate;
            }
        }
        command.to_string()
    }
}

/// Build the argument list for a Pi RPC child.
///
/// `session` is a session file path or id to resume; `None` starts fresh.
fn pi_args(session: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "--mode".to_string(),
        "rpc".to_string(),
        "--no-themes".to_string(),
    ];
    if let Some(session) = session.map(str::trim).filter(|value| !value.is_empty()) {
        args.push("--session".to_string());
        args.push(session.to_string());
    }
    args
}

/// One stdout line that was not a response — i.e. a Pi event.
pub(crate) type PiEvent = Value;

/// The `extension_ui_response` line Pi expects for one dialog request.
///
/// Shape taken from Pi's `docs/rpc-extension-ui.md`; the `id` must be the one
/// from the request or Pi keeps waiting.
fn extension_ui_response(id: &str, cancelled: bool) -> Value {
    json!({ "type": "extension_ui_response", "id": id, "cancelled": cancelled })
}

/// Shared pending-request table: `id` → the oneshot the reader resolves.
type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value>>>>>;

/// A live `pi --mode rpc` child plus its JSONL plumbing.
pub(crate) struct PiRpcProcess {
    /// Writes requests; `None` once the child's stdin is gone.
    stdin: Mutex<Option<tokio::process::ChildStdin>>,
    /// Monotonic request counter behind the `wta-<n>` id prefix.
    next_id: AtomicU64,
    pending: Pending,
    /// Event sender, whose receiver [`Self::spawn`] returns to the caller.
    ///
    /// It is installed before the stdout reader starts, so the very first line
    /// Pi emits is already deliverable, and it is taken back out by the reader
    /// at EOF so the session's event pump ends instead of waiting forever.
    events: Arc<Mutex<Option<mpsc::UnboundedSender<PiEvent>>>>,
    /// Set once stdout reached EOF (or the read failed).
    ///
    /// A dead child can neither answer a request nor emit `agent_settled`, so
    /// both paths check this instead of waiting out [`REQUEST_TIMEOUT`] — and,
    /// for a turn, instead of waiting forever.
    exited: Arc<AtomicBool>,
    /// The child, kept alive for the lifetime of this struct.
    ///
    /// `kill_on_drop(true)` is set at spawn, so dropping the last reference
    /// tears the Pi process down even on an unexpected unwind.
    child: Mutex<Option<tokio::process::Child>>,
}

impl PiRpcProcess {
    /// Spawn `pi --mode rpc` in `cwd`, optionally resuming `session`.
    ///
    /// Returns the process together with the event receiver, which is created
    /// *before* the stdout reader starts: a channel opened afterwards leaves a
    /// window in which a fast child's first line finds no subscriber and is
    /// dropped, and a dropped `extension_ui_request` stalls that Pi extension
    /// for the rest of the session. Handing the receiver out here makes that
    /// window structurally impossible rather than merely small.
    ///
    /// Returns once the process is created; the caller is responsible for
    /// waiting for its first response (e.g. `get_state`) to prove the child
    /// actually speaks RPC. Events emitted in the meantime are buffered by the
    /// unbounded channel until the pump drains them.
    pub(crate) async fn spawn(
        cwd: &std::path::Path,
        session: Option<&str>,
    ) -> Result<(Self, mpsc::UnboundedReceiver<PiEvent>)> {
        let requested = pi_command();
        let program = resolve_pi_program(&requested);
        let args = pi_args(session);
        let mut command = tokio::process::Command::new(&program);
        command
            .args(&args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        // A console window flashing for every agent pane would be a
        // regression against the Node adapter, which spawned windowless.
        #[cfg(windows)]
        command.creation_flags(0x0800_0000);

        let mut child = command.spawn().with_context(|| {
            format!(
                "could not start pi (command: {requested}). Install it with \
                 `npm install -g @earendil-works/pi-coding-agent` or ensure `pi` is on PATH."
            )
        })?;
        let stdin = child.stdin.take().context("pi child has no stdin pipe")?;
        let stdout = child.stdout.take().context("pi child has no stdout pipe")?;

        // Created before `start_reader` runs, so the reader never has to hand an
        // event to a subscriber that does not exist yet.
        let (events, receiver) = mpsc::unbounded_channel();

        let process = Self {
            stdin: Mutex::new(Some(stdin)),
            next_id: AtomicU64::new(1),
            pending: Arc::new(Mutex::new(HashMap::new())),
            events: Arc::new(Mutex::new(Some(events))),
            exited: Arc::new(AtomicBool::new(false)),
            child: Mutex::new(Some(child)),
        };
        process.start_reader(stdout);
        Ok((process, receiver))
    }

    /// Read stdout lines until EOF, resolving responses and forwarding events.
    fn start_reader(&self, stdout: tokio::process::ChildStdout) {
        let pending = Arc::clone(&self.pending);
        let events = Arc::clone(&self.events);
        let exited = Arc::clone(&self.exited);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        let line = line.trim();
                        if line.is_empty() {
                            continue;
                        }
                        let Ok(message) = serde_json::from_str::<Value>(line) else {
                            // Pi's human-readable startup banner; see the module
                            // docs for why it is dropped rather than buffered.
                            tracing::debug!(target: "pi_rpc", "non-JSON pi stdout line: {line}");
                            continue;
                        };
                        if message.get("type").and_then(Value::as_str) == Some("response") {
                            let Some(id) = message.get("id").and_then(Value::as_str) else {
                                continue;
                            };
                            let waiter = pending.lock().await.remove(id);
                            if let Some(waiter) = waiter {
                                let _ = waiter.send(Ok(message));
                            }
                            continue;
                        }
                        let subscriber = events.lock().await.clone();
                        if let Some(subscriber) = subscriber {
                            let _ = subscriber.send(message);
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        tracing::debug!(target: "pi_rpc", %error, "pi stdout read failed");
                        break;
                    }
                }
            }
            // The child is gone: fail every outstanding request rather than
            // letting callers wait out their timeouts.
            exited.store(true, Ordering::Release);
            {
                let mut pending = pending.lock().await;
                for (_, waiter) in pending.drain() {
                    let _ = waiter.send(Err(anyhow!("pi process exited")));
                }
            }
            // Dropping the subscriber ends the session's event pump, which is
            // the only thing that can unblock a turn waiting for an
            // `agent_settled` that will never arrive.
            *events.lock().await = None;
        });
    }

    /// Write one JSONL line to the child's stdin.
    ///
    /// Flushing is not optional: a buffered line looks exactly like a hang to
    /// whoever is waiting for the answer.
    async fn write_line(&self, payload: &Value) -> Result<()> {
        let mut line = serde_json::to_string(payload).context("encode pi rpc line")?;
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        let Some(stdin) = stdin.as_mut() else {
            return Err(anyhow!("pi process stdin is closed"));
        };
        stdin
            .write_all(line.as_bytes())
            .await
            .context("write pi rpc line")?;
        stdin.flush().await.context("flush pi rpc line")
    }

    /// Answer a Pi `extension_ui_request` that has no correlated `response`.
    ///
    /// Pi's dialog methods (`select`, `confirm`, `input`, `editor`) block the
    /// extension until this line arrives, so a bridge that cannot render the
    /// dialog must still answer or the turn stalls. `cancelled: false` is not
    /// used: a real answer needs `value`/`confirmed`, which this bridge has no
    /// way to collect.
    pub(crate) async fn send_extension_ui_response(&self, id: &str, cancelled: bool) -> Result<()> {
        self.write_line(&extension_ui_response(id, cancelled)).await
    }

    /// Send one command and await its `response` line.
    async fn request(&self, command: Value) -> Result<Value> {
        let id = format!("wta-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        {
            // The `exited` check and the insert share one lock hold with the
            // reader's drain, so a request either lands in a table the reader
            // will still fail, or is refused here — it can never be inserted
            // after the drain and then wait out the timeout.
            let mut pending = self.pending.lock().await;
            if self.exited.load(Ordering::Acquire) {
                return Err(anyhow!("pi process exited"));
            }
            pending.insert(id.clone(), tx);
        }

        let mut payload = command;
        payload["id"] = Value::String(id.clone());
        if let Err(error) = self.write_line(&payload).await {
            self.pending.lock().await.remove(&id);
            return Err(error);
        }

        let response = match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(response)) => response?,
            Ok(Err(_)) => return Err(anyhow!("pi rpc response channel closed")),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                return Err(anyhow!("pi rpc request timed out"));
            }
        };
        if response.get("success").and_then(Value::as_bool) != Some(true) {
            let detail = response
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| response.get("data").map(Value::to_string))
                .unwrap_or_else(|| "unknown pi error".to_string());
            return Err(anyhow!("{detail}"));
        }
        Ok(response.get("data").cloned().unwrap_or(Value::Null))
    }

    pub(crate) async fn get_state(&self) -> Result<Value> {
        self.request(json!({ "type": "get_state" })).await
    }

    pub(crate) async fn get_available_models(&self) -> Result<Value> {
        self.request(json!({ "type": "get_available_models" }))
            .await
    }

    pub(crate) async fn get_available_thinking_levels(&self) -> Result<Value> {
        self.request(json!({ "type": "get_available_thinking_levels" }))
            .await
    }

    pub(crate) async fn get_messages(&self) -> Result<Value> {
        self.request(json!({ "type": "get_messages" })).await
    }

    pub(crate) async fn get_session_stats(&self) -> Result<Value> {
        self.request(json!({ "type": "get_session_stats" })).await
    }

    pub(crate) async fn get_commands(&self) -> Result<Value> {
        self.request(json!({ "type": "get_commands" })).await
    }

    pub(crate) async fn set_model(&self, provider: &str, model_id: &str) -> Result<()> {
        self.request(json!({
            "type": "set_model",
            "provider": provider,
            "modelId": model_id,
        }))
        .await
        .map(|_| ())
    }

    pub(crate) async fn set_thinking_level(&self, level: &str) -> Result<()> {
        self.request(json!({ "type": "set_thinking_level", "level": level }))
            .await
            .map(|_| ())
    }

    /// Send a prompt. `images` are base64 blocks in Pi's `{type,mimeType,data}` shape.
    pub(crate) async fn prompt(&self, message: &str, images: &[Value]) -> Result<()> {
        self.request(json!({
            "type": "prompt",
            "message": message,
            "images": images,
        }))
        .await
        .map(|_| ())
    }

    pub(crate) async fn abort(&self) -> Result<()> {
        self.request(json!({ "type": "abort" })).await.map(|_| ())
    }

    /// Stop the child by closing stdin.
    ///
    /// Pi exits on stdin EOF (verified: it terminates within a second), which
    /// is the graceful path. `kill_on_drop` covers a child that ignores it.
    pub(crate) async fn shutdown(&self) {
        self.stdin.lock().await.take();
        let mut child = self.child.lock().await;
        if let Some(child) = child.as_mut() {
            // Bound the graceful wait so a wedged child cannot block teardown.
            let exited =
                tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await;
            if exited.is_err() {
                let _ = child.start_kill();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_command_matches_the_platform_launcher() {
        if cfg!(windows) {
            assert_eq!(default_pi_command(), "pi.cmd");
        } else {
            assert_eq!(default_pi_command(), "pi");
        }
    }

    #[test]
    fn arguments_always_request_windowless_rpc() {
        assert_eq!(pi_args(None), ["--mode", "rpc", "--no-themes"]);
    }

    #[test]
    fn a_session_argument_is_appended_only_when_non_empty() {
        assert_eq!(
            pi_args(Some(r"C:\sessions\s1.jsonl")),
            [
                "--mode",
                "rpc",
                "--no-themes",
                "--session",
                r"C:\sessions\s1.jsonl"
            ]
        );
        assert_eq!(pi_args(Some("   ")), pi_args(None));
    }

    #[test]
    fn explicit_paths_and_extensioned_commands_are_not_rewritten() {
        // A caller-supplied path is authoritative; resolving it further could
        // pick a different install than the one they named.
        assert_eq!(resolve_pi_program(r"C:\Tools\pi.exe"), r"C:\Tools\pi.exe");
        assert_eq!(resolve_pi_program("pi.cmd"), "pi.cmd");
        assert_eq!(resolve_pi_program("pi.exe"), "pi.exe");
    }

    /// Pi matches the answer to a blocked dialog by `id`, so the field names
    /// and the `cancelled` flag have to match its documented shape exactly.
    #[test]
    fn an_extension_ui_response_carries_the_request_id_and_cancel_flag() {
        assert_eq!(
            extension_ui_response("uuid-3", true),
            serde_json::json!({
                "type": "extension_ui_response",
                "id": "uuid-3",
                "cancelled": true,
            })
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_bare_command_is_resolved_through_pathext() {
        // `pi` alone is what WTA exports via PI_ACP_PI_COMMAND; CreateProcess
        // cannot run it, so it must be resolved to a real extension.
        let resolved = resolve_pi_program("pi");
        assert!(
            resolved.ends_with(".exe") || resolved.ends_with(".cmd") || resolved == "pi",
            "unexpected resolution: {resolved}"
        );
        // A name that exists nowhere stays untouched so the spawn error names
        // what the caller actually asked for.
        assert_eq!(
            resolve_pi_program("zzz_definitely_not_installed_qqq"),
            "zzz_definitely_not_installed_qqq"
        );
    }
}
