//! Herdr integration: read a running herdr server's session snapshot — the
//! folder of the pane it has focused, and its agents' states — over its
//! Windows named pipe.
//!
//! Ported from ELYTerminal (`src/term/herdr.rs` and `src/term/herdr/windows.rs`).
//! ELYTerminal finds the server by walking the process tree under the pane's
//! shell, because it must follow whichever herdr client runs in *that* pane.
//! WTA reads the snapshot from an ordinary CLI invocation, which is not a
//! descendant of a herdr client, so the socket is resolved from the
//! environment herdr exports into its panes (`HERDR_SOCKET_PATH`,
//! `HERDR_SESSION`) with `%APPDATA%\herdr\herdr.sock` as the default.
//!
//! Note: herdr names its pipe after the socket path *as written*, so the pipe
//! is `\\.\pipe\<socket path>` — `\\.\pipe\C:\Users\<user>\AppData\Roaming\herdr\herdr.sock`
//! for the default session.

use std::{
    collections::HashMap,
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader},
    net::windows::named_pipe::{ClientOptions, NamedPipeClient},
};
use windows_sys::Win32::Foundation::ERROR_PIPE_BUSY;

/// How long herdr may take to answer; a snapshot of a busy session is large.
const ANSWER: Duration = Duration::from_secs(5);

/// How long herdr's busy pipe may take to free, between attempts.
const BUSY: [u64; 4] = [20, 50, 100, 200];

/// The socket request for herdr's session: its focus, its panes' folders and its agents.
pub(crate) const SNAPSHOT_REQUEST: &str =
    "{\"id\":\"wta\",\"method\":\"session.snapshot\",\"params\":{}}\n";

/// The variables that place herdr's socket, as herdr's client reads them.
const VARIABLES: [&str; 6] = [
    "HERDR_SOCKET_PATH",
    "HERDR_SESSION",
    "XDG_CONFIG_HOME",
    "APPDATA",
    "USERPROFILE",
    "HOME",
];

/// `wta herdr snapshot`: print the snapshot a running herdr server reports.
pub(crate) async fn run_snapshot(
    requested: Option<PathBuf>,
    session: Option<&str>,
) -> anyhow::Result<()> {
    let socket = match requested {
        Some(socket) => socket,
        None => socket(session, &process_env())?,
    };
    let snapshot = match snapshot(&socket).await {
        Ok(snapshot) => snapshot,
        // herdr's client runs before its server listens: no pipe yet, or none
        // answering. That is not a failure of this command.
        Err(error) if is_not_running(&error) => {
            tracing::info!(socket = %socket.display(), error = %error, "herdr is not running");
            eprintln!("herdr is not running ({})", socket.display());
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    tracing::info!(
        socket = %socket.display(),
        focused_pane = ?snapshot.focused_pane_id,
        cwd = ?snapshot.focused_cwd(),
        agents = snapshot.known_agents().len(),
        "herdr snapshot read"
    );
    println!("{}", serde_json::to_string_pretty(&snapshot)?);
    Ok(())
}

/// What herdr's server shows: its focused pane's folder, and its agents.
pub(crate) async fn snapshot(socket: &Path) -> anyhow::Result<SessionSnapshot> {
    let reply = ask(socket)
        .await
        .with_context(|| format!("herdr: asking {}", socket.display()))?;
    parse_reply(&reply).with_context(|| format!("herdr: {}", socket.display()))
}

/// The line herdr's server answers with.
async fn ask(socket: &Path) -> anyhow::Result<String> {
    // herdr names its pipe after the socket's path as written, relative or not.
    let pipe = format!(r"\\.\pipe\{}", socket.to_string_lossy());
    let mut stream = connect(&pipe).await?;
    stream.write_all(SNAPSHOT_REQUEST.as_bytes()).await?;
    stream.flush().await?;
    let mut reply = String::new();
    tokio::time::timeout(ANSWER, BufReader::new(stream).read_line(&mut reply))
        .await
        .context("it does not answer in time")??;
    Ok(reply)
}

/// Opens herdr's pipe; a busy one is waited for, as herdr's own client waits.
async fn connect(pipe: &str) -> io::Result<NamedPipeClient> {
    // Tokio's client asks for identification only: the pipe's server may not
    // act as this user.
    for wait in BUSY {
        match ClientOptions::new().open(pipe) {
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                tokio::time::sleep(Duration::from_millis(wait)).await;
            }
            opened => return opened,
        }
    }
    ClientOptions::new().open(pipe)
}

/// herdr's client runs before its server listens: no pipe yet, or none answering.
pub(crate) fn is_not_running(error: &anyhow::Error) -> bool {
    error.downcast_ref::<io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
        )
    })
}

/// The variables herdr's client reads to place its socket.
pub(crate) fn process_env() -> HashMap<String, OsString> {
    VARIABLES
        .iter()
        .filter_map(|name| std::env::var_os(name).map(|value| ((*name).to_owned(), value)))
        .collect()
}

/// The socket herdr's server listens on: the named session, `HERDR_SOCKET_PATH`,
/// `HERDR_SESSION`, then the default.
pub(crate) fn socket(
    session: Option<&str>,
    env: &HashMap<String, OsString>,
) -> anyhow::Result<PathBuf> {
    let dir = match env.get("XDG_CONFIG_HOME") {
        Some(config) => PathBuf::from(config).join("herdr"),
        None => home(env)?,
    };
    let named = |name: &str| {
        if name == "default" {
            dir.join("herdr.sock")
        } else {
            dir.join("sessions").join(name).join("herdr.sock")
        }
    };
    if let Some(name) = session {
        return Ok(named(name));
    }
    if let Some(path) = env.get("HERDR_SOCKET_PATH") {
        return Ok(PathBuf::from(path));
    }
    Ok(match env.get("HERDR_SESSION") {
        Some(name) => named(&name.to_string_lossy()),
        None => dir.join("herdr.sock"),
    })
}

/// herdr's folder without `XDG_CONFIG_HOME`, as herdr finds it on Windows.
fn home(env: &HashMap<String, OsString>) -> anyhow::Result<PathBuf> {
    if let Some(data) = env.get("APPDATA") {
        return Ok(PathBuf::from(data).join("herdr"));
    }
    if let Some(profile) = env.get("USERPROFILE") {
        return Ok(PathBuf::from(profile).join(r"AppData\Roaming\herdr"));
    }
    let home = env
        .get("HOME")
        .context("herdr: its client has no APPDATA, USERPROFILE or HOME")?;
    Ok(PathBuf::from(home).join(".config/herdr"))
}

#[derive(Deserialize)]
struct Reply {
    #[serde(default)]
    result: Option<ResultBody>,
    #[serde(default)]
    error: Option<Failure>,
}

#[derive(Deserialize)]
struct ResultBody {
    snapshot: SessionSnapshot,
}

/// What herdr's `session.snapshot` reply shows.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct SessionSnapshot {
    #[serde(default)]
    pub(crate) focused_pane_id: Option<String>,
    #[serde(default)]
    pub(crate) panes: Vec<Pane>,
    /// A herdr before agents lists none.
    #[serde(default)]
    pub(crate) agents: Vec<AgentEntry>,
}

impl SessionSnapshot {
    /// The focused pane's folder, once [`parse_reply`] has checked it.
    pub(crate) fn focused_cwd(&self) -> Option<&Path> {
        let id = self.focused_pane_id.as_deref()?;
        self.panes
            .iter()
            .find(|pane| pane.pane_id == id)?
            .cwd
            .as_deref()
    }

    /// Each agent whose state herdr knows, as its pane and state. A herdr
    /// before agents lists none, and `unknown` names no state.
    pub(crate) fn known_agents(&self) -> Vec<(&str, AgentStatus)> {
        self.agents
            .iter()
            .filter(|agent| agent.agent_status != AgentStatus::Unknown)
            .map(|agent| (agent.pane_id.as_str(), agent.agent_status))
            .collect()
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Pane {
    pub(crate) pane_id: String,
    #[serde(default)]
    pub(crate) cwd: Option<PathBuf>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct AgentEntry {
    pub(crate) pane_id: String,
    #[serde(default)]
    pub(crate) agent_status: AgentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) display_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) terminal_title_stripped: Option<String>,
}

/// An agent's state, as herdr names it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
    /// A herdr whose states these are not, or an agent that names none.
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
struct Failure {
    code: String,
    message: String,
}

/// What herdr's `session.snapshot` reply shows, checked: a focused pane it lists,
/// in an absolute folder.
pub(crate) fn parse_reply(reply: &str) -> anyhow::Result<SessionSnapshot> {
    let reply: Reply = serde_json::from_str(reply).context("its reply does not parse")?;
    if let Some(Failure { code, message }) = reply.error {
        anyhow::bail!("{code}: {message}");
    }
    let snapshot = reply.result.context("its reply holds no result")?.snapshot;
    if let Some(id) = &snapshot.focused_pane_id {
        let pane = snapshot
            .panes
            .iter()
            .find(|pane| &pane.pane_id == id)
            .with_context(|| format!("its focused pane {id} is not among its panes"))?;
        if let Some(cwd) = &pane.cwd {
            anyhow::ensure!(
                cwd.is_absolute(),
                "pane {id}'s folder {} is relative",
                cwd.display()
            );
        }
    }
    Ok(snapshot)
}

#[cfg(test)]
mod tests;
