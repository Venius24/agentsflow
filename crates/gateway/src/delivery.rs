//! Code leaves a department as a draft pull request. The agent only commits
//! inside its copy in the run folder; the gateway brings the branch to the
//! owner's repository ([`crate::projectmem::RunProject::publish`]) and then
//! pushes it from there and opens a draft PR with `gh`. The agent never has
//! the keys. Merging and taking the draft mark off stay with the owner.

use crate::sources::Gh;
use agentsflow_events::EventRef;
use anyhow::{Context, Result, bail};
use std::path::Path;

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let mut command = std::process::Command::new("git");
    command.arg("-C").arg(dir).args(args);
    let out = crate::sources::output_within(command, crate::sources::CALL_TIMEOUT)
        .context("git не запустился")?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// `owner/name` of a GitHub remote URL in any of its usual spellings.
pub fn origin_slug(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    let tail = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.strip_prefix("https://github.com/"))
        .or_else(|| url.strip_prefix("http://github.com/"))?;
    crate::department::valid_repo(tail).then(|| tail.to_owned())
}

/// What `open_pr` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Opened {
    /// The task made no commits: nothing to hand over.
    NothingToHandOver,
    Pr(String),
}

/// Pushes `agentsflow/<task>` from the owner's repository and opens a draft
/// PR. Refuses a repository whose `origin` is not the one the department is
/// bound to, never forces a push.
#[allow(clippy::too_many_arguments)] // one call site and its tests; a struct would only rename them
pub fn open_pr(
    gh: &dyn Gh,
    repo: &str,
    owner_repo: &Path,
    task: &str,
    title: &str,
    body: &str,
    base: &str,
    existing_pr: Option<&str>,
) -> Result<Opened> {
    // The raw setting, not `remote get-url`: that one expands `insteadOf`.
    let origin = git(owner_repo, &["config", "--get", "remote.origin.url"])
        .context("у репозитория нет origin")?;
    match origin_slug(&origin) {
        Some(slug) if slug.eq_ignore_ascii_case(repo) => {}
        Some(slug) => bail!("origin репозитория — {slug}, а отдел привязан к {repo}"),
        None => bail!("origin репозитория ({origin}) не GitHub-адрес вида owner/name"),
    }
    let branch = format!("agentsflow/{task}");
    // The base may be stale in the owner's clone: look, but do not fail on it.
    let _ = git(owner_repo, &["fetch", "-q", "origin", base]);
    let remote_base = format!("origin/{base}");
    // A commit that adds a workflow runs with the repository's secrets the
    // moment its branch is pushed. Such a branch is the owner's to push.
    let changed = git(
        owner_repo,
        &[
            "-c",
            "core.quotepath=false",
            "diff",
            "--name-only",
            &format!("{remote_base}...{branch}"),
        ],
    )
    .unwrap_or_default();
    if let Some(path) = changed.lines().find(|path| runs_in_ci(path)) {
        bail!(
            "ветка меняет {path}: так в CI попадают секреты репозитория, поэтому сама она не уходит. \
             Прочитайте дифф и отправьте ветку вручную"
        );
    }
    let ahead = git(
        owner_repo,
        &["rev-list", "--count", &format!("{remote_base}..{branch}")],
    )
    .with_context(|| format!("нет ветки {branch} или {remote_base}"))?;
    if ahead.trim() == "0" {
        return Ok(Opened::NothingToHandOver);
    }
    // No `--force`: a branch that is already there with other history is an error.
    git(
        owner_repo,
        &["push", "-q", "origin", &format!("{branch}:{branch}")],
    )
    .with_context(|| format!("ветку {branch} не удалось отправить"))?;
    // A task sent back and run again only adds commits to its PR.
    if let Some(url) = existing_pr {
        return Ok(Opened::Pr(url.to_owned()));
    }
    let file = owner_repo
        .join(".git")
        .join(format!("agentsflow-pr-{task}.md"));
    std::fs::write(&file, body).context("не записал текст PR")?;
    let created = gh.run(&[
        "pr".into(),
        "create".into(),
        "--draft".into(),
        "--repo".into(),
        repo.into(),
        "--head".into(),
        branch,
        "--base".into(),
        base.into(),
        "--title".into(),
        title.into(),
        "--body-file".into(),
        file.to_string_lossy().into_owned(),
    ]);
    let _ = std::fs::remove_file(&file);
    let created = created?;
    let url = created
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| line.starts_with("https://"))
        .context("gh не вернул адрес PR")?;
    Ok(Opened::Pr(url.to_owned()))
}

/// Files whose change makes a push run code with the repository's secrets.
pub fn runs_in_ci(path: &str) -> bool {
    let path = path.replace('\\', "/").to_lowercase();
    [
        ".github/",
        ".circleci/",
        ".buildkite/",
        ".gitlab-ci",
        "azure-pipelines",
        "jenkinsfile",
        ".travis.yml",
        "bitbucket-pipelines",
    ]
    .iter()
    .any(|mark| path.starts_with(mark))
        || path.contains("/.github/")
}

/// The base branch PRs go to: the repository's own default, else `main`.
pub fn base_branch(owner_repo: &Path, configured: &[String]) -> String {
    if let Some(first) = configured.first() {
        return first.clone();
    }
    git(
        owner_repo,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )
    .ok()
    .and_then(|head| head.strip_prefix("origin/").map(str::to_owned))
    .unwrap_or_else(|| "main".into())
}

/// Text that goes into GitHub: no `@name` can ping a stranger.
pub fn plain(text: &str) -> String {
    text.replace('@', "@\u{200b}")
}

/// The body of the PR: the department's result, the task, the spend. Text of
/// the outside event is not copied here.
pub fn pr_body(task: &str, title: &str, result: &str, spent: &str) -> String {
    let title = plain(title);
    let mut result = plain(result.trim());
    if result.chars().count() > 6000 {
        result = result.chars().take(6000).collect::<String>() + "…";
    }
    format!(
        "{result}\n\n---\nЗадача agentsflow `{task}`: {title}\nПотрачено: {spent}\nЧерновик: смотрит и вливает хозяин.\n"
    )
}

/// The closing comment: a fixed shape, one line of the result, nothing from
/// the outside event.
pub fn comment_text(result: &str, pr: Option<&str>, spent: &str) -> String {
    let first = result
        .lines()
        .map(|line| line.trim().trim_start_matches('#').trim())
        .find(|line| !line.is_empty())
        .unwrap_or("готово");
    let mut first: String = first
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(200)
        .collect();
    // No mention may ping a stranger.
    first = first.replace('@', "@\u{200b}");
    let mut text = format!("Итог: {first}");
    if let Some(pr) = pr {
        text.push_str(&format!("\nPR: {pr}"));
    }
    text.push_str(&format!("\nРасход: {spent}"));
    text
}

/// Writes the closing comment on the issue or pull request of the event.
pub fn post_comment(gh: &dyn Gh, event: &EventRef, text: &str, dir: &Path) -> Result<()> {
    let Some(number) = event.number else {
        bail!("у события нет номера, комментировать некуда");
    };
    let file = dir.join("comment.md");
    std::fs::write(&file, text).context("не записал комментарий")?;
    let result = gh.run(&[
        if event.is_pr { "pr" } else { "issue" }.into(),
        "comment".into(),
        number.to_string(),
        "--repo".into(),
        event.repo.clone(),
        "--body-file".into(),
        file.to_string_lossy().into_owned(),
    ]);
    let _ = std::fs::remove_file(&file);
    result.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FakeGh {
        calls: Mutex<Vec<Vec<String>>>,
        reply: Result<String, String>,
    }

    impl FakeGh {
        fn ok(reply: &str) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                reply: Ok(reply.into()),
            }
        }
    }

    impl Gh for FakeGh {
        fn run(&self, args: &[String]) -> Result<String> {
            self.calls.lock().unwrap().push(args.to_vec());
            self.reply.clone().map_err(|e| anyhow::anyhow!(e))
        }
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

    struct Scratch(std::path::PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// An owner's repository whose `origin` says GitHub but is redirected to
    /// a local bare repository, with the task branch `agentsflow/t1` ahead of
    /// main by one commit (or by none).
    fn owner_repo(commits_on_branch: bool) -> (Scratch, std::path::PathBuf, std::path::PathBuf) {
        let root =
            std::env::temp_dir().join(format!("af-delivery-{}", uuid::Uuid::new_v4().simple()));
        let bare = root.join("remote.git");
        let owner = root.join("owner");
        std::fs::create_dir_all(&bare).unwrap();
        std::fs::create_dir_all(&owner).unwrap();
        sh(&bare, &["init", "-q", "--bare", "-b", "main"]);
        sh(&owner, &["init", "-q", "-b", "main"]);
        std::fs::write(owner.join("a.txt"), "a").unwrap();
        sh(&owner, &["add", "."]);
        sh(&owner, &["commit", "-q", "-m", "first"]);
        sh(
            &owner,
            &["remote", "add", "origin", "https://github.com/o/r.git"],
        );
        let local = bare.to_string_lossy().replace('\\', "/");
        sh(
            &owner,
            &[
                "config",
                &format!("url.{local}.insteadOf"),
                "https://github.com/o/r.git",
            ],
        );
        sh(&owner, &["push", "-q", "origin", "main"]);
        sh(&owner, &["checkout", "-q", "-b", "agentsflow/t1"]);
        if commits_on_branch {
            std::fs::write(owner.join("b.txt"), "b").unwrap();
            sh(&owner, &["add", "."]);
            sh(&owner, &["commit", "-q", "-m", "work"]);
        }
        sh(&owner, &["checkout", "-q", "main"]);
        (Scratch(root), owner, bare)
    }

    #[test]
    fn origin_urls_in_every_spelling() {
        for url in [
            "https://github.com/o/r.git",
            "https://github.com/o/r",
            "git@github.com:o/r.git",
            "ssh://git@github.com/o/r",
            "https://github.com/o/r/",
        ] {
            assert_eq!(origin_slug(url).as_deref(), Some("o/r"), "{url}");
        }
        for url in [
            "https://gitlab.com/o/r",
            "/srv/git/r",
            "https://github.com/o",
            "https://github.com/o/r/x",
        ] {
            assert_eq!(origin_slug(url), None, "{url}");
        }
    }

    #[test]
    fn the_comment_has_a_fixed_shape_and_pings_nobody() {
        let text = comment_text(
            "# Починил сборку @alice\n\nподробности...",
            Some("https://github.com/o/r/pull/9"),
            "$0.42",
        );
        assert_eq!(
            text,
            "Итог: Починил сборку @\u{200b}alice\nPR: https://github.com/o/r/pull/9\nРасход: $0.42"
        );
        let long = comment_text(&"я".repeat(900), None, "$1");
        assert!(long.chars().count() < 260);
        assert!(!long.contains("PR:"));
    }

    #[test]
    fn a_pr_body_carries_the_result_but_not_the_event() {
        let body = pr_body("t1", "Чинит CI", "Готово.", "$1");
        assert!(body.starts_with("Готово."));
        assert!(body.contains("`t1`"));
    }

    #[test]
    fn a_draft_pr_is_pushed_without_force_and_opened_as_draft() {
        let (_guard, owner, bare) = owner_repo(true);
        let gh = FakeGh::ok("Creating draft pull request...\nhttps://github.com/o/r/pull/9\n");
        let opened = open_pr(&gh, "o/r", &owner, "t1", "Чинит CI", "тело", "main", None).unwrap();
        assert_eq!(opened, Opened::Pr("https://github.com/o/r/pull/9".into()));
        let pushed = git(&bare, &["rev-parse", "--verify", "agentsflow/t1"]).unwrap();
        assert!(!pushed.is_empty());
        let calls = gh.calls.lock().unwrap();
        let call = &calls[0];
        assert_eq!(&call[..3], ["pr", "create", "--draft"]);
        assert!(call.contains(&"agentsflow/t1".to_owned()));
        assert!(call.contains(&"--body-file".to_owned()));
    }

    #[test]
    fn ci_files_are_recognised_in_any_spelling() {
        for yes in [
            ".github/workflows/x.yml",
            ".GitHub\\workflows\\x.yml",
            "sub/.github/workflows/x.yml",
            ".gitlab-ci.yml",
            "Jenkinsfile",
            ".circleci/config.yml",
        ] {
            assert!(runs_in_ci(yes), "{yes}");
        }
        for no in ["src/github.rs", "docs/.github-notes.md", "deploy/local.ps1"] {
            assert!(!runs_in_ci(no), "{no}");
        }
    }

    #[test]
    fn a_branch_that_adds_a_workflow_does_not_leave_by_itself() {
        let (_guard, owner, bare) = owner_repo(true);
        sh(&owner, &["checkout", "-q", "agentsflow/t1"]);
        std::fs::create_dir_all(owner.join(".github/workflows")).unwrap();
        std::fs::write(owner.join(".github/workflows/x.yml"), "on: push\n").unwrap();
        sh(&owner, &["add", "."]);
        sh(&owner, &["commit", "-q", "-m", "ci"]);
        sh(&owner, &["checkout", "-q", "-"]);
        let gh = FakeGh::ok("https://github.com/o/r/pull/9");
        let error = open_pr(&gh, "o/r", &owner, "t1", "t", "b", "main", None).unwrap_err();
        assert!(
            format!("{error:#}").contains(".github/workflows/x.yml"),
            "{error:#}"
        );
        assert!(git(&bare, &["rev-parse", "--verify", "agentsflow/t1"]).is_err());
        assert!(gh.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn a_task_without_commits_hands_nothing_over() {
        let (_guard, owner, bare) = owner_repo(false);
        let gh = FakeGh::ok("https://github.com/o/r/pull/9");
        let opened = open_pr(&gh, "o/r", &owner, "t1", "t", "b", "main", None).unwrap();
        assert_eq!(opened, Opened::NothingToHandOver);
        assert!(gh.calls.lock().unwrap().is_empty());
        assert!(git(&bare, &["rev-parse", "--verify", "agentsflow/t1"]).is_err());
    }

    #[test]
    fn another_repository_is_refused_before_anything_leaves() {
        let (_guard, owner, bare) = owner_repo(true);
        let gh = FakeGh::ok("https://github.com/x/y/pull/1");
        let error = open_pr(&gh, "x/y", &owner, "t1", "t", "b", "main", None).unwrap_err();
        assert!(format!("{error:#}").contains("o/r"), "{error:#}");
        assert!(git(&bare, &["rev-parse", "--verify", "agentsflow/t1"]).is_err());
        assert!(gh.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn a_branch_that_is_already_there_is_not_overwritten() {
        let (_guard, owner, bare) = owner_repo(true);
        // Someone pushed other history to the same branch name.
        let other = bare.parent().unwrap().join("other");
        std::fs::create_dir_all(&other).unwrap();
        sh(&other, &["clone", "-q", &bare.to_string_lossy(), "."]);
        sh(&other, &["checkout", "-q", "-b", "agentsflow/t1"]);
        std::fs::write(other.join("c.txt"), "c").unwrap();
        sh(&other, &["add", "."]);
        sh(&other, &["commit", "-q", "-m", "stranger"]);
        sh(&other, &["push", "-q", "origin", "agentsflow/t1"]);
        let gh = FakeGh::ok("https://github.com/o/r/pull/9");
        assert!(open_pr(&gh, "o/r", &owner, "t1", "t", "b", "main", None).is_err());
        assert!(
            gh.calls.lock().unwrap().is_empty(),
            "no PR for a refused push"
        );
    }

    #[test]
    fn a_comment_goes_to_the_issue_or_the_pull_request_it_came_from() {
        let dir =
            std::env::temp_dir().join(format!("af-comment-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let gh = FakeGh::ok("");
        let mut event = EventRef {
            kind: "issue".into(),
            repo: "o/r".into(),
            key: "k".into(),
            number: Some(4),
            is_pr: false,
            url: String::new(),
        };
        post_comment(&gh, &event, "итог", &dir).unwrap();
        event.is_pr = true;
        post_comment(&gh, &event, "итог", &dir).unwrap();
        event.number = None;
        assert!(post_comment(&gh, &event, "итог", &dir).is_err());
        let calls = gh.calls.lock().unwrap();
        assert_eq!(&calls[0][..3], ["issue", "comment", "4"]);
        assert_eq!(&calls[1][..3], ["pr", "comment", "4"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
