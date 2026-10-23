//! Fast, ACP-free discovery of Pi's **main** sessions straight off disk.
//!
//! Pi persists every conversation as one JSONL file under
//! `~/.pi/agent/sessions/<encoded-cwd>/<iso-ts>_<session-id>.jsonl`
//! (`PI_CODING_AGENT_DIR` overrides the agent dir; `settings.json`'s
//! `sessionDir` overrides the `sessions` subdirectory — both honoured here so
//! this scanner and `pi-acp`'s own `listSessions` cannot disagree about where
//! sessions live).
//!
//! Why this exists: the only other way master learns about Pi's sessions is
//! ACP `session/list`, which needs `npx -y pi-acp` to cold-start (Node.js
//! bootstrap + `initialize` handshake — seconds, sometimes a timeout). Pi is
//! excluded from background host history discovery
//! (`master::host_history_agent_ids`), so that adapter only starts when a pane
//! actually opens Pi. Reading the files directly is a handful of small reads,
//! so master seeds its registry at startup and the sessions view has rows to
//! show immediately; a later ACP listing (once Pi is explicitly opened)
//! reconciles onto the same identities (`provider_id = "pi"`, `location = Host`,
//! `session_universe = None` — the exact key
//! [`crate::session_registry::agent_session_to_session_info`] derives from an
//! ACP row, so `upsert_if_absent` cannot create duplicates).
//!
//! ## Sub-agent exclusion
//!
//! Pi writes a sub-agent conversation as its own session file whose first line
//! carries `"parentSession": "<parent file path>"`. That header field — not the
//! `<role>#<nonce>` title convention — is the authoritative sub-agent marker,
//! and a file with it is skipped outright.
//!
//! ## Deliberate limits
//!
//! * Title discovery reads a bounded head and tail window (256 KiB each) rather
//!   than the whole file. `session_info` records are appended, so the newest
//!   name is normally in the tail; a name buried mid-file in a very large
//!   session is missed and the row falls back to the first user message. This
//!   is the `Note:` tradeoff that keeps the scan in the hundreds of
//!   milliseconds instead of reading hundreds of megabytes.
//! * Only the host profile is scanned. Pi sessions inside a WSL distro are not
//!   reachable from here and remain ACP-only.

use crate::agent_sessions::{CliSource, SessionLocation, SessionOrigin};
use crate::session_registry::SessionInfo;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Bytes read from the start of a session file when looking for a title.
const HEAD_BYTES: u64 = 256 * 1024;
/// Bytes read from the end of a session file when looking for the newest
/// `session_info` name and the last activity timestamp.
const TAIL_BYTES: u64 = 256 * 1024;
/// Guard against a pathological symlink loop under the sessions root.
const MAX_DEPTH: usize = 8;
/// Guard against a runaway sessions directory (each file costs two opens).
const MAX_FILES: usize = 20_000;
/// A fallback title (the session's first user message) is truncated to this
/// many characters — it is a one-line list label, not a summary.
const TITLE_MAX_CHARS: usize = 60;

/// Scan the host Pi session store and return one row per **main** session.
///
/// Never fails: an unreadable file, a malformed header, or a missing sessions
/// root all just yield fewer rows. Safe to call from `spawn_blocking`; it does
/// synchronous file I/O only.
pub(crate) fn scan_pi_disk_sessions() -> Vec<SessionInfo> {
    match pi_sessions_root() {
        Some(root) => scan_sessions_dir(&root),
        None => Vec::new(),
    }
}

/// Root directory holding Pi's session JSONL files.
///
/// Mirrors `pi-acp`'s resolution exactly: `PI_CODING_AGENT_DIR` (absolute) or
/// `<home>/.pi/agent`, then an optional `sessionDir` override from that
/// directory's `settings.json` (relative overrides resolve against it), then
/// `sessions`.
pub(crate) fn pi_sessions_root() -> Option<PathBuf> {
    let agent_dir = std::env::var_os("PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| home_dir().map(|home| home.join(".pi").join("agent")))?;
    Some(settings_session_dir(&agent_dir).unwrap_or_else(|| agent_dir.join("sessions")))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

/// `sessionDir` from `<agent_dir>/settings.json`, resolved the way `pi-acp`
/// resolves it. `None` when absent, blank, or unparsable.
fn settings_session_dir(agent_dir: &Path) -> Option<PathBuf> {
    let raw = std::fs::read_to_string(agent_dir.join("settings.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let configured = value.get("sessionDir")?.as_str()?.trim().to_string();
    if configured.is_empty() {
        return None;
    }
    let path = PathBuf::from(configured);
    Some(if path.is_absolute() {
        path
    } else {
        agent_dir.join(path)
    })
}

/// Scan an explicit sessions root. Split out from [`scan_pi_disk_sessions`] so
/// tests can point at a fixture tree without touching the process environment.
pub(crate) fn scan_sessions_dir(root: &Path) -> Vec<SessionInfo> {
    let mut files = Vec::new();
    collect_jsonl(root, 0, &mut files);
    // Newest first: the map below keeps the newest copy of a duplicated
    // session id, and the caller only cares about the final set.
    let mut by_id: HashMap<String, SessionInfo> = HashMap::new();
    for file in files {
        let Some(row) = read_session_file(&file) else {
            continue;
        };
        let key = row.session_id.0.to_string();
        match by_id.get(&key) {
            Some(existing)
                if existing.last_activity_at_ms.unwrap_or(0)
                    >= row.last_activity_at_ms.unwrap_or(0) => {}
            _ => {
                by_id.insert(key, row);
            }
        }
    }
    by_id.into_values().collect()
}

fn collect_jsonl(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_DEPTH || out.len() >= MAX_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= MAX_FILES {
            return;
        }
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_jsonl(&path, depth + 1, out);
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            out.push(path);
        }
    }
}

/// Parse one session file into a registry row, or `None` when it is not a
/// main Pi session (sub-agent file, malformed header, unreadable).
fn read_session_file(path: &Path) -> Option<SessionInfo> {
    let header = read_header(path)?;
    let size = std::fs::metadata(path).ok()?.len();

    let tail = read_range(path, size.saturating_sub(TAIL_BYTES), TAIL_BYTES)
        .map(|bytes| decode_tail(&bytes, size <= TAIL_BYTES))
        .unwrap_or_default();
    let head = if size > TAIL_BYTES {
        read_range(path, 0, HEAD_BYTES)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default()
    } else {
        tail.clone()
    };

    let title = scan_for_name(&tail)
        .filter(|name| !crate::agent_sessions::title_is_pi_subagent(name))
        .or_else(|| {
            scan_for_name(&head).filter(|name| !crate::agent_sessions::title_is_pi_subagent(name))
        })
        .or_else(|| scan_for_first_user_message(&head))
        .unwrap_or_else(|| {
            crate::session_history::short_id(
                &header.id,
                crate::session_history::cli_label(&CliSource::Pi),
            )
        });

    let last_activity_at_ms = scan_for_last_message_timestamp(&tail)
        .or_else(|| scan_for_last_message_timestamp(&head))
        .and_then(|iso| crate::session_history::parse_iso_to_system_time(&iso))
        .or_else(|| {
            std::fs::metadata(path)
                .ok()
                .and_then(|meta| meta.modified().ok())
        })
        .and_then(|time| {
            time.duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|delta| delta.as_millis() as u64)
        });

    let mut row = SessionInfo::new(
        agent_client_protocol::schema::v1::SessionId::new(header.id),
        PathBuf::from(header.cwd),
    );
    // `provider_id` + `location` are what make this row's identity match the
    // one ACP `session/list` will later derive, and what lets reconcile and
    // resume attribute it to the Pi agent.
    row.provider_id = Some("pi".to_string());
    row.title = Some(title);
    row.status = Some(crate::agent_sessions::AgentStatus::Historical);
    row.cli_source = Some(CliSource::Pi);
    // `Unknown` (not `AgentPane`): these are conversations the user started by
    // hand, so the default `ShellOnly` sessions filter must keep them visible.
    row.origin = Some(SessionOrigin::Unknown);
    row.location = SessionLocation::Host;
    row.last_activity_at_ms = last_activity_at_ms;
    Some(row)
}

struct Header {
    id: String,
    cwd: String,
}

/// Read and validate the `{"type":"session", …}` first line.
///
/// Returns `None` for a sub-agent file (`parentSession` present) — that is the
/// authoritative exclusion, independent of how the sub-agent named itself.
fn read_header(path: &Path) -> Option<Header> {
    let file = std::fs::File::open(path).ok()?;
    let mut line = String::new();
    BufReader::new(file).read_line(&mut line).ok()?;
    let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    if value.get("type").and_then(|v| v.as_str()) != Some("session") {
        return None;
    }
    if value
        .get("parentSession")
        .and_then(|v| v.as_str())
        .is_some_and(|parent| !parent.trim().is_empty())
    {
        return None;
    }
    let id = value.get("id").and_then(|v| v.as_str())?.trim().to_string();
    let cwd = value
        .get("cwd")
        .and_then(|v| v.as_str())?
        .trim()
        .to_string();
    if id.is_empty() || cwd.is_empty() {
        return None;
    }
    Some(Header { id, cwd })
}

fn read_range(path: &Path, start: u64, len: u64) -> Option<Vec<u8>> {
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    file.take(len).read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// Decode a tail window. A non-zero start almost certainly lands mid-line, and
/// a half record would parse as garbage, so the leading partial line is dropped.
fn decode_tail(bytes: &[u8], whole_file: bool) -> String {
    let text = String::from_utf8_lossy(bytes);
    if whole_file {
        return text.into_owned();
    }
    match text.find('\n') {
        Some(index) => text[index + 1..].to_string(),
        None => String::new(),
    }
}

/// Newest `session_info` name in the window, in file order.
fn scan_for_name(text: &str) -> Option<String> {
    let mut found = None;
    for record in records(text) {
        if record.get("type").and_then(|v| v.as_str()) != Some("session_info") {
            continue;
        }
        if let Some(name) = record
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            found = Some(name.to_string());
        }
    }
    found
}

/// First user message in the window, as a one-line list label.
fn scan_for_first_user_message(text: &str) -> Option<String> {
    for record in records(text) {
        if record.get("type").and_then(|v| v.as_str()) != Some("message") {
            continue;
        }
        let message = match record.get("message") {
            Some(message) => message,
            None => continue,
        };
        if message.get("role").and_then(|v| v.as_str()) != Some("user") {
            continue;
        }
        let label = match message.get("content") {
            Some(serde_json::Value::String(raw)) => Some(collapse(raw)),
            Some(serde_json::Value::Array(blocks)) => blocks
                .iter()
                .find_map(|block| {
                    (block.get("type").and_then(|v| v.as_str()) == Some("text"))
                        .then(|| block.get("text").and_then(|v| v.as_str()))
                        .flatten()
                })
                .map(collapse),
            _ => None,
        };
        if let Some(label) = label.filter(|label| !label.is_empty()) {
            return Some(label);
        }
    }
    None
}

/// Timestamp of the last `message` record in the window. Records are appended,
/// so the last one is the session's most recent activity.
fn scan_for_last_message_timestamp(text: &str) -> Option<String> {
    records(text)
        .filter(|record| record.get("type").and_then(|v| v.as_str()) == Some("message"))
        .filter_map(|record| {
            record
                .get("timestamp")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .next_back()
}

fn records(text: &str) -> impl DoubleEndedIterator<Item = serde_json::Value> + '_ {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line.trim()).ok())
}

/// Flatten a message body into one bounded, whitespace-normalized label.
fn collapse(raw: &str) -> String {
    let head: String = raw.chars().take(4_000).collect();
    let joined = head.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() <= TITLE_MAX_CHARS {
        joined
    } else {
        joined.chars().take(TITLE_MAX_CHARS).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Removes its directory on drop so a test run leaves nothing behind.
    struct Fixture(PathBuf);

    impl Fixture {
        fn new(label: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "wta-pi-disk-{}-{}-{}",
                label,
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&dir).expect("create fixture dir");
            Self(dir)
        }

        fn write(&self, rel: &str, body: &str) -> PathBuf {
            let path = self.0.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create fixture parent");
            }
            std::fs::write(&path, body).expect("write fixture file");
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Build the session header the way Pi writes it — through `serde_json` so
    /// a Windows `cwd` is escaped correctly instead of being spliced into a raw
    /// string literal.
    fn header(id: &str, cwd: &str, parent: Option<&str>) -> String {
        let mut value = serde_json::json!({
            "type": "session",
            "version": 3,
            "id": id,
            "timestamp": "2026-10-18T14:08:58.602Z",
            "cwd": cwd,
        });
        if let Some(parent) = parent {
            value["parentSession"] = serde_json::Value::String(parent.to_string());
        }
        value.to_string()
    }

    fn user_message(text: &str) -> String {
        serde_json::json!({
            "type": "message",
            "id": "m1",
            "timestamp": "2026-10-18T14:09:09.716Z",
            "message": {
                "role": "user",
                "content": [{ "type": "text", "text": text }],
            },
        })
        .to_string()
    }

    fn lines(parts: &[String]) -> String {
        let mut out = parts.join("\n");
        out.push('\n');
        out
    }

    #[test]
    fn main_session_is_read_with_header_and_first_user_message() {
        let fx = Fixture::new("main");
        fx.write(
            "--repo--/2026-10-18T14-08-58-602Z_sess-1.jsonl",
            &lines(&[
                header("sess-1", r"D:\repo", None),
                r#"{"type":"model_change","id":"a","parentId":null}"#.to_string(),
                user_message("お疲れ様です！ 今回はセッション一覧の話です"),
            ]),
        );

        let rows = scan_sessions_dir(&fx.0);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.session_id.0.as_ref(), "sess-1");
        assert_eq!(row.cwd, PathBuf::from(r"D:\repo"));
        assert_eq!(row.provider_id.as_deref(), Some("pi"));
        assert_eq!(row.cli_source, Some(CliSource::Pi));
        assert_eq!(row.origin, Some(SessionOrigin::Unknown));
        assert_eq!(row.location, SessionLocation::Host);
        assert_eq!(
            row.status,
            Some(crate::agent_sessions::AgentStatus::Historical)
        );
        assert_eq!(
            row.title.as_deref(),
            Some("お疲れ様です！ 今回はセッション一覧の話です")
        );
        // 2026-10-18T14:09:09.716Z, from the last `message` record.
        assert_eq!(row.last_activity_at_ms, Some(1_792_332_549_716));
    }

    #[test]
    fn subagent_file_is_excluded_by_parent_session_header() {
        let fx = Fixture::new("subagent");
        let parent = r"C:\Users\u\.pi\agent\sessions\--repo--\parent.jsonl";
        fx.write(
            "--repo--/parent.jsonl",
            &lines(&[header("parent", r"D:\repo", None), user_message("hello")]),
        );
        fx.write(
            "--repo--/child.jsonl",
            &lines(&[
                header("child", r"D:\repo", Some(parent)),
                r#"{"type":"session_info","name":"executor#executor#task#68d293db"}"#.to_string(),
                user_message("You are a pi coding agent sub-agent."),
            ]),
        );

        let rows = scan_sessions_dir(&fx.0);
        assert_eq!(rows.len(), 1, "only the main session survives");
        assert_eq!(rows[0].session_id.0.as_ref(), "parent");
    }

    #[test]
    fn subagent_named_title_falls_back_instead_of_leaking() {
        // Defensive: a main session whose `session_info` name happens to be a
        // sub-agent name (no `parentSession`) must not show that name.
        let fx = Fixture::new("subagent-title");
        fx.write(
            "--repo--/s.jsonl",
            &lines(&[
                header("s1", r"D:\repo", None),
                r#"{"type":"session_info","name":"explore#explore-3#abort#fe0f5cf2"}"#.to_string(),
                user_message("why does abort happen"),
            ]),
        );

        let rows = scan_sessions_dir(&fx.0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title.as_deref(), Some("why does abort happen"));
    }

    #[test]
    fn newest_session_info_name_wins() {
        let fx = Fixture::new("rename");
        fx.write(
            "--repo--/s.jsonl",
            &lines(&[
                header("s1", r"D:\repo", None),
                r#"{"type":"session_info","name":"Old name"}"#.to_string(),
                r#"{"type":"session_info","name":"Renamed"}"#.to_string(),
            ]),
        );
        assert_eq!(
            scan_sessions_dir(&fx.0)[0].title.as_deref(),
            Some("Renamed")
        );
    }

    #[test]
    fn archived_copy_does_not_duplicate_a_session() {
        let fx = Fixture::new("archive");
        fx.write(
            "--repo--/s.jsonl",
            &lines(&[header("s1", r"D:\repo", None), user_message("live")]),
        );
        fx.write(
            "--repo--/archive/s.jsonl",
            &lines(&[header("s1", r"D:\repo", None), user_message("archived")]),
        );
        let rows = scan_sessions_dir(&fx.0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session_id.0.as_ref(), "s1");
    }

    #[test]
    fn untitled_session_falls_back_to_short_id() {
        let fx = Fixture::new("untitled");
        fx.write(
            "--repo--/s.jsonl",
            &lines(&[header("abcdef1234567890", r"D:\repo", None)]),
        );
        let rows = scan_sessions_dir(&fx.0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title.as_deref(), Some("pi abcdef12"));
    }

    #[test]
    fn malformed_header_and_unrelated_files_are_skipped() {
        let fx = Fixture::new("malformed");
        fx.write("--repo--/empty.jsonl", "");
        fx.write("--repo--/garbage.jsonl", "not json\n");
        fx.write(
            "--repo--/wrong-type.jsonl",
            "{\"type\":\"message\",\"id\":\"x\"}\n",
        );
        fx.write(
            "--repo--/no-cwd.jsonl",
            "{\"type\":\"session\",\"id\":\"x\"}\n",
        );
        fx.write("--repo--/notes.txt", "ignored");
        assert!(scan_sessions_dir(&fx.0).is_empty());
    }

    #[test]
    fn missing_root_yields_no_rows() {
        assert!(scan_sessions_dir(Path::new("this-root-does-not-exist-9f8d3c2")).is_empty());
    }

    #[test]
    fn activity_falls_back_to_file_mtime_without_message_records() {
        let fx = Fixture::new("mtime");
        fx.write(
            "--repo--/s.jsonl",
            &lines(&[header("s1", r"D:\repo", None)]),
        );
        let rows = scan_sessions_dir(&fx.0);
        assert!(
            rows[0].last_activity_at_ms.is_some_and(|ms| ms > 0),
            "mtime must stand in for a missing message timestamp"
        );
    }

    /// The whole point of seeding from disk is that the later ACP listing lands
    /// on the SAME registry identities. If the two paths derived different
    /// `HistoryRowKey`s, `upsert_if_absent` would keep both and every Pi session
    /// would appear twice.
    #[test]
    fn disk_row_identity_matches_the_acp_listing_identity() {
        let fx = Fixture::new("identity");
        fx.write(
            "--repo--/s.jsonl",
            &lines(&[header("s1", r"D:\repo", None), user_message("hi")]),
        );
        let disk = scan_sessions_dir(&fx.0).remove(0);

        let acp_row = crate::session_history::acp_session_to_agent_session(
            &agent_client_protocol::schema::v1::SessionInfo::new(
                agent_client_protocol::schema::v1::SessionId::new("s1".to_string()),
                PathBuf::from(r"D:\repo"),
            ),
            SessionLocation::Host,
            &CliSource::Pi,
        );
        let from_acp = crate::session_registry::agent_session_to_session_info(&acp_row);

        assert_eq!(
            crate::session_registry::SessionIdentity::from_info(&disk),
            crate::session_registry::SessionIdentity::from_info(&from_acp)
        );
    }
}
