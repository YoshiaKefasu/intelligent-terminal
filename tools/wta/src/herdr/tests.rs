//! Fixtures follow ELYTerminal's `src/term/herdr/tests.rs`: the reply shapes
//! herdr 0.9 answers with, and the socket each client shape resolves.

use super::*;

/// A root that makes a path absolute here.
const ROOT: &str = if cfg!(windows) { "C:/" } else { "/" };

fn env(pairs: &[(&str, &str)]) -> HashMap<String, OsString> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
        .collect()
}

fn reply(focused_pane: &str, cwd: &str) -> String {
    format!(
        r#"{{"id":"wta","result":{{"type":"session_snapshot","snapshot":{{"version":"0.9.5","protocol":22,"focused_pane_id":{focused_pane},"workspaces":[],"tabs":[],"panes":[{{"pane_id":"w1:p1","cwd":"/a","focused":false}},{{"pane_id":"w1:p2","cwd":{cwd},"focused":true}}],"layouts":[],"agents":[]}}}}}}"#
    )
}

#[test]
fn the_default_session_sits_in_app_data() {
    let data = env(&[("APPDATA", r"C:\Users\u\AppData\Roaming")]);
    assert_eq!(
        socket(None, &data).expect("a socket"),
        PathBuf::from(r"C:\Users\u\AppData\Roaming\herdr\herdr.sock")
    );
}

#[test]
fn a_named_session_sits_under_sessions() {
    let data = env(&[("APPDATA", r"C:\Users\u\AppData\Roaming")]);
    let work = PathBuf::from(r"C:\Users\u\AppData\Roaming\herdr\sessions\work\herdr.sock");
    assert_eq!(socket(Some("work"), &data).expect("a socket"), work);
    // `default` is the unnamed one, not a folder called default.
    assert_eq!(
        socket(Some("default"), &data).expect("a socket"),
        PathBuf::from(r"C:\Users\u\AppData\Roaming\herdr\herdr.sock")
    );
}

#[test]
fn the_environment_places_the_socket_before_the_default() {
    let exported = env(&[
        ("APPDATA", r"C:\Users\u\AppData\Roaming"),
        ("HERDR_SOCKET_PATH", r"C:\D\herdr.sock"),
    ]);
    assert_eq!(
        socket(None, &exported).expect("a socket"),
        PathBuf::from(r"C:\D\herdr.sock")
    );
    // An explicit --session outranks an exported path, as herdr's client reads it.
    assert_eq!(
        socket(Some("work"), &exported).expect("a socket"),
        PathBuf::from(r"C:\Users\u\AppData\Roaming\herdr\sessions\work\herdr.sock")
    );
    let named = env(&[
        ("APPDATA", r"C:\Users\u\AppData\Roaming"),
        ("HERDR_SESSION", "work"),
    ]);
    assert_eq!(
        socket(None, &named).expect("a socket"),
        PathBuf::from(r"C:\Users\u\AppData\Roaming\herdr\sessions\work\herdr.sock")
    );
    let config = env(&[("XDG_CONFIG_HOME", r"C:\x"), ("APPDATA", r"C:\D")]);
    assert_eq!(
        socket(None, &config).expect("a socket"),
        PathBuf::from(r"C:\x\herdr\herdr.sock")
    );
    let profile = env(&[("USERPROFILE", r"C:\Users\u")]);
    assert_eq!(
        socket(None, &profile).expect("a socket"),
        PathBuf::from(r"C:\Users\u\AppData\Roaming\herdr\herdr.sock")
    );
    let home = env(&[("HOME", r"C:\h")]);
    assert_eq!(
        socket(None, &home).expect("a socket"),
        PathBuf::from(r"C:\h\.config\herdr\herdr.sock")
    );
    assert!(socket(None, &env(&[])).is_err());
}

#[test]
fn the_focused_pane_s_folder_comes_from_the_snapshot() {
    let read = |focused_pane: &str, cwd: &str| {
        parse_reply(&reply(focused_pane, cwd))
            .map(|snapshot| snapshot.focused_cwd().map(Path::to_path_buf))
    };
    let folder = format!("{ROOT}srv/b");
    assert_eq!(
        read(r#""w1:p2""#, &format!("\"{folder}\"")).expect("a folder"),
        Some(folder.clone().into())
    );
    assert_eq!(
        read("null", &format!("\"{folder}\"")).expect("no focus"),
        None
    );
    assert_eq!(read(r#""w1:p2""#, "null").expect("no folder"), None);
    let error = read(r#""w1:p9""#, r#""/b""#).expect_err("a lost pane");
    assert!(error.to_string().contains("w1:p9"), "{error}");
    let error = read(r#""w1:p2""#, r#""b""#).expect_err("a relative folder");
    assert!(error.to_string().contains("relative"), "{error}");
    let failed = r#"{"id":"wta","error":{"code":"invalid_request","message":"no such method"}}"#;
    let error = parse_reply(failed).expect_err("an error");
    assert_eq!(error.to_string(), "invalid_request: no such method");
    assert!(parse_reply("").is_err());
}

#[test]
fn agents_whose_state_herdr_knows_are_listed_by_pane() {
    let agent = |pane: &str, status: &str, more: &str| {
        format!(
            r#"{{"terminal_id":"t","agent_status":"{status}","workspace_id":"w1","tab_id":"w1:t1","pane_id":"{pane}","focused":false,"revision":1{more}}}"#
        )
    };
    let agents = [
        agent(
            "w1:p1",
            "working",
            r#","agent":"codex","display_agent":"Codex","name":"review","title":"codex","terminal_title_stripped":"Reading files""#,
        ),
        agent(
            "w1:p2",
            "blocked",
            r#","agent":"claude","display_agent":"Claude Code","title":"claude""#,
        ),
        agent("w1:p3", "unknown", ""),
        agent("w1:p4", "napping", ""),
        agent("w1:p5", "done", ""),
        agent("w1:p6", "idle", ""),
    ];
    let reply = reply("null", "null").replace(
        r#""agents":[]"#,
        &format!(r#""agents":[{}]"#, agents.join(",")),
    );
    let snapshot = parse_reply(&reply).expect("a snapshot");
    assert_eq!(snapshot.focused_cwd(), None);
    assert_eq!(
        snapshot.known_agents(),
        [
            ("w1:p1", AgentStatus::Working),
            ("w1:p2", AgentStatus::Blocked),
            ("w1:p5", AgentStatus::Done),
            ("w1:p6", AgentStatus::Idle),
        ]
    );
    let first = &snapshot.agents[0];
    assert_eq!(first.agent.as_deref(), Some("codex"));
    assert_eq!(first.name.as_deref(), Some("review"));
    assert_eq!(
        first.terminal_title_stripped.as_deref(),
        Some("Reading files")
    );
    let second = &snapshot.agents[1];
    assert_eq!(second.title.as_deref(), Some("claude"));
    assert_eq!(second.display_agent.as_deref(), Some("Claude Code"));

    // A herdr before agents omits the field entirely.
    let older = reply.replace(&format!(r#","agents":[{}]"#, agents.join(",")), "");
    assert!(parse_reply(&older)
        .expect("a herdr before agents")
        .agents
        .is_empty());
}

#[test]
fn only_a_missing_or_refused_pipe_counts_as_herdr_not_running() {
    let missing = anyhow::Error::from(io::Error::from(io::ErrorKind::NotFound))
        .context("herdr: asking C:/D/herdr.sock");
    assert!(is_not_running(&missing));
    let refused = anyhow::Error::from(io::Error::from(io::ErrorKind::ConnectionRefused));
    assert!(is_not_running(&refused));
    let denied = anyhow::Error::from(io::Error::from(io::ErrorKind::PermissionDenied));
    assert!(!is_not_running(&denied));
    assert!(!is_not_running(&anyhow::anyhow!(
        "its reply does not parse"
    )));
}

#[test]
fn the_snapshot_serializes_back_to_herdr_s_shape() {
    let snapshot = parse_reply(&reply(r#""w1:p2""#, r#""C:/srv""#)).expect("a snapshot");
    let printed = serde_json::to_value(&snapshot).expect("it serializes");
    assert_eq!(printed["focused_pane_id"], "w1:p2");
    assert_eq!(printed["panes"][1]["cwd"], "C:/srv");
    assert_eq!(printed["agents"], serde_json::json!([]));
}

/// herdr answers over its pipe: the request on the wire, the reply read back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn herdr_answers_over_its_pipe() {
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::windows::named_pipe::ServerOptions,
    };

    let socket = PathBuf::from(format!(r"C:\wta-herdr-{}\herdr.sock", std::process::id()));
    let pipe = format!(r"\\.\pipe\{}", socket.display());
    let server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&pipe)
        .expect("a pipe");
    let answer = reply(r#""w1:p2""#, r#""C:/srv""#);
    let served = tokio::spawn(async move {
        server.connect().await.expect("a client");
        let (reader, mut writer) = tokio::io::split(server);
        let mut request = String::new();
        BufReader::new(reader)
            .read_line(&mut request)
            .await
            .expect("a request");
        writer
            .write_all(format!("{answer}\n").as_bytes())
            .await
            .expect("a reply");
        writer.flush().await.expect("the reply goes");
        request
    });

    let read = snapshot(&socket).await.expect("an answer");
    assert_eq!(
        read.focused_cwd().map(Path::to_path_buf),
        Some(PathBuf::from("C:/srv"))
    );
    // The server's pipe closes with the task, so the next ask finds none.
    assert_eq!(served.await.expect("the server"), SNAPSHOT_REQUEST);
    let error = snapshot(&socket).await.expect_err("no server");
    assert!(format!("{error:#}").contains("asking"), "{error:#}");
    assert!(is_not_running(&error), "{error:#}");
}
