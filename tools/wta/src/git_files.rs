//! Git status, log and diff, plus the file tree with its Git marks, for a
//! folder.
//!
//! Ported from ELYTerminal's `src/git.rs`, `src/git/diff.rs` and
//! `src/files.rs`. The git invocations (English messages, literal pathspecs, no
//! optional locks, no console window), the porcelain v2 and unified-diff
//! parsing, the folder listing and the mark propagation are ELY's. What stays
//! behind is ELY's GPUI rendering (tree nodes, tones, icons) and the word-level
//! diff marks, which a JSON consumer does not need.
//!
//! Note: ELY's `path_of` has a Unix branch that keeps non-UTF-8 names as bytes.
//! WTA runs on Windows only, where git writes paths as UTF-8, so the port keeps
//! the UTF-8 form.

use std::{
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use anyhow::{anyhow, bail, ensure, Context as _};
use serde::Serialize;

// ─── git processes ──────────────────────────────────────────────────────────

/// Git's arguments on one line, for messages.
fn line<A: AsRef<OsStr>>(args: &[A]) -> String {
    let args: Vec<_> = args
        .iter()
        .map(|arg| arg.as_ref().to_string_lossy())
        .collect();
    args.join(" ")
}

/// Keeps Windows from opening a console window for git.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Runs git in `folder`: English messages, literal paths, no lock the user's own
/// git could meet; `input` feeds stdin.
fn git<A: AsRef<OsStr>>(folder: &Path, args: &[A], input: &[u8]) -> anyhow::Result<Output> {
    let named = || format!("git: `git {}` in {}", line(args), folder.display());
    let mut command = Command::new("git");
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut command, CREATE_NO_WINDOW);
    let mut child = command
        .arg("-C")
        .arg(folder)
        .args(["--no-optional-locks", "--literal-pathspecs"])
        .args(args)
        .env("LC_ALL", "C")
        .stdin(if input.is_empty() {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("{} did not start", named()))?;
    let wrote = child
        .stdin
        .take()
        .map_or(Ok(()), |mut stdin| stdin.write_all(input));
    let output = child
        .wait_with_output()
        .with_context(|| format!("{} did not end", named()))?;
    if output.status.success() {
        wrote.with_context(|| format!("{} took no input", named()))?;
    }
    Ok(output)
}

/// What git said about a failure: its errors, else its output, else its exit status.
fn said(output: &Output) -> String {
    [&output.stderr, &output.stdout]
        .into_iter()
        .map(|text| String::from_utf8_lossy(text).trim().to_string())
        .find(|text| !text.is_empty())
        .unwrap_or_else(|| output.status.to_string())
}

/// Logs what git warned of while it still did its work.
fn warnings<A: AsRef<OsStr>>(folder: &Path, args: &[A], output: &Output) {
    let warned = String::from_utf8_lossy(&output.stderr);
    if !warned.trim().is_empty() {
        tracing::warn!(
            target: "git_files",
            command = %line(args),
            folder = %folder.display(),
            warning = warned.trim(),
            "git warned while it worked"
        );
    }
}

/// Runs git and gives what it printed; a failure is an error carrying what git said.
fn run<A: AsRef<OsStr>>(folder: &Path, args: &[A], input: &[u8]) -> anyhow::Result<Vec<u8>> {
    let output = git(folder, args, input)?;
    let (command, place) = (line(args), folder.display());
    ensure!(
        output.status.success(),
        "git: `git {command}` failed in {place}: {}",
        said(&output)
    );
    warnings(folder, args, &output);
    Ok(output.stdout)
}

/// A path from git's bytes: UTF-8, as git writes them on Windows.
fn path_of(bytes: &[u8]) -> anyhow::Result<PathBuf> {
    let path = std::str::from_utf8(bytes).with_context(|| {
        let shown = String::from_utf8_lossy(bytes);
        format!("git: path {shown:?} is not UTF-8")
    })?;
    Ok(PathBuf::from(path))
}

// ─── status ─────────────────────────────────────────────────────────────────

/// How a change stands in the work tree, as the Git panel groups it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum GitStatus {
    Modified,
    Added,
    Deleted,
    Untracked,
    Renamed,
    Conflicted,
}

/// A record of `git status --porcelain=v2 -z` in git's bytes: index and work
/// tree codes, its path from the top, a rename's or copy's old path.
#[derive(Debug, PartialEq)]
struct Record {
    x: u8,
    y: u8,
    path: Vec<u8>,
    from: Option<Vec<u8>>,
}

/// HEAD as status reads it: its branch, none when detached; its commit, none
/// before the first.
#[derive(Debug, Default, PartialEq)]
struct Head {
    branch: Option<String>,
    commit: Option<String>,
}

/// Porcelain v2 records, each ended by NUL: headers name HEAD; a rename's or
/// copy's old path follows it.
fn parse(out: &[u8]) -> anyhow::Result<(Head, Vec<Record>)> {
    let mut fields = out
        .split(|&byte| byte == 0)
        .filter(|field| !field.is_empty());
    let (mut head, mut records) = (Head::default(), Vec::new());
    while let Some(field) = fields.next() {
        let malformed = || {
            let shown = String::from_utf8_lossy(field);
            anyhow!("git: status record {shown:?} is malformed")
        };
        let mut words = field.splitn(2, |&byte| byte == b' ');
        let (Some(kind), Some(rest)) = (words.next(), words.next()) else {
            return Err(malformed());
        };
        // Fields before the path, which may hold spaces.
        let count = match kind {
            b"#" => {
                match String::from_utf8_lossy(rest).split_once(' ') {
                    Some(("branch.oid", oid)) => {
                        head.commit = (oid != "(initial)").then(|| oid.into())
                    }
                    Some(("branch.head", name)) => {
                        head.branch = (name != "(detached)").then(|| name.into())
                    }
                    _ => {}
                }
                continue;
            }
            b"?" => 0,
            b"1" => 7,
            b"2" => 8,
            b"u" => 9,
            _ => return Err(malformed()),
        };
        let parts: Vec<&[u8]> = rest.splitn(count + 1, |&byte| byte == b' ').collect();
        let (codes, path) = match (kind, parts.as_slice()) {
            (b"?", [path]) => (b"??".as_slice(), *path),
            (_, [codes, .., path]) if parts.len() == count + 1 => (*codes, *path),
            _ => return Err(malformed()),
        };
        let (&[x, y], false) = (codes, path.is_empty()) else {
            return Err(malformed());
        };
        let from = match kind {
            b"2" => Some(fields.next().ok_or_else(malformed)?.to_vec()),
            _ => None,
        };
        let path = path.to_vec();
        records.push(Record { x, y, path, from });
    }
    Ok((head, records))
}

/// One mark for an index and a work tree code, `.` for none; a copy is a new file.
fn kind(x: u8, y: u8) -> Option<GitStatus> {
    Some(match (x, y) {
        (b'?', b'?') => GitStatus::Untracked,
        (b'U', _) | (_, b'U') | (b'A', b'A') | (b'D', b'D') => GitStatus::Conflicted,
        (b'D', _) | (_, b'D') => GitStatus::Deleted,
        (b'R', _) | (_, b'R') => GitStatus::Renamed,
        (b'A' | b'C', _) | (_, b'A' | b'C') => GitStatus::Added,
        (b'M' | b'T', _) | (_, b'M' | b'T') => GitStatus::Modified,
        _ => return None,
    })
}

/// A record's mark from `x` and `y`; codes git never writes are an error.
fn mark(record: &Record, x: u8, y: u8) -> anyhow::Result<GitStatus> {
    let (codes, path) = (
        format!("{}{}", x as char, y as char),
        String::from_utf8_lossy(&record.path),
    );
    kind(x, y).with_context(|| format!("git: {path} has codes {codes}"))
}

/// Each change at or under `folder`, by its full path; none outside a work tree.
pub(crate) fn changes(folder: &Path) -> anyhow::Result<Option<Vec<(PathBuf, GitStatus)>>> {
    let place = ["rev-parse", "--is-inside-work-tree", "--show-prefix"];
    let place = git(folder, &place, &[])?;
    if !place.status.success() {
        let error = said(&place);
        if error.contains("not a git repository") {
            return Ok(None);
        }
        bail!("git: rev-parse in {} failed: {error}", folder.display());
    }
    // The prefix may hold newlines of its own.
    let answer = place.stdout.strip_suffix(b"\n").and_then(|out| {
        let at = out.iter().position(|&byte| byte == b'\n')?;
        Some((&out[..at], &out[at + 1..]))
    });
    let prefix = match answer {
        Some((b"true", prefix)) => prefix,
        Some((b"false", _)) => return Ok(None),
        _ => {
            let printed = String::from_utf8_lossy(&place.stdout);
            bail!("git: rev-parse in {} printed {printed:?}", folder.display())
        }
    };
    let status = ["status", "--porcelain=v2", "-z", "--", "."];
    let (_, records) = parse(&run(folder, &status, &[])?)?;
    records
        .into_iter()
        .map(|record| {
            let status = mark(&record, record.x, record.y)?;
            let Some(rest) = record.path.strip_prefix(prefix) else {
                let path = String::from_utf8_lossy(&record.path);
                let prefix = String::from_utf8_lossy(prefix);
                bail!("git: {path} lies outside {prefix}");
            };
            let mut path = folder.to_path_buf();
            for part in rest.split(|&byte| byte == b'/') {
                if !part.is_empty() {
                    path.push(path_of(part)?);
                }
            }
            Ok((path, status))
        })
        .collect::<anyhow::Result<_>>()
        .map(Some)
}

/// A changed file, by its path from the work tree's top; a staged rename keeps
/// its old path.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Change {
    pub path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<PathBuf>,
    pub status: GitStatus,
}

/// A work tree as the Git panel shows it.
#[derive(Debug, PartialEq, Serialize)]
pub(crate) struct RepoChanges {
    /// The work tree's top folder.
    pub top: PathBuf,
    /// The branch checked out; none when HEAD is detached.
    pub branch: Option<String>,
    /// HEAD's commit; none before the first.
    pub commit: Option<String>,
    pub conflicted: Vec<Change>,
    pub staged: Vec<Change>,
    pub unstaged: Vec<Change>,
    pub untracked: Vec<Change>,
}

/// The work tree holding `folder`, read whole; none outside one.
pub(crate) fn repo(folder: &Path) -> anyhow::Result<Option<RepoChanges>> {
    let answer = git(folder, &["rev-parse", "--show-toplevel"], &[])?;
    if !answer.status.success() {
        let error = said(&answer);
        if error.contains("not a git repository") || error.contains("must be run in a work tree") {
            return Ok(None);
        }
        bail!("git: rev-parse in {} failed: {error}", folder.display());
    }
    let top = answer.stdout.strip_suffix(b"\n").with_context(|| {
        let printed = String::from_utf8_lossy(&answer.stdout);
        format!("git: rev-parse in {} printed {printed:?}", folder.display())
    })?;
    let top = path_of(top)?;
    let status = ["status", "--porcelain=v2", "--branch", "-z"];
    let (head, records) = parse(&run(&top, &status, &[])?)?;
    let mut repo = RepoChanges {
        top,
        branch: head.branch,
        commit: head.commit,
        conflicted: Vec::new(),
        staged: Vec::new(),
        unstaged: Vec::new(),
        untracked: Vec::new(),
    };
    for record in records {
        let path = path_of(&record.path)?;
        let change = |status, from| Change {
            path: path.clone(),
            from,
            status,
        };
        match mark(&record, record.x, record.y)? {
            GitStatus::Untracked => repo.untracked.push(change(GitStatus::Untracked, None)),
            GitStatus::Conflicted => repo.conflicted.push(change(GitStatus::Conflicted, None)),
            _ => {
                if record.x != b'.' {
                    // A copy's source is no part of it.
                    let from = match (record.x, &record.from) {
                        (b'R', Some(from)) => Some(path_of(from)?),
                        _ => None,
                    };
                    repo.staged
                        .push(change(mark(&record, record.x, b'.')?, from));
                }
                if record.y != b'.' {
                    repo.unstaged
                        .push(change(mark(&record, b'.', record.y)?, None));
                }
            }
        }
    }
    Ok(Some(repo))
}

// ─── log ────────────────────────────────────────────────────────────────────

/// A commit as the log lists it.
#[derive(Debug, PartialEq, Serialize)]
pub(crate) struct Commit {
    pub id: String,
    pub parents: Vec<String>,
    pub subject: String,
    pub author: String,
    /// How long ago it was made, as git words it.
    pub when: String,
    pub refs: Vec<String>,
}

/// The newest `count` commits from HEAD, which must have one.
pub(crate) fn log(top: &Path, count: usize) -> anyhow::Result<Vec<Commit>> {
    let count = format!("-n{count}");
    let format = "--format=%H%x00%P%x00%s%x00%an%x00%ar%x00%D";
    let args = ["log", &count, "-z", "--no-show-signature", format];
    let out = run(top, &args, &[])?;
    let out = String::from_utf8_lossy(&out);
    let fields: Vec<&str> = out.split_terminator('\0').collect();
    ensure!(
        fields.len().is_multiple_of(6),
        "git: log in {} printed {} fields, not six for each commit",
        top.display(),
        fields.len()
    );
    let listed = |text: &str, by: &str| -> Vec<String> {
        text.split(by)
            .filter(|part| !part.is_empty())
            .map(str::to_string)
            .collect()
    };
    let commits = fields.chunks(6).map(|commit| Commit {
        id: commit[0].to_string(),
        parents: listed(commit[1], " "),
        subject: commit[2].to_string(),
        author: commit[3].to_string(),
        when: commit[4].to_string(),
        refs: listed(commit[5], ", "),
    });
    Ok(commits.collect())
}

// ─── diff ───────────────────────────────────────────────────────────────────

/// Which side of a change a diff reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ChangeSection {
    Conflicted,
    Staged,
    Unstaged,
    Untracked,
}

/// What a diff line is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LineKind {
    Same,
    Added,
    Removed,
}

/// A diff line: its kind, numbers from one, and text.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct DiffLine {
    pub kind: LineKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new: Option<usize>,
    pub text: String,
}

/// A hunk of changes in context.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Hunk {
    /// `@@ -old +new @@`, from one.
    pub header: String,
    pub lines: Vec<DiffLine>,
}

/// `change`'s diff as `section` holds it; none for a binary file.
pub(crate) fn diff(
    top: &Path,
    path: &Path,
    from: Option<&Path>,
    section: ChangeSection,
) -> anyhow::Result<Option<Vec<Hunk>>> {
    let (named, shown) = (path.as_os_str(), path.display());
    let flags: &[&str] = match section {
        ChangeSection::Staged => &["--cached", "-M", "--"],
        ChangeSection::Unstaged => &["--"],
        // HEAD is our side: a file we deleted reads as new.
        ChangeSection::Conflicted => &["HEAD", "--"],
        ChangeSection::Untracked if named.as_encoded_bytes().ends_with(b"/") => {
            bail!("git: {shown} is a folder")
        }
        ChangeSection::Untracked => &["--no-index", "--", "/dev/null"],
    };
    let mut args: Vec<&OsStr> = ["diff", "--no-color", "--no-ext-diff", "--no-textconv"]
        .into_iter()
        .chain(flags.iter().copied())
        .map(OsStr::new)
        .collect();
    args.push(named);
    args.extend(from.map(Path::as_os_str));
    let output = git(top, &args, &[])?;
    // Without an index git exits 1 for a difference and a failure alike; only a
    // difference prints.
    let differ = section == ChangeSection::Untracked
        && output.status.code() == Some(1)
        && !output.stdout.is_empty();
    ensure!(
        output.status.success() || differ,
        "git: `git {}` failed in {}: {}",
        line(&args),
        top.display(),
        said(&output)
    );
    warnings(top, &args, &output);
    hunks(&String::from_utf8_lossy(&output.stdout))
}

/// A hunk header's first old and new line numbers.
fn starts(header: &str) -> Option<(usize, usize)> {
    let mut ranges = header.strip_prefix("@@ -")?.split(' ');
    let first = |range: &str| range.split(',').next()?.parse().ok();
    let old = first(ranges.next()?)?;
    let new = first(ranges.next()?.strip_prefix('+')?)?;
    Some((old, new))
}

/// Git's unified diff as hunks; none for a binary file.
pub(crate) fn hunks(text: &str) -> anyhow::Result<Option<Vec<Hunk>>> {
    let (mut hunks, mut at, mut inside) = (Vec::new(), (0, 0), false);
    for line in text.lines() {
        if line.starts_with("diff ") {
            inside = false;
        } else if line.starts_with("@@") {
            at = starts(line).with_context(|| format!("git: hunk header {line:?}"))?;
            hunks.push(Hunk {
                header: line.to_string(),
                lines: Vec::new(),
            });
            inside = true;
        } else if !inside {
            if line.starts_with("Binary files ") {
                return Ok(None);
            }
        } else if let Some(hunk) = hunks.last_mut() {
            let (kind, text) = match line.as_bytes().first() {
                // `diff.suppressBlankEmpty` drops a blank line's space.
                None => (LineKind::Same, ""),
                Some(b' ') => (LineKind::Same, &line[1..]),
                Some(b'-') => (LineKind::Removed, &line[1..]),
                Some(b'+') => (LineKind::Added, &line[1..]),
                Some(b'\\') => continue,
                Some(_) => bail!("git: diff line {line:?} is malformed"),
            };
            let old = (kind != LineKind::Added).then_some(at.0);
            let new = (kind != LineKind::Removed).then_some(at.1);
            at = (
                at.0 + usize::from(old.is_some()),
                at.1 + usize::from(new.is_some()),
            );
            hunk.lines.push(DiffLine {
                kind,
                old,
                new,
                text: text.to_string(),
            });
        }
    }
    Ok(Some(hunks))
}

// ─── the file tree ──────────────────────────────────────────────────────────

/// A folder's entry: its name, and whether it is a folder, links followed.
#[derive(Debug, PartialEq)]
struct Entry {
    name: OsString,
    folder: bool,
}

/// A folder's entries, or why it could not be read.
type Listing = Result<Vec<Entry>, String>;

/// Reads `folder`: folders first, then files, each by name ignoring case.
fn list(folder: &Path) -> Listing {
    let read = || -> io::Result<Vec<Entry>> {
        fs::read_dir(folder)?
            .map(|entry| {
                let entry = entry?;
                let kind = entry.file_type()?;
                Ok(Entry {
                    folder: kind.is_dir() || (kind.is_symlink() && entry.path().is_dir()),
                    name: entry.file_name(),
                })
            })
            .collect()
    };
    let mut entries = read().map_err(|error| error.to_string())?;
    entries.sort_by_cached_key(|entry| {
        let name = entry.name.to_string_lossy().to_lowercase();
        (!entry.folder, name, entry.name.clone())
    });
    Ok(entries)
}

/// Changes by their paths, and the folders from `root` down that hold one.
#[derive(Debug, Default)]
struct Marks {
    marks: HashMap<PathBuf, GitStatus>,
    dirty: HashSet<PathBuf>,
}

impl Marks {
    fn of(root: &Path, changes: Vec<(PathBuf, GitStatus)>) -> Self {
        let marks: HashMap<PathBuf, GitStatus> = changes.into_iter().collect();
        let dirty = marks
            .keys()
            .flat_map(|path| {
                let up = path.ancestors().skip(1);
                up.take_while(|folder| folder.starts_with(root))
            })
            .map(Path::to_path_buf)
            .collect();
        Self { marks, dirty }
    }

    /// The mark git put on `path` itself.
    fn own(&self, path: &Path) -> Option<GitStatus> {
        self.marks.get(path).copied()
    }

    /// Whether a change sits below `path`.
    fn is_dirty(&self, path: &Path) -> bool {
        self.dirty.contains(path)
    }
}

/// A folder's entry as the Files panel shows it, and as this CLI prints it.
#[derive(Debug, Serialize)]
pub(crate) struct TreeEntry {
    pub name: String,
    pub path: PathBuf,
    pub folder: bool,
    /// Git's mark on the entry itself; a folder inside an untracked one is untracked whole.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<GitStatus>,
    /// A folder that holds a change below it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub dirty: bool,
    /// Why a folder could not be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<TreeEntry>,
}

/// `path`'s node, its entries read `depth` levels deeper; `untracked` when an
/// untracked folder holds it.
fn tree_entry(
    path: &Path,
    name: String,
    untracked: bool,
    depth: usize,
    marks: &Marks,
) -> TreeEntry {
    let own = marks
        .own(path)
        .or(untracked.then_some(GitStatus::Untracked));
    let inside = own == Some(GitStatus::Untracked);
    let mut node = TreeEntry {
        name,
        path: path.to_path_buf(),
        folder: true,
        status: own,
        dirty: marks.is_dirty(path),
        error: None,
        children: Vec::new(),
    };
    if depth == 0 {
        return node;
    }
    match list(path) {
        Err(error) => node.error = Some(error),
        Ok(entries) => {
            node.children = entries
                .into_iter()
                .map(|entry| {
                    let child = path.join(&entry.name);
                    let label = entry.name.to_string_lossy().into_owned();
                    if entry.folder {
                        return tree_entry(&child, label, inside, depth - 1, marks);
                    }
                    TreeEntry {
                        name: label,
                        status: marks.own(&child).or(inside.then_some(GitStatus::Untracked)),
                        path: child,
                        folder: false,
                        dirty: false,
                        error: None,
                        children: Vec::new(),
                    }
                })
                .collect();
        }
    }
    node
}

/// A folder as `wta fs tree` prints it.
#[derive(Debug, Serialize)]
struct Tree {
    root: PathBuf,
    /// Whether the folder lies in a Git work tree, whose marks the entries wear.
    repo: bool,
    entries: Vec<TreeEntry>,
}

// ─── the commands ───────────────────────────────────────────────────────────

/// The folder a command reads: the one named, else the current one.
fn folder_of(path: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    match path {
        Some(folder) => Ok(folder),
        None => std::env::current_dir().context("wta: this process has no current folder"),
    }
}

fn print_json<T: Serialize>(value: &T) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// `wta git status`: the work tree's branch and changed files, or `null` outside one.
pub(crate) fn run_status(path: Option<PathBuf>) -> anyhow::Result<()> {
    let folder = folder_of(path)?;
    let repo = repo(&folder)?;
    tracing::info!(
        folder = %folder.display(),
        repo = repo.is_some(),
        "git status read"
    );
    print_json(&repo)
}

/// `wta git log`: the newest commits from HEAD, `null` outside a work tree and
/// empty before the first commit.
pub(crate) fn run_log(path: Option<PathBuf>, limit: usize) -> anyhow::Result<()> {
    let folder = folder_of(path)?;
    let Some(repo) = repo(&folder)? else {
        return print_json(&Option::<Vec<Commit>>::None);
    };
    let commits = match repo.commit {
        Some(_) => log(&repo.top, limit)?,
        None => Vec::new(),
    };
    tracing::info!(
        folder = %folder.display(),
        top = %repo.top.display(),
        commits = commits.len(),
        "git log read"
    );
    print_json(&Some(commits))
}

/// A file's diff as `wta git diff` prints it.
#[derive(Debug, Serialize)]
struct DiffReport {
    top: PathBuf,
    path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<PathBuf>,
    section: ChangeSection,
    /// None for a binary file; empty for a rename that changes no line.
    hunks: Option<Vec<Hunk>>,
}

/// The paths `file` may name, most literal first: as written from the work
/// tree's top, then as written from `folder`.
fn candidates(top: &Path, folder: &Path, file: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    match file.strip_prefix(top) {
        Ok(rest) => candidates.push(rest.to_path_buf()),
        Err(_) => candidates.push(file.to_path_buf()),
    }
    if let Ok(rest) = folder.strip_prefix(top) {
        let inside = rest.join(file);
        if !candidates.contains(&inside) {
            candidates.push(inside);
        }
    }
    candidates
}

/// `file` as git names it from the work tree's top: stripped of `top` when it
/// carries it, else joined to the folder it was read from.
fn top_relative(top: &Path, folder: &Path, file: &Path) -> PathBuf {
    if let Ok(rest) = file.strip_prefix(top) {
        return rest.to_path_buf();
    }
    match folder.strip_prefix(top) {
        Ok(rest) => rest.join(file),
        Err(_) => file.to_path_buf(),
    }
}

/// The change `file` names, from whichever section holds it.
fn find_change<'a>(
    repo: &'a RepoChanges,
    file: &Path,
    folder: &Path,
) -> Option<(&'a Change, ChangeSection)> {
    let named = candidates(&repo.top, folder, file);
    let sections = [
        (ChangeSection::Conflicted, &repo.conflicted),
        (ChangeSection::Staged, &repo.staged),
        (ChangeSection::Unstaged, &repo.unstaged),
        (ChangeSection::Untracked, &repo.untracked),
    ];
    sections.into_iter().find_map(|(section, changes)| {
        let change = changes.iter().find(|change| named.contains(&change.path))?;
        Some((change, section))
    })
}

/// Whether `change` names a folder: git lists an untracked folder whole and
/// marks its path with a separator.
fn is_folder(change: &Change) -> bool {
    change.path.to_string_lossy().ends_with(['/', '\\'])
}

/// The untracked folder holding `file`, if any: git lists the folder and not
/// what it holds, while the Files panel marks the contents untracked whole.
fn untracked_holder<'a>(repo: &'a RepoChanges, file: &Path, folder: &Path) -> Option<&'a Change> {
    let named = top_relative(&repo.top, folder, file);
    // A folder's path keeps the separator git wrote; components ignore it.
    repo.untracked
        .iter()
        .find(|change| named.starts_with(&change.path) && named != change.path)
}

/// `wta git diff`: one changed file's diff.
pub(crate) fn run_diff(
    path: Option<PathBuf>,
    file: &Path,
    from: Option<PathBuf>,
    section: Option<ChangeSection>,
) -> anyhow::Result<()> {
    let folder = folder_of(path)?;
    let Some(repo) = repo(&folder)? else {
        bail!("git: {} is not in a Git work tree", folder.display());
    };
    let found = find_change(&repo, file, &folder);
    let (named, from, section) = match (section, found) {
        // A section the caller named needs no matching change; a rename's old
        // path still comes from one when the file is changed.
        (Some(section), found) => {
            let change = found.map(|(change, _)| change).or_else(|| {
                (section == ChangeSection::Untracked)
                    .then(|| untracked_holder(&repo, file, &folder))
                    .flatten()
            });
            let named = match change {
                Some(change) if !is_folder(change) => change.path.clone(),
                _ => top_relative(&repo.top, &folder, file),
            };
            (
                named,
                from.or_else(|| change.and_then(|change| change.from.clone())),
                section,
            )
        }
        (None, Some((change, section))) => (
            change.path.clone(),
            from.or_else(|| change.from.clone()),
            section,
        ),
        // A file inside an untracked folder is that folder's change, read at
        // the file.
        (None, None) => match untracked_holder(&repo, file, &folder) {
            Some(change) => (
                top_relative(&repo.top, &folder, file),
                from.or_else(|| change.from.clone()),
                ChangeSection::Untracked,
            ),
            None => bail!(
                "git: {} is not changed in {}",
                file.display(),
                repo.top.display()
            ),
        },
    };
    let hunks = diff(&repo.top, &named, from.as_deref(), section)?;
    tracing::info!(
        top = %repo.top.display(),
        path = %named.display(),
        section = ?section,
        hunks = hunks.as_ref().map(Vec::len),
        "git diff read"
    );
    print_json(&DiffReport {
        top: repo.top,
        path: named,
        from,
        section,
        hunks,
    })
}

/// `wta fs tree`: a folder's entries with their Git marks.
pub(crate) fn run_tree(path: Option<PathBuf>, depth: usize) -> anyhow::Result<()> {
    let root = folder_of(path)?;
    let changes = changes(&root)?;
    let repo = changes.is_some();
    let marks = Marks::of(&root, changes.unwrap_or_default());
    let name = match root.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => root.display().to_string(),
    };
    let node = tree_entry(&root, name, false, depth, &marks);
    if let Some(error) = &node.error {
        bail!("wta: {} is unreadable: {error}", root.display());
    }
    tracing::info!(
        root = %root.display(),
        depth,
        entries = node.children.len(),
        marks = marks.marks.len(),
        "file tree read"
    );
    print_json(&Tree {
        root,
        repo,
        entries: node.children,
    })
}

#[cfg(test)]
mod tests;
