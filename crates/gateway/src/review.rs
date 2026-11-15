//! Everything a task changed, for the review panel («Правки») and the bot.
//!
//! A department with `worktree = true` works in its own clone `<run>/repo` on
//! branch `agentsflow/<task>`. There the truth is git: the branch's commits
//! against the commit it was cut from. Only committed work is shown: comparing
//! two commits never reads the working tree, so no clean filter, fsmonitor or
//! other command from the clone's config runs in the gateway. Without a clone,
//! or before its first commit, the changes are the agents' `diff` events of
//! the task, each one apart and in the order they came: Claude's edits carry
//! no line numbers and may have been refused afterwards, so merging them into
//! one file would show something that never existed.

use crate::home::Home;
use agentsflow_events::{Envelope, Event, TaskSnapshot};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Patch text one answer carries at most; the file list stays complete.
pub const DIFF_LIMIT: usize = 2 * 1024 * 1024;
/// The `--numstat` list: paths and counts only.
const LIST_LIMIT: usize = 512 * 1024;
/// Small answers: a commit id, a branch name.
const SHORT: usize = 4096;
const GIT_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The branch of the task's clone against its base.
    Git,
    /// The agents' `diff` events of the task.
    Events,
    /// Nothing changed that the gateway could see.
    None,
}

#[derive(Debug, Serialize)]
pub struct TaskDiff {
    pub source: Source,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Why the source is not the one the department would normally give.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Some patch text was left out by [`DIFF_LIMIT`].
    pub truncated: bool,
    pub files: Vec<FileDiff>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct FileDiff {
    pub path: String,
    /// `added`, `deleted` or `modified`.
    pub status: &'static str,
    pub added: u64,
    pub removed: u64,
    pub binary: bool,
    /// The file's patch is missing or cut by the size limit.
    pub truncated: bool,
    pub patches: Vec<Patch>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Patch {
    /// The department member who made it (events only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub text: String,
}

impl TaskDiff {
    fn none() -> Self {
        Self {
            source: Source::None,
            branch: None,
            base: None,
            note: None,
            truncated: false,
            files: Vec::new(),
        }
    }

    /// One line on where the changes come from, the same words as the page.
    pub fn source_label(&self) -> String {
        let mut text = match self.source {
            Source::Git => format!(
                "Ветка {} против {}, только закоммиченное.",
                self.branch.as_deref().unwrap_or("задачи"),
                self.base.as_deref().unwrap_or("базы")
            ),
            Source::Events => "Правки из ленты агентов, каждая отдельно и по порядку.".to_owned(),
            Source::None => "Правок нет.".to_owned(),
        };
        if let Some(note) = &self.note {
            text = format!("{note} {text}");
        }
        if self.truncated {
            text.push_str(" Показано не всё: правки больше 2 МБ.");
        }
        text
    }

    /// The changes as one unified diff, for a `.diff` file.
    pub fn unified(&self) -> String {
        let mut out = String::new();
        for file in &self.files {
            if self.source == Source::Git {
                for patch in &file.patches {
                    out.push_str(&patch.text);
                    if !patch.text.ends_with('\n') {
                        out.push('\n');
                    }
                }
                if file.patches.is_empty() {
                    out.push_str(&format!(
                        "# {}: не показано, правки больше 2 МБ\n",
                        file.path
                    ));
                }
                continue;
            }
            for (index, patch) in file.patches.iter().enumerate() {
                out.push_str(&format!(
                    "# {} · правка {}{}\n",
                    file.path,
                    index + 1,
                    patch
                        .agent
                        .as_deref()
                        .map_or(String::new(), |agent| format!(" · {agent}"))
                ));
                out.push_str(&patch.text);
                if !patch.text.ends_with('\n') {
                    out.push('\n');
                }
            }
            if file.truncated {
                out.push_str(&format!(
                    "# {}: часть правок не показана, их больше 2 МБ\n",
                    file.path
                ));
            }
        }
        out
    }
}

/// The changes of a department task, from git when the department keeps a
/// clone for it, otherwise from the agents' events in `history`.
pub fn task_diff(home: &Home, history: &[Envelope], task: &TaskSnapshot) -> TaskDiff {
    task_diff_within(home, history, task, DIFF_LIMIT)
}

fn task_diff_within(
    home: &Home,
    history: &[Envelope],
    task: &TaskSnapshot,
    limit: usize,
) -> TaskDiff {
    let Some(work) = &task.work else {
        return TaskDiff::none();
    };
    let dept = home.departments.get(&work.department);
    let mut roots = Vec::new();
    if let Some(dept) = dept {
        let run = dept.run_dir(&task.id);
        roots.push(run.join("repo"));
        roots.push(run);
        roots.extend(dept.repos.iter().cloned());
    }
    let events = from_events(history, &work.department, &task.id, &roots, limit);
    let mut note = None;
    if let Some(dept) = dept.filter(|dept| dept.worktree) {
        let run = dept.run_dir(&task.id);
        let branches = dept
            .source
            .as_ref()
            .map(|source| source.branches.clone())
            .unwrap_or_default();
        match repo_in_run(&run) {
            Some(repo) => match from_git(&repo, &branches, limit) {
                Ok(diff) if !diff.files.is_empty() || events.files.is_empty() => return diff,
                Ok(_) => note = Some("В ветке задачи пока нет коммитов.".to_owned()),
                Err(error) => note = Some(format!("git не показал правки: {error:#}.")),
            },
            None if std::fs::symlink_metadata(run.join("repo")).is_ok() => {
                note = Some(
                    "Копия репозитория задачи — ссылка или без своей папки .git, её не читаю."
                        .to_owned(),
                );
            }
            None => {}
        }
    }
    TaskDiff { note, ..events }
}

/// `<run>/repo` when it is a real folder with its own `.git` folder inside
/// the run: no link, no `.git` file pointing elsewhere.
fn repo_in_run(run: &Path) -> Option<PathBuf> {
    let repo = run.join("repo");
    let dot_git = repo.join(".git");
    let plain_dir = |path: &Path| {
        std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir() && !meta.is_symlink())
    };
    if !plain_dir(&repo) || !plain_dir(&dot_git) {
        return None;
    }
    let run_real = run.canonicalize().ok()?;
    let inside = |path: &Path| {
        path.canonicalize()
            .is_ok_and(|real| real.starts_with(&run_real))
    };
    (inside(&repo) && inside(&dot_git)).then_some(repo)
}

/// Runs git in `repo`, reading at most `cap` bytes of its output; `true`
/// when there was more. Settings that could start another program are off.
fn git(repo: &Path, args: &[&str], cap: usize) -> Result<(Vec<u8>, bool)> {
    let mut command = std::process::Command::new("git");
    command
        .arg("--no-pager")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.quotepath=false",
            "-c",
            "diff.noprefix=false",
            "-c",
            "diff.mnemonicPrefix=false",
            "-c",
            "diff.relative=false",
            "-c",
            "color.ui=false",
        ])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_EXTERNAL_DIFF",
        "GIT_CONFIG_PARAMETERS",
    ] {
        command.env_remove(name);
    }
    let mut child = command.spawn().context("git не запустился")?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let limit = u64::try_from(cap).unwrap_or(u64::MAX).saturating_add(1);
    // Reading stops at the cap and the pipe closes: git ends on its next write.
    let out = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(stdout) = stdout {
            let _ = stdout.take(limit).read_to_end(&mut bytes);
        }
        bytes
    });
    let err = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut stderr) = stderr {
            let _ = (&mut stderr).take(4096).read_to_end(&mut bytes);
            let _ = std::io::copy(&mut stderr, &mut std::io::sink());
        }
        bytes
    });
    let deadline = Instant::now() + GIT_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().context("git не дождался")? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut bytes = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    let Some(status) = status else {
        bail!("git не ответил за {} с", GIT_TIMEOUT.as_secs());
    };
    if bytes.len() > cap {
        bytes.truncate(cap);
        return Ok((bytes, true));
    }
    if !status.success() {
        bail!(
            "git {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&stderr).trim()
        );
    }
    Ok((bytes, false))
}

fn git_line(repo: &Path, args: &[&str]) -> Option<String> {
    let (bytes, _) = git(repo, args, SHORT).ok()?;
    let line = String::from_utf8_lossy(&bytes).trim().to_owned();
    (!line.is_empty()).then_some(line)
}

fn from_git(repo: &Path, branches: &[String], limit: usize) -> Result<TaskDiff> {
    let commit = |spec: &str| git_line(repo, &["rev-parse", "--verify", "--quiet", spec]);
    let head = commit("HEAD^{commit}").context("в копии нет коммитов")?;
    // The clone was made from the owner's repository: its `origin/HEAD` is
    // the branch the task was cut from.
    let mut candidates: Vec<String> = git_line(
        repo,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    )
    .into_iter()
    .collect();
    for branch in branches
        .iter()
        .map(String::as_str)
        .chain(["main", "master"])
    {
        if !branch.is_empty() && !branch.starts_with('-') {
            candidates.push(format!("origin/{branch}"));
        }
    }
    let (label, tip) = candidates
        .iter()
        .find_map(|name| commit(&format!("{name}^{{commit}}")).map(|tip| (name.clone(), tip)))
        .context("не нашлась базовая ветка (origin/HEAD, main, master)")?;
    let base = git_line(repo, &["merge-base", &head, &tip])
        .context("у ветки и базы нет общего коммита")?;
    let branch = git_line(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"]);
    let (list, list_cut) = git(
        repo,
        &[
            "diff",
            "--numstat",
            "-z",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            &base,
            &head,
            "--",
        ],
        LIST_LIMIT,
    )?;
    let (patch, cut) = git(
        repo,
        &[
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "-U3",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            &base,
            &head,
            "--",
        ],
        limit,
    )?;
    let files = merge(
        parse_numstat(&String::from_utf8_lossy(&list)),
        split_patch(&String::from_utf8_lossy(&patch)),
        cut,
    );
    Ok(TaskDiff {
        source: Source::Git,
        branch,
        base: Some(label.strip_prefix("origin/").unwrap_or(&label).to_owned()),
        note: None,
        truncated: cut || list_cut,
        files,
    })
}

/// `added\tremoved\tpath\0` records; binary files count as `-\t-`.
fn parse_numstat(list: &str) -> Vec<(String, u64, u64, bool)> {
    list.split('\0')
        .filter_map(|record| {
            let mut parts = record.splitn(3, '\t');
            let added = parts.next()?;
            let removed = parts.next()?;
            let path = parts.next()?.trim_start_matches('\n');
            if path.is_empty() {
                return None;
            }
            let binary = added == "-" && removed == "-";
            Some((
                path.to_owned(),
                added.parse().unwrap_or(0),
                removed.parse().unwrap_or(0),
                binary,
            ))
        })
        .collect()
}

struct Section {
    path: String,
    status: &'static str,
    binary: bool,
    text: String,
}

/// One section per file: from its `diff --git` line to the next one.
fn split_patch(patch: &str) -> Vec<Section> {
    let mut sections: Vec<String> = Vec::new();
    for line in patch.split_inclusive('\n') {
        if line.starts_with("diff --git ") || sections.is_empty() {
            sections.push(String::new());
        }
        if let Some(last) = sections.last_mut() {
            last.push_str(line);
        }
    }
    sections
        .into_iter()
        .filter(|text| text.starts_with("diff --git "))
        .map(|text| {
            let mut new_path = None;
            let mut old_path = None;
            let mut status = "modified";
            let mut binary = false;
            for line in text.lines() {
                if line.starts_with("@@") {
                    break;
                }
                if line.starts_with("new file mode") {
                    status = "added";
                } else if line.starts_with("deleted file mode") {
                    status = "deleted";
                } else if line.starts_with("Binary files ") || line == "GIT binary patch" {
                    binary = true;
                } else if let Some(path) = line.strip_prefix("+++ ") {
                    new_path = side(path, "b/");
                } else if let Some(path) = line.strip_prefix("--- ") {
                    old_path = side(path, "a/");
                }
            }
            let path = new_path
                .or(old_path)
                .or_else(|| header_path(text.lines().next().unwrap_or("")))
                .unwrap_or_default();
            Section {
                path,
                status,
                binary,
                text,
            }
        })
        .collect()
}

/// The path of a `---`/`+++` line, none for `/dev/null`.
fn side(path: &str, prefix: &str) -> Option<String> {
    // Git ends a path with a space by a tab, so the line stays unambiguous.
    let path = path.trim_end_matches(['\r', '\n']);
    let path = path.strip_suffix('\t').unwrap_or(path);
    if path == "/dev/null" {
        return None;
    }
    let path = unquote(path);
    Some(path.strip_prefix(prefix).unwrap_or(&path).to_owned())
}

/// `diff --git a/x b/x` of a file without `---`/`+++` lines (binary, mode).
fn header_path(line: &str) -> Option<String> {
    let rest = line.strip_prefix("diff --git ")?;
    if rest.starts_with('"') {
        let end = rest.rfind(" \"b/").or_else(|| rest.rfind(" b/"))?;
        let path = unquote(rest[end + 1..].trim());
        return Some(path.strip_prefix("b/").unwrap_or(&path).to_owned());
    }
    let at = rest.rfind(" b/")?;
    Some(rest[at + 3..].to_owned())
}

/// Git's C-style quoting of unusual paths: `"a\tb"`, `"\303\244"`.
fn unquote(path: &str) -> String {
    let Some(inner) = path
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    else {
        return path.to_owned();
    };
    let mut bytes = Vec::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0; 4];
            bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('n') => bytes.push(b'\n'),
            Some('t') => bytes.push(b'\t'),
            Some('r') => bytes.push(b'\r'),
            Some('a') => bytes.push(7),
            Some('b') => bytes.push(8),
            Some('f') => bytes.push(12),
            Some('v') => bytes.push(11),
            Some(d @ '0'..='7') => {
                let mut value = d.to_digit(8).unwrap_or(0);
                for _ in 0..2 {
                    if let Some(next) = chars.peek().and_then(|c| c.to_digit(8)) {
                        value = value * 8 + next;
                        chars.next();
                    }
                }
                bytes.push(u8::try_from(value).unwrap_or(b'?'));
            }
            Some(other) => {
                let mut buf = [0; 4];
                bytes.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
            }
            None => bytes.push(b'\\'),
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The full file list with git's counts, each with its patch where it fit.
fn merge(list: Vec<(String, u64, u64, bool)>, sections: Vec<Section>, cut: bool) -> Vec<FileDiff> {
    let last = sections.len().saturating_sub(1);
    let mut sections: Vec<Option<(usize, Section)>> =
        sections.into_iter().enumerate().map(Some).collect();
    let mut take = |path: &str| {
        sections
            .iter_mut()
            .find(|slot| slot.as_ref().is_some_and(|(_, s)| s.path == path))
            .and_then(Option::take)
    };
    let mut files: Vec<FileDiff> = list
        .into_iter()
        .map(|(path, added, removed, binary)| match take(&path) {
            Some((index, section)) => FileDiff {
                status: section.status,
                binary: binary || section.binary,
                truncated: cut && index == last,
                patches: vec![Patch {
                    agent: None,
                    text: section.text,
                }],
                path,
                added,
                removed,
            },
            None => FileDiff {
                path,
                status: "modified",
                added,
                removed,
                binary,
                truncated: true,
                patches: Vec::new(),
            },
        })
        .collect();
    // The list came back cut: what the patch has beyond it still counts.
    for (index, section) in sections.into_iter().flatten() {
        let (added, removed) = count(&section.text);
        files.push(FileDiff {
            path: section.path,
            status: section.status,
            added,
            removed,
            binary: section.binary,
            truncated: cut && index == last,
            patches: vec![Patch {
                agent: None,
                text: section.text,
            }],
        });
    }
    files
}

/// Added and removed lines of a patch, its `---`/`+++` header aside.
fn count(patch: &str) -> (u64, u64) {
    let mut added = 0;
    let mut removed = 0;
    let mut in_hunk = false;
    for line in patch.lines() {
        if line.starts_with("diff --git ") {
            in_hunk = false;
            continue;
        }
        if line.starts_with("@@") {
            in_hunk = true;
            continue;
        }
        let header = !in_hunk && (line.starts_with("+++") || line.starts_with("---"));
        if header {
            continue;
        }
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    (added, removed)
}

/// The `diff` events of the task's agents (`<department>:<member>:<task>`),
/// grouped by file in the order the files were first touched.
fn from_events(
    history: &[Envelope],
    department: &str,
    task: &str,
    roots: &[PathBuf],
    limit: usize,
) -> TaskDiff {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut size = 0;
    let mut truncated = false;
    for envelope in history {
        let Event::Diff { path, patch } = &envelope.event else {
            continue;
        };
        let mut parts = envelope.agent.as_str().split(':');
        let (Some(dept), Some(member), Some(owner), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if dept != department || owner != task {
            continue;
        }
        let path = short_path(path, roots);
        let at = match files.iter().position(|file| file.path == path) {
            Some(at) => at,
            None => {
                files.push(FileDiff {
                    path,
                    status: if patch.starts_with("--- /dev/null") {
                        "added"
                    } else {
                        "modified"
                    },
                    added: 0,
                    removed: 0,
                    binary: false,
                    truncated: false,
                    patches: Vec::new(),
                });
                files.len() - 1
            }
        };
        let (added, removed) = count(patch);
        let file = &mut files[at];
        file.added += added;
        file.removed += removed;
        if size + patch.len() > limit {
            truncated = true;
            file.truncated = true;
            continue;
        }
        size += patch.len();
        file.patches.push(Patch {
            agent: Some(member.to_owned()),
            text: patch.clone(),
        });
    }
    TaskDiff {
        source: if files.is_empty() {
            Source::None
        } else {
            Source::Events
        },
        truncated,
        files,
        ..TaskDiff::none()
    }
}

/// Agents name files by their full path: shown relative to the run's clone,
/// the run folder or a department repository when it is inside one.
fn short_path(path: &str, roots: &[PathBuf]) -> String {
    let path = path.replace('\\', "/");
    for root in roots {
        let root = root.to_string_lossy().replace('\\', "/");
        let root = root.trim_end_matches('/');
        if root.is_empty() {
            continue;
        }
        let same = path
            .get(..root.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(root));
        if same && path.as_bytes().get(root.len()) == Some(&b'/') {
            if let Some(rest) = path.get(root.len() + 1..).filter(|rest| !rest.is_empty()) {
                return rest.to_owned();
            }
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentsflow_events::{AgentId, WorkInfo, WorkState};

    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn scratch() -> Scratch {
        let dir = std::env::temp_dir().join(format!("agentsflow-review-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    fn sh(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.autocrlf=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn task(id: &str, department: &str) -> TaskSnapshot {
        TaskSnapshot {
            id: id.into(),
            title: "t".into(),
            description: String::new(),
            status: agentsflow_events::TaskStatus::InProgress,
            assignee: None,
            progress: 0,
            labels: vec![],
            note: String::new(),
            thread: None,
            created_ms: 0,
            updated_ms: 0,
            work: Some(WorkInfo {
                department: department.into(),
                state: WorkState::Running,
                ..Default::default()
            }),
        }
    }

    fn diff_event(agent: &str, path: &str, patch: &str) -> Envelope {
        Envelope {
            session: "s".into(),
            agent: AgentId::from(agent),
            seq: 0,
            ts_ms: 0,
            event: Event::Diff {
                path: path.into(),
                patch: patch.into(),
            },
        }
    }

    /// A home whose department `code` keeps a clone of `repo` per task.
    fn code_home(dir: &Path, repo: &Path) -> Home {
        crate::home::init(dir).unwrap();
        let code = dir.join("departments").join("code");
        std::fs::create_dir_all(&code).unwrap();
        let toml = crate::department::tests::SAMPLE
            .replace("id = \"pr\"", "id = \"code\"")
            .replace(
                "[limits]",
                &format!("worktree = true\nrepos = [{repo:?}]\n\n[limits]"),
            );
        std::fs::write(code.join("department.toml"), toml).unwrap();
        Home::load(dir).unwrap()
    }

    /// The owner's repository with one commit on `main`, and the task's clone.
    fn repo_and_clone(root: &Path) -> (Home, PathBuf) {
        let repo = root.join("owner");
        std::fs::create_dir_all(&repo).unwrap();
        sh(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(repo.join("gone.txt"), "bye\n").unwrap();
        sh(&repo, &["add", "."]);
        sh(&repo, &["commit", "-q", "-m", "init"]);
        let home = code_home(&root.join("home"), &repo);
        let run = home.departments["code"].run_dir("t1");
        std::fs::create_dir_all(&run).unwrap();
        let project = crate::projectmem::RunProject::new(std::slice::from_ref(&repo), None)
            .with_worktree(&run, "t1")
            .unwrap();
        (home, project.worktree.unwrap())
    }

    #[test]
    fn git_shows_the_task_branch_against_its_base() {
        let root = scratch();
        let (home, clone) = repo_and_clone(&root.0);
        std::fs::write(clone.join("a.txt"), "one\nTWO\nthree\n").unwrap();
        std::fs::write(clone.join("new file.txt"), "hello\n").unwrap();
        std::fs::remove_file(clone.join("gone.txt")).unwrap();
        sh(&clone, &["add", "-A"]);
        sh(&clone, &["commit", "-q", "-m", "work"]);
        // Not committed: not shown, the working tree is never read.
        std::fs::write(clone.join("a.txt"), "draft\n").unwrap();
        let diff = task_diff(&home, &[], &task("t1", "code"));
        assert_eq!(diff.source, Source::Git, "{:?}", diff.note);
        assert_eq!(diff.branch.as_deref(), Some("agentsflow/t1"));
        assert_eq!(diff.base.as_deref(), Some("main"));
        assert!(!diff.truncated);
        let seen: Vec<_> = diff
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.status, f.added, f.removed))
            .collect();
        assert_eq!(
            seen,
            vec![
                ("a.txt", "modified", 1, 1),
                ("gone.txt", "deleted", 0, 1),
                ("new file.txt", "added", 1, 0),
            ]
        );
        let a = &diff.files[0].patches[0].text;
        assert!(a.starts_with("diff --git a/a.txt b/a.txt"), "{a}");
        assert!(
            a.contains("@@ -1,3 +1,3 @@") && a.contains("-two\n+TWO\n"),
            "{a}"
        );
        assert!(!diff.unified().contains("draft"));
        assert!(diff.source_label().contains("agentsflow/t1"));
    }

    #[test]
    fn the_size_cap_keeps_the_file_list() {
        let root = scratch();
        let (home, clone) = repo_and_clone(&root.0);
        std::fs::write(clone.join("big.txt"), "line\n".repeat(5000)).unwrap();
        std::fs::write(clone.join("small.txt"), "x\n").unwrap();
        sh(&clone, &["add", "-A"]);
        sh(&clone, &["commit", "-q", "-m", "work"]);
        let diff = task_diff_within(&home, &[], &task("t1", "code"), 2000);
        assert_eq!(diff.source, Source::Git);
        assert!(diff.truncated);
        let big = diff.files.iter().find(|f| f.path == "big.txt").unwrap();
        assert_eq!((big.added, big.truncated), (5000, true));
        let small = diff.files.iter().find(|f| f.path == "small.txt").unwrap();
        assert_eq!(small.added, 1);
        let text: usize = diff
            .files
            .iter()
            .flat_map(|f| &f.patches)
            .map(|p| p.text.len())
            .sum();
        assert!(text <= 2000, "{text}");
        assert!(diff.source_label().contains("не всё"));
    }

    #[test]
    fn a_clone_that_points_outside_the_run_is_not_read() {
        let root = scratch();
        let (home, clone) = repo_and_clone(&root.0);
        // `.git` as a file sends git to another repository: refused.
        std::fs::remove_dir_all(clone.join(".git")).unwrap();
        let elsewhere = root.0.join("owner").join(".git");
        std::fs::write(
            clone.join(".git"),
            format!("gitdir: {}\n", elsewhere.display()),
        )
        .unwrap();
        let history = [diff_event(
            "code:doer:t1",
            &clone.join("a.txt").to_string_lossy(),
            "--- a.txt\n+++ a.txt\n@@ edit @@\n-two\n+2\n",
        )];
        let diff = task_diff(&home, &history, &task("t1", "code"));
        assert_eq!(diff.source, Source::Events);
        assert!(diff.note.as_deref().unwrap().contains(".git"));
        assert_eq!(diff.files[0].path, "a.txt");
        assert!(repo_in_run(&home.departments["code"].run_dir("t1")).is_none());
        assert!(repo_in_run(&root.0.join("nowhere")).is_none());
    }

    #[test]
    fn before_the_first_commit_the_feed_is_shown() {
        let root = scratch();
        let (home, _) = repo_and_clone(&root.0);
        let history = [diff_event(
            "code:doer:t1",
            "x.rs",
            "--- /dev/null\n+++ x.rs\n@@ new file @@\n+fn x() {}\n",
        )];
        let diff = task_diff(&home, &history, &task("t1", "code"));
        assert_eq!(diff.source, Source::Events);
        assert!(diff.note.as_deref().unwrap().contains("нет коммитов"));
        // Nothing anywhere: the empty git answer, not a fallback.
        let diff = task_diff(&home, &[], &task("t1", "code"));
        assert_eq!(diff.source, Source::Git);
        assert!(diff.files.is_empty());
    }

    #[test]
    fn events_are_the_task_agents_own_in_order() {
        let root = scratch();
        crate::home::init(&root.0).unwrap();
        let home = Home::load(&root.0).unwrap();
        let run = home.departments["general"].run_dir("t1");
        let full = run.join("notes.md").to_string_lossy().into_owned();
        let history = [
            diff_event(
                "general:doer:t1",
                &full,
                "--- /dev/null\n+++ notes.md\n@@ new file @@\n+a\n+b\n",
            ),
            diff_event("general:doer:t2", "other.md", "+x\n"),
            diff_event("pr:doer:t1", "foreign.md", "+x\n"),
            diff_event("general:doer", "short.md", "+x\n"),
            diff_event(
                "general:critic:t1",
                &full,
                "--- notes.md\n+++ notes.md\n@@ edit @@\n-b\n+c\n",
            ),
        ];
        let diff = task_diff(&home, &history, &task("t1", "general"));
        assert_eq!(diff.source, Source::Events);
        assert!(diff.note.is_none());
        assert_eq!(diff.files.len(), 1);
        let file = &diff.files[0];
        assert_eq!(
            (file.path.as_str(), file.status, file.added, file.removed),
            ("notes.md", "added", 3, 1)
        );
        let agents: Vec<_> = file.patches.iter().map(|p| p.agent.as_deref()).collect();
        assert_eq!(agents, vec![Some("doer"), Some("critic")]);
        let unified = diff.unified();
        assert!(
            unified.contains("# notes.md · правка 2 · critic\n"),
            "{unified}"
        );
        // The cap drops patch text, never the file or its counts.
        let capped = task_diff_within(&home, &history, &task("t1", "general"), 60);
        assert!(capped.truncated && capped.files[0].truncated);
        assert_eq!(capped.files[0].patches.len(), 1);
        assert_eq!(capped.files[0].added, 3);
        let none = task_diff(&home, &[], &task("t1", "general"));
        assert_eq!(none.source, Source::None);
    }

    #[test]
    fn paths_and_patches_parse() {
        assert_eq!(unquote("\"a\\tb\\\"c\\303\\244\""), "a\tb\"cä");
        assert_eq!(unquote("plain"), "plain");
        assert_eq!(
            header_path("diff --git a/img.png b/img.png"),
            Some("img.png".into())
        );
        assert_eq!(
            parse_numstat("1\t2\ta.txt\0-\t-\timg.png\0"),
            vec![
                ("a.txt".into(), 1, 2, false),
                ("img.png".into(), 0, 0, true)
            ]
        );
        let sections = split_patch(
            "diff --git a/img.png b/img.png\nnew file mode 100644\nBinary files /dev/null and b/img.png differ\n",
        );
        assert_eq!(sections.len(), 1);
        assert_eq!(
            (
                sections[0].path.as_str(),
                sections[0].status,
                sections[0].binary
            ),
            ("img.png", "added", true)
        );
        assert_eq!(count("--- a/x\n+++ b/x\n@@ -1 +1 @@\n---y\n+++z\n"), (1, 1));
        let roots = [PathBuf::from("C:\\Run\\repo")];
        assert_eq!(short_path("c:/run/repo/src/a.rs", &roots), "src/a.rs");
        assert_eq!(
            short_path("C:\\Run\\repository\\a.rs", &roots),
            "C:/Run/repository/a.rs"
        );
    }
}
