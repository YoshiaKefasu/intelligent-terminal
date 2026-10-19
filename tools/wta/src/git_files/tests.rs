//! Fixtures follow ELYTerminal's `src/git/tests.rs` and `src/git/tests/diffs.rs`:
//! the porcelain v2 records git writes, and repositories set up with real git.

use std::{env, fs};

use super::*;

/// A fresh folder under the temp dir.
fn folder(name: &str) -> PathBuf {
    let folder = env::temp_dir().join(format!("wta-git-{name}-{}", std::process::id()));
    if folder.exists() {
        fs::remove_dir_all(&folder).expect("a stale test folder goes");
    }
    fs::create_dir_all(&folder).expect("a test folder");
    folder
}

/// Runs git in `root` to set a test up.
fn prepare(root: &Path, args: &[&str]) {
    let done = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("git runs");
    let error = String::from_utf8_lossy(&done.stderr);
    assert!(done.status.success(), "git {args:?}: {error}");
}

/// A new repository on `main` whose own config keeps the user's away.
fn repository(name: &str) -> PathBuf {
    let root = folder(name);
    prepare(&root, &["init", "-q", "-b", "main"]);
    for [key, value] in [
        ["user.name", "Wta"],
        ["user.email", "wta@example.com"],
        ["commit.gpgsign", "false"],
        ["core.hooksPath", "no-hooks"],
        ["core.autocrlf", "false"],
        ["merge.conflictStyle", "merge"],
        ["status.showUntrackedFiles", "normal"],
    ] {
        prepare(&root, &["config", key, value]);
    }
    root
}

/// Writes `text` to `path` under `root`, making its folders.
fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().expect("a parent")).expect("folders");
    fs::write(path, text).expect("a file");
}

fn change(path: &str, status: GitStatus) -> Change {
    Change {
        path: path.into(),
        from: None,
        status,
    }
}

fn read(root: &Path) -> RepoChanges {
    repo(root).expect("status").expect("a work tree")
}

// ─── porcelain v2 ───────────────────────────────────────────────────────────

#[test]
fn status_records_read_with_head() {
    let out = b"# branch.oid 1a2b\0# branch.head main\0# branch.upstream origin/main\0\
        # branch.ab +1 -0\0\
        1 .M N... 100644 100644 100644 aaa bbb src/a b.rs\0\
        2 RM N... 100644 100644 100644 aaa bbb R100 new name.rs\0old name.rs\0\
        u UU N... 100644 100644 100644 100644 aaa bbb ccc clash.rs\0? notes/\0";
    let (head, records) = parse(out).expect("records");
    let main = Head {
        branch: Some("main".into()),
        commit: Some("1a2b".into()),
    };
    assert_eq!(head, main);
    let record = |x, y, path: &str, from: Option<&str>| Record {
        x,
        y,
        path: path.into(),
        from: from.map(Into::into),
    };
    let expected = [
        record(b'.', b'M', "src/a b.rs", None),
        record(b'R', b'M', "new name.rs", Some("old name.rs")),
        record(b'U', b'U', "clash.rs", None),
        record(b'?', b'?', "notes/", None),
    ];
    assert_eq!(records, expected);
    let unborn = b"# branch.oid (initial)\0# branch.head (detached)\0";
    let (head, _) = parse(unborn).expect("headers");
    assert_eq!(head, Head::default(), "no commit yet, no branch");
}

#[test]
fn records_git_never_writes_are_errors() {
    for out in [
        &b"1 .M N... 100644\0"[..],
        b"x junk\0",
        b"2 R. N... 1 2 3 a b R100 new\0",
        b"1 M N... 1 2 3 a b p\0",
        b"?\0",
        b"1 .M N... 1 2 3 a b \0",
    ] {
        assert!(parse(out).is_err(), "{:?}", String::from_utf8_lossy(out));
    }
}

#[test]
fn both_codes_combine_into_one_mark() {
    use GitStatus::*;
    for (codes, status) in [
        (b"??", Untracked),
        (b"UU", Conflicted),
        (b"AA", Conflicted),
        (b"DU", Conflicted),
        (b".D", Deleted),
        (b"RM", Renamed),
        (b"AM", Added),
        (b"C.", Added),
        (b".T", Modified),
        (b"MM", Modified),
    ] {
        let named = String::from_utf8_lossy(codes);
        assert_eq!(kind(codes[0], codes[1]), Some(status), "{named}");
    }
    assert_eq!(kind(b'.', b'.'), None);
    assert_eq!(kind(b'!', b'!'), None);
}

#[test]
fn a_path_git_writes_as_bytes_is_utf8_here() {
    assert_eq!(
        path_of(b"src/main.rs").expect("a path"),
        Path::new("src/main.rs")
    );
    assert!(path_of(b"bad\xff.txt").is_err(), "not UTF-8");
}

// ─── status against real repositories ───────────────────────────────────────

#[test]
fn a_work_tree_reports_changes_under_the_folder() {
    let root = repository("changes");
    for (path, text) in [
        ("kept.txt", "kept"),
        ("gone.txt", "gone"),
        ("src/main.rs", "fn main() {}"),
        ("src/inner/deep.rs", "deep"),
    ] {
        write(&root, path, text);
    }
    prepare(&root, &["add", "."]);
    prepare(&root, &["commit", "-qm", "first"]);
    write(&root, "src/main.rs", "fn main() { }");
    fs::remove_file(root.join("gone.txt")).expect("a removal");
    write(&root, "added.txt", "new");
    prepare(&root, &["add", "added.txt"]);
    write(&root, "notes/sub/todo.txt", "todo");

    let mut all = changes(&root).expect("status").expect("a work tree");
    all.sort_by(|a, b| a.0.cmp(&b.0));
    let expected = [
        ("added.txt", GitStatus::Added),
        ("gone.txt", GitStatus::Deleted),
        ("notes", GitStatus::Untracked),
        ("src/main.rs", GitStatus::Modified),
    ]
    .map(|(path, status)| (root.join(path), status));
    assert_eq!(all, expected);
    let src = root.join("src");
    let main = (src.join("main.rs"), GitStatus::Modified);
    assert_eq!(changes(&src).expect("status"), Some(vec![main]));
    let sub = root.join("notes").join("sub");
    let whole = (sub.clone(), GitStatus::Untracked);
    assert_eq!(
        changes(&sub).expect("status"),
        Some(vec![whole]),
        "a folder inside an untracked one is untracked whole"
    );
    assert_eq!(changes(&root.join(".git")).expect("rev-parse"), None);
    assert_eq!(changes(&folder("outside")).expect("rev-parse"), None);
}

#[test]
fn a_repository_lists_its_branch_and_each_section() {
    let root = repository("repo");
    for path in ["kept.txt", "gone.txt", "old.txt", "src/main.rs"] {
        write(&root, path, "first\n");
    }
    prepare(&root, &["add", "."]);
    prepare(&root, &["commit", "-qm", "first"]);
    write(&root, "src/main.rs", "second\n");
    fs::remove_file(root.join("gone.txt")).expect("a removal");
    write(&root, "added.txt", "new\n");
    write(&root, "kept.txt", "staged\n");
    prepare(&root, &["add", "added.txt", "kept.txt"]);
    write(&root, "kept.txt", "staged, then changed\n");
    prepare(&root, &["mv", "old.txt", "new.txt"]);
    write(&root, "notes/todo.txt", "todo\n");

    let state = read(&root.join("src"));
    let real = |path: &Path| fs::canonicalize(path).expect("a real path");
    assert_eq!(
        real(&state.top),
        real(&root),
        "the top, from a folder under it"
    );
    assert_eq!(state.branch.as_deref(), Some("main"));
    assert!(state.commit.is_some());
    let renamed = Change {
        path: "new.txt".into(),
        from: Some("old.txt".into()),
        status: GitStatus::Renamed,
    };
    let staged = [
        change("added.txt", GitStatus::Added),
        change("kept.txt", GitStatus::Modified),
        renamed,
    ];
    assert_eq!(state.staged, staged);
    let unstaged = [
        change("gone.txt", GitStatus::Deleted),
        change("kept.txt", GitStatus::Modified),
        change("src/main.rs", GitStatus::Modified),
    ];
    assert_eq!(state.unstaged, unstaged);
    assert_eq!(state.untracked, [change("notes/", GitStatus::Untracked)]);
    assert!(state.conflicted.is_empty());

    prepare(&root, &["commit", "-qm", "second"]);
    prepare(&root, &["checkout", "-q", "--detach"]);
    assert_eq!(read(&root).branch, None, "a detached HEAD has no branch");
    assert_eq!(repo(&root.join(".git")).expect("rev-parse"), None);
    assert_eq!(repo(&folder("repo-outside")).expect("rev-parse"), None);
}

#[test]
fn the_status_serializes_to_json_that_names_each_section() {
    let root = repository("json");
    write(&root, "a.txt", "a\n");
    prepare(&root, &["add", "."]);
    prepare(&root, &["commit", "-qm", "first"]);
    write(&root, "a.txt", "b\n");
    write(&root, "new.txt", "new\n");
    let printed = serde_json::to_value(read(&root)).expect("it serializes");
    assert_eq!(printed["branch"], "main");
    assert_eq!(printed["unstaged"][0]["path"], "a.txt");
    assert_eq!(printed["unstaged"][0]["status"], "modified");
    assert_eq!(printed["untracked"][0]["status"], "untracked");
    assert!(
        printed["unstaged"][0].get("from").is_none(),
        "no source to a plain change"
    );
    assert_eq!(printed["conflicted"], serde_json::json!([]));
}

// ─── log ────────────────────────────────────────────────────────────────────

#[test]
fn the_log_lists_the_newest_commits_first() {
    let root = repository("log");
    write(&root, "a.txt", "a\n");
    prepare(&root, &["add", "."]);
    prepare(&root, &["commit", "-qm", "first subject"]);
    write(&root, "b.txt", "b\n");
    prepare(&root, &["add", "."]);
    prepare(&root, &["commit", "-qm", "second"]);

    let commits = log(&root, 10).expect("a log");
    let [newest, first] = commits.as_slice() else {
        panic!("two commits: {commits:?}");
    };
    assert_eq!(newest.subject, "second");
    assert_eq!(first.subject, "first subject");
    assert_eq!(newest.parents, [first.id.clone()]);
    assert!(first.parents.is_empty());
    assert!(newest.id.chars().all(|c| c.is_ascii_hexdigit()) && !newest.id.is_empty());
    assert_eq!(newest.author, "Wta");
    assert!(newest.when.ends_with("ago"), "{}", newest.when);
    assert_eq!(newest.refs, ["HEAD -> main"]);
    assert!(first.refs.is_empty());
    assert_eq!(log(&root, 1).expect("a log").len(), 1);
    // A log with no commit to read is an error git reports, not an empty list.
    assert!(log(&folder("log-outside"), 1).is_err());
}

#[test]
fn a_repository_before_its_first_commit_has_no_log() {
    let root = repository("unborn");
    write(&root, "a.txt", "a\n");
    let state = read(&root);
    assert_eq!(state.commit, None, "no commit yet");
    assert_eq!(state.branch.as_deref(), Some("main"));
    assert!(log(&root, 10).is_err(), "git has nothing to walk");
}

// ─── diff ───────────────────────────────────────────────────────────────────

fn line(kind: LineKind, at: (Option<usize>, Option<usize>), text: &str) -> DiffLine {
    DiffLine {
        kind,
        old: at.0,
        new: at.1,
        text: text.to_string(),
    }
}

fn hunk(header: &str, lines: Vec<DiffLine>) -> Hunk {
    Hunk {
        header: header.to_string(),
        lines,
    }
}

#[test]
fn a_diff_reads_as_hunks_with_numbers() {
    use LineKind::*;
    let root = repository("diff");
    write(&root, "a.txt", "one\nthe old line\nthree\n");
    prepare(&root, &["add", "."]);
    prepare(&root, &["commit", "-qm", "first"]);
    write(&root, "a.txt", "one\nthe new line\nthree\n");
    prepare(&root, &["add", "a.txt"]);
    write(&root, "a.txt", "one\nthe new line\nthree\nfour");

    let staged = diff(&root, Path::new("a.txt"), None, ChangeSection::Staged).expect("a diff");
    let expected = hunk(
        "@@ -1,3 +1,3 @@",
        vec![
            line(Same, (Some(1), Some(1)), "one"),
            line(Removed, (Some(2), None), "the old line"),
            line(Added, (None, Some(2)), "the new line"),
            line(Same, (Some(3), Some(3)), "three"),
        ],
    );
    assert_eq!(staged, Some(vec![expected]));
    let unstaged = diff(&root, Path::new("a.txt"), None, ChangeSection::Unstaged).expect("a diff");
    let expected = hunk(
        "@@ -1,3 +1,4 @@",
        vec![
            line(Same, (Some(1), Some(1)), "one"),
            line(Same, (Some(2), Some(2)), "the new line"),
            line(Same, (Some(3), Some(3)), "three"),
            line(Added, (None, Some(4)), "four"),
        ],
    );
    assert_eq!(unstaged, Some(vec![expected]));
}

#[test]
fn an_untracked_file_diffs_against_nothing_and_a_folder_is_refused() {
    use LineKind::*;
    let root = repository("untracked");
    write(&root, "fresh.txt", "fresh\nlines\n");
    let hunks = diff(
        &root,
        Path::new("fresh.txt"),
        None,
        ChangeSection::Untracked,
    )
    .expect("a diff git exited 1 for");
    let expected = hunk(
        "@@ -0,0 +1,2 @@",
        vec![
            line(Added, (None, Some(1)), "fresh"),
            line(Added, (None, Some(2)), "lines"),
        ],
    );
    assert_eq!(hunks, Some(vec![expected]));
    let error =
        diff(&root, Path::new("notes/"), None, ChangeSection::Untracked).expect_err("a folder");
    assert!(error.to_string().contains("is a folder"), "{error}");
    assert!(
        diff(&root, Path::new("gone.txt"), None, ChangeSection::Untracked).is_err(),
        "a file that is not there"
    );
}

#[test]
fn a_file_inside_an_untracked_folder_diffs_against_nothing() {
    use LineKind::*;
    let root = repository("untracked-inside");
    write(&root, "notes/sub/todo.txt", "todo\n");
    let state = read(&root);
    assert_eq!(
        state.untracked,
        [change("notes/", GitStatus::Untracked)],
        "git lists the folder, not what it holds"
    );
    let found = find_change(&state, Path::new("notes/sub/todo.txt"), &root);
    assert!(found.is_none(), "no record names the file itself");
    let holder = untracked_holder(&state, Path::new("notes/sub/todo.txt"), &root);
    assert_eq!(
        holder.map(|change| change.path.clone()),
        Some("notes/".into())
    );
    let hunks = diff(
        &root,
        Path::new("notes/sub/todo.txt"),
        None,
        ChangeSection::Untracked,
    )
    .expect("a diff");
    let expected = hunk("@@ -0,0 +1 @@", vec![line(Added, (None, Some(1)), "todo")]);
    assert_eq!(hunks, Some(vec![expected]));
    // A file that no untracked folder holds is no change at all.
    assert!(untracked_holder(&state, Path::new("elsewhere.txt"), &root).is_none());
    assert!(is_folder(&state.untracked[0]));
    assert!(!is_folder(&change("a.txt", GitStatus::Modified)));
}

#[test]
fn a_binary_file_has_no_hunks() {
    let root = repository("binary");
    fs::write(root.join("bin.dat"), b"x\0y").expect("a binary file");
    prepare(&root, &["add", "."]);
    prepare(&root, &["commit", "-qm", "first"]);
    fs::write(root.join("bin.dat"), b"x\0z").expect("a binary edit");
    let binary = diff(&root, Path::new("bin.dat"), None, ChangeSection::Unstaged).expect("a diff");
    assert_eq!(binary, None, "a binary file has no lines");
}

#[test]
fn a_pure_rename_changes_no_line() {
    let root = repository("rename");
    write(&root, "old.txt", "kept\n");
    prepare(&root, &["add", "."]);
    prepare(&root, &["commit", "-qm", "first"]);
    prepare(&root, &["mv", "old.txt", "new.txt"]);
    let renamed = read(&root).staged;
    let moved = renamed.first().expect("a staged rename");
    assert_eq!(moved.from.as_deref(), Some(Path::new("old.txt")));
    let hunks = diff(
        &root,
        &moved.path,
        moved.from.as_deref(),
        ChangeSection::Staged,
    )
    .expect("a diff");
    assert_eq!(hunks, Some(Vec::new()), "a rename changes no line");
}

#[test]
fn a_conflict_diffs_against_head_our_side() {
    use LineKind::*;
    let root = repository("conflict");
    write(&root, "c.txt", "base\n");
    prepare(&root, &["add", "."]);
    prepare(&root, &["commit", "-qm", "base"]);
    prepare(&root, &["checkout", "-qb", "other"]);
    write(&root, "c.txt", "theirs\n");
    prepare(&root, &["commit", "-qam", "theirs"]);
    prepare(&root, &["checkout", "-q", "main"]);
    write(&root, "c.txt", "ours\n");
    prepare(&root, &["commit", "-qam", "ours"]);
    let merged = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["merge", "-q", "other"])
        .output()
        .expect("git runs");
    assert!(!merged.status.success(), "the merge stops");

    let state = read(&root);
    assert_eq!(state.conflicted, [change("c.txt", GitStatus::Conflicted)]);
    let ours = diff(&root, Path::new("c.txt"), None, ChangeSection::Conflicted).expect("a diff");
    let Some([hunk]) = ours.as_deref() else {
        panic!("one hunk: {ours:?}");
    };
    let shown: Vec<_> = hunk
        .lines
        .iter()
        .map(|line| (line.kind, line.text.as_str()))
        .collect();
    let expected = [
        (Added, "<<<<<<< HEAD"),
        (Same, "ours"),
        (Added, "======="),
        (Added, "theirs"),
        (Added, ">>>>>>> other"),
    ];
    assert_eq!(shown, expected);
}

#[test]
fn hunks_number_lines_skip_markers_and_stop_at_the_next_file() {
    use LineKind::*;
    let text = "* Unmerged path a\ndiff --git a/a b/a\n--- a/a\n+++ b/a\n\
        @@ -3 +3,2 @@ fn main\n-x\r\n+y\n+z\n\\ No newline at end of file\n\
        diff --git a/b b/b\nnew file mode 100644\n--- /dev/null\n+++ b/b\n@@ -0,0 +1 @@\n+b\n";
    let first = hunk(
        "@@ -3 +3,2 @@ fn main",
        vec![
            line(Removed, (Some(3), None), "x"),
            line(Added, (None, Some(3)), "y"),
            line(Added, (None, Some(4)), "z"),
        ],
    );
    let second = hunk("@@ -0,0 +1 @@", vec![line(Added, (None, Some(1)), "b")]);
    assert_eq!(hunks(text).expect("hunks"), Some(vec![first, second]));
    let blank = hunk(
        "@@ -1,3 +1,3 @@",
        vec![
            line(Same, (Some(1), Some(1)), "a"),
            line(Same, (Some(2), Some(2)), ""),
            line(Removed, (Some(3), None), "b"),
            line(Added, (None, Some(3)), "c"),
        ],
    );
    let suppressed = "@@ -1,3 +1,3 @@\n a\n\n-b\n+c\n";
    assert_eq!(hunks(suppressed).expect("hunks"), Some(vec![blank]));
    let binary = "diff --git a/x b/x\nBinary files a/x and b/x differ\n";
    assert_eq!(hunks(binary).expect("a diff"), None);
    assert!(hunks("@@ -a +b @@\n").is_err(), "a header without numbers");
    assert!(
        hunks("@@ -1 +1 @@\n?x\n").is_err(),
        "a line git never writes"
    );
}

#[test]
fn a_diff_runs_no_text_converter() {
    use LineKind::*;
    let root = repository("textconv");
    write(&root, "a.txt", "old\n");
    write(&root, ".gitattributes", "a.txt diff=review\n");
    prepare(&root, &["add", "."]);
    prepare(&root, &["commit", "-qm", "first"]);
    // Git dies when a converter fails, so this one would fail the diff it ran in.
    prepare(&root, &["config", "diff.review.textconv", "false"]);
    write(&root, "a.txt", "new\n");
    let expected = hunk(
        "@@ -1 +1 @@",
        vec![
            line(Removed, (Some(1), None), "old"),
            line(Added, (None, Some(1)), "new"),
        ],
    );
    let unstaged = diff(&root, Path::new("a.txt"), None, ChangeSection::Unstaged).expect("a diff");
    assert_eq!(unstaged, Some(vec![expected]));
}

#[test]
fn the_diff_serializes_its_hunks() {
    let root = repository("diff-json");
    write(&root, "a.txt", "old\n");
    prepare(&root, &["add", "."]);
    prepare(&root, &["commit", "-qm", "first"]);
    write(&root, "a.txt", "new\n");
    let hunks = diff(&root, Path::new("a.txt"), None, ChangeSection::Unstaged)
        .expect("a diff")
        .expect("text");
    let printed = serde_json::to_value(&hunks).expect("it serializes");
    assert_eq!(printed[0]["header"], "@@ -1 +1 @@");
    assert_eq!(printed[0]["lines"][0]["kind"], "removed");
    assert_eq!(printed[0]["lines"][0]["old"], 1);
    assert!(
        printed[0]["lines"][0].get("new").is_none(),
        "a removed line has no new number"
    );
    assert_eq!(printed[0]["lines"][1]["kind"], "added");
    assert_eq!(printed[0]["lines"][1]["new"], 1);
}

// ─── which file and which section a diff reads ───────────────────────────────

#[test]
fn a_named_file_is_found_by_its_path_or_by_one_relative_to_the_folder() {
    let root = repository("find");
    write(&root, "src/main.rs", "first\n");
    prepare(&root, &["add", "."]);
    prepare(&root, &["commit", "-qm", "first"]);
    write(&root, "src/main.rs", "second\n");
    let state = read(&root);
    let found = find_change(&state, Path::new("src/main.rs"), &root);
    assert_eq!(
        found.map(|(change, section)| (change.path.clone(), section)),
        Some((PathBuf::from("src/main.rs"), ChangeSection::Unstaged))
    );
    let src = root.join("src");
    let found = find_change(&state, Path::new("main.rs"), &src);
    assert_eq!(
        found.map(|(change, _)| change.path.clone()),
        Some(PathBuf::from("src/main.rs")),
        "a path relative to the folder the command reads"
    );
    let whole = root.join("src/main.rs");
    let found = find_change(&state, &whole, &root);
    assert_eq!(
        found.map(|(change, _)| change.path.clone()),
        Some(PathBuf::from("src/main.rs")),
        "a path from the work tree's top"
    );
    assert!(find_change(&state, Path::new("gone.rs"), &root).is_none());
    assert_eq!(
        candidates(&root, &root, Path::new("src/main.rs")),
        [PathBuf::from("src/main.rs")]
    );
    assert_eq!(
        top_relative(&root, &root, Path::new("src/main.rs")),
        PathBuf::from("src/main.rs")
    );
    assert_eq!(
        top_relative(&root, &src, Path::new("main.rs")),
        PathBuf::from("src/main.rs"),
        "a path from the folder the command reads"
    );
    assert_eq!(
        top_relative(&root, &root, &whole),
        PathBuf::from("src/main.rs"),
        "a path from the work tree's top"
    );
}

#[test]
fn a_section_that_holds_the_file_is_chosen_when_none_is_named() {
    let root = repository("sections");
    write(&root, "a.txt", "a\n");
    prepare(&root, &["add", "."]);
    prepare(&root, &["commit", "-qm", "first"]);
    write(&root, "a.txt", "staged\n");
    prepare(&root, &["add", "a.txt"]);
    write(&root, "a.txt", "staged, then changed\n");
    let state = read(&root);
    let found = find_change(&state, Path::new("a.txt"), &root);
    assert_eq!(
        found.map(|(_, section)| section),
        Some(ChangeSection::Staged),
        "the staged side comes first, as the panel groups it"
    );
}

// ─── the file tree ──────────────────────────────────────────────────────────

#[test]
fn folders_come_first_then_files_by_name_ignoring_case() {
    let root = folder("list");
    for name in ["Zeta", "alpha"] {
        fs::create_dir(root.join(name)).expect("a folder");
    }
    for name in ["c.txt", "B.txt", "a.txt"] {
        fs::write(root.join(name), "").expect("a file");
    }
    let listed: Vec<_> = list(&root)
        .expect("a listing")
        .into_iter()
        .map(|entry| (entry.name.to_string_lossy().into_owned(), entry.folder))
        .collect();
    let expected = [
        ("alpha", true),
        ("Zeta", true),
        ("a.txt", false),
        ("B.txt", false),
        ("c.txt", false),
    ]
    .map(|(name, folder)| (name.to_string(), folder));
    assert_eq!(listed, expected);
    assert!(list(&root.join("missing")).is_err());
}

#[test]
fn a_mark_dots_each_folder_above_it_up_to_the_root() {
    let root = Path::new("/r");
    let changes = vec![
        (
            root.join("src").join("ui").join("a.rs"),
            GitStatus::Modified,
        ),
        (root.join("gone.txt"), GitStatus::Deleted),
    ];
    let marks = Marks::of(root, changes);
    assert_eq!(marks.marks.len(), 2);
    let expected = HashSet::from([
        root.to_path_buf(),
        root.join("src"),
        root.join("src").join("ui"),
    ]);
    assert_eq!(marks.dirty, expected);
    assert!(marks.is_dirty(&root.join("src")));
    assert!(!marks.is_dirty(&root.join("notes")));
}

#[test]
fn the_tree_wears_marks_and_an_untracked_folder_marks_what_it_holds() {
    let root = repository("tree");
    write(&root, "src/main.rs", "first\n");
    prepare(&root, &["add", "."]);
    prepare(&root, &["commit", "-qm", "first"]);
    write(&root, "src/main.rs", "second\n");
    write(&root, "notes/todo.txt", "todo\n");
    let changes = changes(&root).expect("status").expect("a work tree");
    let marks = Marks::of(&root, changes);
    let node = tree_entry(&root, "r".into(), false, 2, &marks);
    assert!(node.dirty, "the root holds changes");
    fn find<'a>(node: &'a TreeEntry, name: &str) -> &'a TreeEntry {
        node.children
            .iter()
            .find(|entry| entry.name == name)
            .unwrap_or_else(|| panic!("{name} among {:?}", node.children))
    }
    let src = find(&node, "src");
    assert_eq!(src.status, None);
    assert!(src.dirty, "src holds a change");
    let main = find(src, "main.rs");
    assert_eq!(main.status, Some(GitStatus::Modified));
    assert!(!main.folder);
    let notes = find(&node, "notes");
    assert_eq!(notes.status, Some(GitStatus::Untracked));
    let todo = find(notes, "todo.txt");
    assert_eq!(
        todo.status,
        Some(GitStatus::Untracked),
        "an untracked folder marks what it holds"
    );
    // A depth of zero reads no folder.
    let top = tree_entry(&root, "r".into(), false, 0, &marks);
    assert!(top.children.is_empty());
}

#[test]
fn a_folder_that_cannot_be_read_carries_why() {
    let root = folder("unreadable");
    let missing = root.join("gone");
    let node = tree_entry(&missing, "gone".into(), false, 1, &Marks::default());
    assert!(
        node.error.is_some(),
        "a folder that is not there is unreadable"
    );
    assert!(node.children.is_empty());
}

#[test]
fn a_tree_outside_a_work_tree_wears_no_marks() {
    let root = folder("no-repo");
    fs::write(root.join("a.txt"), "").expect("a file");
    assert_eq!(changes(&root).expect("rev-parse"), None);
    let marks = Marks::of(&root, Vec::new());
    let node = tree_entry(&root, "r".into(), false, 1, &marks);
    assert!(!node.dirty);
    assert_eq!(node.children.len(), 1);
    assert_eq!(node.children[0].status, None);
}
