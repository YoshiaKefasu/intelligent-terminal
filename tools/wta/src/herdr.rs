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
///
/// `root_pid` confines the answer to one tab: when it is given, the server is
/// asked only if that process tree holds a herdr.exe. A tab running an ordinary
/// shell therefore keeps its own folder instead of adopting whatever Space
/// another window's herdr happens to have focused.
pub(crate) async fn run_snapshot(
    requested: Option<PathBuf>,
    session: Option<&str>,
    root_pid: Option<u32>,
) -> anyhow::Result<()> {
    if let Some(root) = root_pid {
        if !tree_runs_herdr(root) {
            // Nothing to say, and no pipe opened: this is the common case for
            // every non-herdr tab, so it must stay cheap and silent.
            tracing::info!(root_pid = root, "herdr is not running under this pane");
            return Ok(());
        }
    }
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

/// The name herdr's client runs under, as Toolhelp reports it.
const HERDR_IMAGE: &str = "herdr.exe";

/// Does the process tree rooted at `root` hold a herdr.exe?
///
/// The tree is read from one Toolhelp snapshot rather than by walking parent
/// links upward: a herdr that spawned a shell (and the shell then spawned the
/// pane's process) is a descendant of the pane's root, not its ancestor, and
/// the same snapshot answers for every level at once. A `root` that has already
/// exited answers no, which is the same outcome as a shell tab.
#[cfg(windows)]
pub(crate) fn tree_runs_herdr(root: u32) -> bool {
    use std::collections::{HashMap, HashSet};

    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    if root == 0 {
        return false;
    }

    // SAFETY: the snapshot handle is owned immediately and closed on drop; the
    // entry buffer is ours and `dwSize` is set before the first call, as
    // Toolhelp requires.
    let processes = unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            // Without a snapshot the tree is unknown. Answering "no" keeps the
            // tab's own folder, which is the safe half of the guess.
            tracing::debug!("herdr: no process snapshot, assuming none");
            return false;
        }
        let snapshot = std::os::windows::io::OwnedHandle::from_raw_handle(snapshot);

        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found: HashMap<u32, (u32, String)> = HashMap::new();
        let mut more = Process32FirstW(snapshot.as_raw_handle(), &mut entry);
        while more != 0 {
            let name_len = entry
                .szExeFile
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..name_len]);
            found.insert(entry.th32ProcessID, (entry.th32ParentProcessID, name));
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            more = Process32NextW(snapshot.as_raw_handle(), &mut entry);
        }
        found
    };

    // Breadth-first over children. `seen` also bounds the walk: a recycled PID
    // can make the parent links circular, and following one would loop forever.
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, (parent, _)) in &processes {
        children.entry(*parent).or_default().push(*pid);
    }
    let mut seen: HashSet<u32> = HashSet::new();
    let mut pending = vec![root];
    while let Some(pid) = pending.pop() {
        if !seen.insert(pid) {
            continue;
        }
        if let Some((_, name)) = processes.get(&pid) {
            if name.eq_ignore_ascii_case(HERDR_IMAGE) {
                return true;
            }
        }
        if let Some(kids) = children.get(&pid) {
            pending.extend(kids.iter().copied());
        }
    }
    false
}

/// A tree walk needs a real process table, which only Windows has here.
#[cfg(not(windows))]
pub(crate) fn tree_runs_herdr(_root: u32) -> bool {
    false
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
