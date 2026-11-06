//! Departments that work inside the owner's repositories. The agent starts in
//! the repository, so Claude Code loads its CLAUDE.md and skills itself. The
//! owner's memory of the project (`~/.claude/projects/<slug>/memory`) is read
//! only: the bot keeps its own memory next to it (`bot-memory/`), and a fact
//! reaches the owner's memory only when the owner approves it.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// A fact for the owner's memory an agent leaves in the run folder.
pub const SHARE_FILE: &str = "share.md";
/// Proposals to share a fact are kept with the rule proposals of the
/// department, told apart by this id prefix.
pub const SHARE_PREFIX: &str = "share-";
/// Turns Claude Code's own auto memory off for the bot's agents: they read
/// the owner's memory by path and write only their own.
pub const NO_AUTO_MEMORY: (&str, &str) = ("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1");

/// The repositories of one run: the chosen one first, and the worktree the
/// run works in when the department commits.
#[derive(Debug, Clone, Default)]
pub struct RunProject {
    pub repos: Vec<PathBuf>,
    /// `<run>/repo`, a git worktree of `repos[0]` on the branch below.
    pub worktree: Option<PathBuf>,
    pub branch: String,
}

impl RunProject {
    /// The department's repositories with the task's choice first.
    pub fn new(dept_repos: &[PathBuf], chosen: Option<&str>) -> Self {
        let mut repos = dept_repos.to_vec();
        if let Some(chosen) = chosen.and_then(|c| find_repo(dept_repos, c))
            && let Some(at) = repos.iter().position(|r| *r == chosen)
        {
            let repo = repos.remove(at);
            repos.insert(0, repo);
        }
        Self {
            repos,
            ..Self::default()
        }
    }

    /// Makes (or keeps, on the next runs of the task) the task's own copy of
    /// the first repository in `<run>/repo` on branch `agentsflow/<task>`.
    /// A local clone, not a git worktree: commits stay inside the run folder,
    /// where Codex's sandbox may write. [`RunProject::publish`] brings the
    /// branch back to the owner's repository.
    pub fn with_worktree(mut self, run_dir: &Path, task: &str) -> Result<Self> {
        let Some(repo) = self.repos.first() else {
            return Ok(self);
        };
        let path = run_dir.join("repo");
        let branch = format!("agentsflow/{task}");
        if !path.join(".git").exists() {
            git(None, &["clone", "-q", &repo.to_string_lossy(), &path.to_string_lossy()])?;
            let earlier = format!("origin/{branch}");
            let known = git(Some(&path), &["rev-parse", "--verify", "--quiet", &earlier]).is_ok();
            if known {
                git(Some(&path), &["checkout", "-q", "-b", &branch, &earlier])?;
            } else {
                git(Some(&path), &["checkout", "-q", "-b", &branch])?;
            }
        }
        self.worktree = Some(path);
        self.branch = branch;
        Ok(self)
    }

    /// Fetches the task's branch from its copy into the owner's repository,
    /// so it is there to look at and merge. Fast-forward only.
    pub fn publish(&self) -> Result<()> {
        let (Some(repo), Some(copy)) = (self.repos.first(), &self.worktree) else {
            return Ok(());
        };
        let spec = format!("{0}:{0}", self.branch);
        git(Some(repo), &["fetch", "-q", &copy.to_string_lossy(), &spec])?;
        Ok(())
    }
}

fn git(dir: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut command = std::process::Command::new("git");
    if let Some(dir) = dir {
        command.arg("-C").arg(dir);
    }
    let out = command.args(args).output().context("git не запустился")?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The repository of the list that `name` names: its full path or its last
/// folder name, ignoring case and slashes.
pub fn find_repo(repos: &[PathBuf], name: &str) -> Option<PathBuf> {
    let norm = |s: &str| s.replace('\\', "/").trim_end_matches('/').to_lowercase();
    let name = norm(name);
    repos
        .iter()
        .find(|repo| {
            let full = norm(&repo.to_string_lossy());
            full == name || full.rsplit('/').next() == Some(name.as_str())
        })
        .cloned()
}

/// One line about a repository for the orchestrator: the first heading of
/// its CLAUDE.md, AGENTS.md or README.
pub fn describe(repo: &Path) -> String {
    for name in ["CLAUDE.md", "AGENTS.md", "README.md"] {
        let Ok(text) = std::fs::read_to_string(repo.join(name)) else {
            continue;
        };
        if let Some(line) = text
            .lines()
            .map(|line| line.trim().trim_start_matches('#').trim())
            .find(|line| !line.is_empty())
        {
            return line.chars().take(120).collect();
        }
    }
    "без описания".into()
}

/// Claude Code names a project folder by its path with every character that
/// is not a letter or digit replaced by `-`: `F:\Dev\sbx` → `F--Dev-sbx`.
pub fn slug(repo: &Path) -> String {
    repo.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn claude_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(|home| PathBuf::from(home).join(".claude"))
}

fn project_dir(repo: &Path) -> Option<PathBuf> {
    Some(claude_dir()?.join("projects").join(slug(repo)))
}

/// The owner's memory of the project, as Claude Code keeps it.
pub fn owner_memory(repo: &Path) -> Option<PathBuf> {
    Some(project_dir(repo)?.join("memory"))
}

/// The bot's own memory of the project, shared by its departments.
pub fn bot_memory(repo: &Path) -> Option<PathBuf> {
    Some(project_dir(repo)?.join("bot-memory"))
}

/// Whether a path lies in the owner's memory of any project.
pub fn in_owner_memory(path: &str) -> bool {
    let path = path.replace('\\', "/").to_lowercase();
    let Some(root) = claude_dir() else {
        return false;
    };
    let root = root.to_string_lossy().replace('\\', "/").to_lowercase();
    let Some(rest) = path.strip_prefix(&format!("{root}/projects/")) else {
        // Relative or spelled differently: judge by the shape.
        return path.contains("/.claude/projects/") && path.contains("/memory");
    };
    let mut parts = rest.split('/');
    parts.next();
    parts.next() == Some("memory")
}

/// Commands that only read files.
const READERS: &[&str] = &[
    "cat", "head", "tail", "ls", "dir", "grep", "rg", "find", "wc", "type", "less", "more", "stat",
    "file", "get-content", "get-childitem", "select-string", "test-path",
];

/// A shell command that touches the owner's memory with anything but reading.
pub fn writes_owner_memory(command: &str) -> bool {
    let lower = command.replace('\\', "/").to_lowercase();
    if !(lower.contains(".claude/projects/") && lower.contains("/memory")) {
        return false;
    }
    // Any redirection or non-reading program in a command that names it.
    if lower.contains('>') {
        return true;
    }
    lower
        .split(['|', ';', '&', '\n'])
        .filter_map(|segment| segment.split_whitespace().next())
        .any(|program| {
            let program = program.rsplit('/').next().unwrap_or(program);
            !READERS.contains(&program.trim_end_matches(".exe"))
        })
}

/// Tools that write a file given by path.
pub fn is_write_tool(tool: &str) -> bool {
    matches!(
        tool.to_ascii_lowercase().as_str(),
        "write" | "edit" | "multiedit" | "notebookedit"
    )
}

/// The brief section of a department with repositories.
pub fn brief(project: &RunProject, run_dir: &Path) -> String {
    let repos = &project.repos;
    let Some(main) = repos.first() else {
        return String::new();
    };
    let mut out = String::from("\n# Проект\n\n");
    match &project.worktree {
        Some(worktree) => out.push_str(&format!(
            "- Репозиторий задачи — `{main}`. Работай в его отдельной копии `{wt}` (локальный клон, ветка `{branch}`): там правь код и коммить в эту ветку. Ветку бот сам заберёт в `{main}` после запуска. Исходную папку `{main}` не трогай, не пушь и не открывай PR. Прочитай CLAUDE.md, AGENTS.md и HANDOFF копии, прежде чем действовать. Исполнитель Codex стартует в рабочей папке задачи: код лежит в её `repo/`.\n",
            main = main.display(),
            wt = worktree.display(),
            branch = project.branch,
        )),
        None => out.push_str(&format!(
            "- Ты работаешь в репозитории хозяина `{}`, это текущая папка. Его CLAUDE.md, HANDOFF и код — твоя база знаний: прочитай, что там сказано про задачу, прежде чем действовать.\n",
            main.display()
        )),
    }
    for repo in &repos[1..] {
        out.push_str(&format!(
            "- Ещё репозиторий: `{}` (его скиллы тоже твои).\n",
            repo.display()
        ));
    }
    out.push_str(&format!(
        "- Рабочая папка задачи — `{}`. Всё, что в этом брифе названо «в рабочей папке» (result.md, questions.md, request.md, memory.md, artifacts/), клади туда по полному пути, а не в репозиторий.\n",
        run_dir.display()
    ));
    if project.worktree.is_none() {
        out.push_str(
            "- Код продукта не меняй, не коммить и не переключай ветки: это рабочая копия хозяина. Разовые скрипты пиши в `scratch/` репозитория, если она там есть, иначе в рабочую папку задачи. Запись в живые системы: сначала прогон без записи, копия старых значений, потом запись.\n",
        );
    }
    if let Some(owner) = owner_memory(main) {
        out.push_str(&format!(
            "\n## Память хозяина по проекту\n\nПапка `{}`: читай файлы оттуда, когда они касаются задачи. Писать туда нельзя.\n",
            owner.display()
        ));
        let index = std::fs::read_to_string(owner.join("MEMORY.md")).unwrap_or_default();
        if !index.trim().is_empty() {
            out.push_str(&format!("\n{}\n", index.trim()));
        }
    }
    if let Some(bot) = bot_memory(main) {
        out.push_str(&format!(
            "\n## Память бота по проекту\n\nПапка `{dir}`, общая для отделов бота. Устроена как память хозяина: один факт в файле `<имя>.md` с первой строкой-заголовком, оглавление в `{dir}/MEMORY.md` (строка на файл: `- [Название](файл.md) — суть`).\n\
             - В конце работы обнови её сам: узнал новое — новый файл и строка в оглавлении; уточнилось — поправь существующий файл; оказалось неверным или устарело — удали файл и строку. Не дублируй то, что уже есть в памяти хозяина или в документах репозитория.\n\
             - Факт стоит знать и самому хозяину в его сессиях — опиши его коротко в {share} в рабочей папке. Хозяин решит, переносить ли его в свою память.\n",
            dir = bot.display(),
            share = SHARE_FILE,
        ));
        let index = std::fs::read_to_string(bot.join("MEMORY.md")).unwrap_or_default();
        if !index.trim().is_empty() {
            out.push_str(&format!("\n{}\n", index.trim()));
        }
    }
    out
}

/// Folders the agent of a department with repositories gets besides the
/// first repository: the other repositories, the run folder and the bot's
/// memory (created if missing).
pub fn add_dirs(repos: &[PathBuf], run_dir: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = repos.iter().skip(1).cloned().collect();
    dirs.push(run_dir.to_path_buf());
    if let Some(bot) = repos.first().and_then(|repo| bot_memory(repo)) {
        let _ = std::fs::create_dir_all(&bot);
        dirs.push(bot);
    }
    dirs
}

/// Writes an approved fact into the owner's memory of the project and adds
/// it to the index. Returns the file written.
pub fn share(repo: &Path, id: &str, text: &str) -> Result<PathBuf> {
    let dir = owner_memory(repo).context("нет папки памяти хозяина")?;
    share_into(&dir, id, text)
}

fn share_into(dir: &Path, id: &str, text: &str) -> Result<PathBuf> {
    let text = text.trim();
    if text.is_empty() {
        bail!("пустой факт");
    }
    std::fs::create_dir_all(dir)?;
    let name = format!("bot-{id}.md");
    let title: String = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim_start_matches(['#', ' ', '-'])
        .chars()
        .take(80)
        .collect();
    let file = dir.join(&name);
    std::fs::write(
        &file,
        format!(
            "---\nname: bot-{id}\ndescription: {title}\nmetadata:\n  type: project\n  source: agentsflow\n---\n\n{text}\n"
        ),
    )?;
    let index = dir.join("MEMORY.md");
    let mut current = std::fs::read_to_string(&index).unwrap_or_default();
    if !current.is_empty() && !current.ends_with('\n') {
        current.push('\n');
    }
    current.push_str(&format!("- [{title}]({name}) — от бота agentsflow\n"));
    std::fs::write(&index, current)?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_matches_claude_code() {
        assert_eq!(slug(Path::new(r"F:\Dev\sbx")), "F--Dev-sbx");
        assert_eq!(slug(Path::new(r"F:\Dev\baitaq-cards")), "F--Dev-baitaq-cards");
    }

    #[test]
    fn owner_memory_is_read_only() {
        let owner = owner_memory(Path::new(r"F:\Dev\sbx")).unwrap();
        let note = owner.join("x.md").to_string_lossy().into_owned();
        assert!(in_owner_memory(&note));
        let bot = bot_memory(Path::new(r"F:\Dev\sbx")).unwrap();
        assert!(!in_owner_memory(&bot.join("x.md").to_string_lossy()));
        assert!(!writes_owner_memory(&format!("cat \"{note}\"")));
        assert!(!writes_owner_memory(&format!("grep -n pool {note} | head")));
        assert!(writes_owner_memory(&format!("echo hi >> {note}")));
        assert!(writes_owner_memory(&format!("rm {note}")));
        assert!(writes_owner_memory(&format!("python -c 'open(r\"{note}\",\"w\")'")));
        assert!(!writes_owner_memory("cat scratch/apply.py"));
    }

    #[test]
    fn brief_names_the_repo_the_run_folder_and_both_memories() {
        let repos = vec![PathBuf::from(r"F:\Dev\sbx"), PathBuf::from(r"F:\Dev\baitaq-cards")];
        let run = Path::new(r"C:\home\departments\baitaq\runs\t1");
        let project = RunProject::new(&repos, None);
        let text = brief(&project, run);
        assert!(text.contains(r"F:\Dev\sbx") && text.contains(r"F:\Dev\baitaq-cards"));
        assert!(text.contains(&run.display().to_string()));
        assert!(text.contains("Писать туда нельзя") && text.contains("bot-memory"));
        assert!(text.contains(SHARE_FILE) && text.contains("Код продукта не меняй"));
        assert_eq!(brief(&RunProject::default(), run), "");
        // The task's choice goes first.
        let chosen = RunProject::new(&repos, Some("baitaq-cards"));
        assert_eq!(chosen.repos[0], repos[1]);
        assert_eq!(find_repo(&repos, "f:/dev/SBX/"), Some(repos[0].clone()));
        assert_eq!(find_repo(&repos, "other"), None);
        let dirs = add_dirs(&repos, run);
        assert_eq!(dirs[0], repos[1]);
        assert_eq!(dirs[1], run);
    }

    #[test]
    fn worktree_is_made_once_on_the_task_branch() {
        let dir = std::env::temp_dir().join(format!("agentsflow-wt-{}", uuid::Uuid::new_v4()));
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git").arg("-C").arg(&repo).args(args).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        };
        git(&["init", "-q"]);
        git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "init"]);
        std::fs::write(repo.join("CLAUDE.md"), "# Тестовый проект\n").unwrap();
        assert_eq!(describe(&repo), "Тестовый проект");
        let run = dir.join("runs/t1");
        std::fs::create_dir_all(&run).unwrap();
        let project = RunProject::new(&[repo.clone()], None).with_worktree(&run, "t1").unwrap();
        assert_eq!(project.worktree.as_deref(), Some(run.join("repo").as_path()));
        assert_eq!(project.branch, "agentsflow/t1");
        assert!(run.join("repo/.git").exists());
        let text = brief(&project, &run);
        assert!(text.contains("agentsflow/t1") && !text.contains("Код продукта не меняй"));
        // A commit in the copy reaches the owner's repository as the branch.
        let copy = run.join("repo");
        let commit = std::process::Command::new("git")
            .arg("-C")
            .arg(&copy)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "fix"])
            .output()
            .unwrap();
        assert!(commit.status.success());
        project.publish().unwrap();
        git(&["rev-parse", "--verify", "refs/heads/agentsflow/t1"]);
        // The next run of the task keeps the same copy.
        RunProject::new(&[repo.clone()], None).with_worktree(&run, "t1").unwrap();
        // A lost copy comes back on the published branch.
        std::fs::remove_dir_all(&copy).unwrap();
        let again = RunProject::new(&[repo.clone()], None).with_worktree(&run, "t1").unwrap();
        let log = std::process::Command::new("git").arg("-C").arg(&copy).args(["log", "--format=%s"]).output().unwrap();
        assert!(String::from_utf8_lossy(&log.stdout).starts_with("fix"));
        again.publish().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn share_writes_a_fact_and_an_index_line() {
        let dir = std::env::temp_dir().join(format!("agentsflow-share-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("MEMORY.md"), "- [Старое](old.md) — было").unwrap();
        let file = share_into(&dir, "ab12", "Рецепт ×2: списание 2 на заказ.\nПодробности.").unwrap();
        let index = std::fs::read_to_string(dir.join("MEMORY.md")).unwrap();
        assert!(index.starts_with("- [Старое]") && index.contains("(bot-ab12.md)"));
        assert!(index.contains("Рецепт ×2"));
        assert!(std::fs::read_to_string(file).unwrap().contains("source: agentsflow"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
