//! The agentsflow home: global settings, the orchestrator's rules and the
//! departments. It is a git repository so rule changes can be reviewed and
//! rolled back.

use crate::department::{self, Department};
use agentsflow_events::AgentKind;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
};

pub const CONFIG: &str = "agentsflow.toml";

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Price {
    /// Dollars per million tokens.
    pub input: f64,
    #[serde(default)]
    pub cached_input: Option<f64>,
    pub output: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prices {
    /// Codex does not report cost; it is computed from tokens and this table.
    #[serde(default)]
    pub codex: HashMap<String, Price>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestratorConfig {
    #[serde(default = "default_kind")]
    pub kind: AgentKind,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    /// Off: every result goes to the owner, messages are not parsed.
    #[serde(default = "enabled")]
    pub enabled: bool,
}
impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            kind: default_kind(),
            model: None,
            effort: None,
            enabled: true,
        }
    }
}
fn enabled() -> bool {
    true
}
fn default_kind() -> AgentKind {
    AgentKind::Claude
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_parallel_runs")]
    pub max_parallel_runs: usize,
    #[serde(default = "default_digest_kb")]
    pub memory_digest_kb: usize,
    #[serde(default)]
    pub orchestrator: OrchestratorConfig,
    #[serde(default)]
    pub prices: Prices,
    /// Local time of schedules, like `+05:00`.
    #[serde(default = "default_offset")]
    pub utc_offset: String,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            max_parallel_runs: default_parallel_runs(),
            memory_digest_kb: default_digest_kb(),
            orchestrator: OrchestratorConfig::default(),
            prices: Prices::default(),
            utc_offset: default_offset(),
        }
    }
}
fn default_parallel_runs() -> usize {
    3
}
fn default_digest_kb() -> usize {
    8
}

fn default_offset() -> String {
    "+00:00".into()
}

impl Config {
    /// Minutes east of UTC for schedules.
    pub fn offset_minutes(&self) -> i32 {
        crate::schedule::parse_offset(&self.utc_offset).unwrap_or(0)
    }

    pub fn parse(text: &str) -> Result<Self> {
        let config: Self = toml::from_str(text).context("invalid agentsflow.toml")?;
        crate::schedule::parse_offset(&config.utc_offset).context("utc_offset")?;
        if !(1..=20).contains(&config.max_parallel_runs) {
            bail!("max_parallel_runs must be 1..=20");
        }
        if !(1..=256).contains(&config.memory_digest_kb) {
            bail!("memory_digest_kb must be 1..=256");
        }
        for (model, price) in &config.prices.codex {
            let values = [Some(price.input), price.cached_input, Some(price.output)];
            if values
                .into_iter()
                .flatten()
                .any(|v| !(v >= 0.0 && v.is_finite()))
            {
                bail!("prices.codex.{model}: prices must be non-negative numbers");
            }
        }
        Ok(config)
    }
}

#[derive(Debug, Clone)]
pub struct Home {
    pub root: PathBuf,
    pub config: Config,
    pub departments: BTreeMap<String, Department>,
}

impl Home {
    pub fn load(root: &Path) -> Result<Self> {
        let root = std::path::absolute(root)?;
        let config_path = root.join(CONFIG);
        let text = std::fs::read_to_string(&config_path).with_context(|| {
            format!(
                "cannot read {}; create a home with `agentsflow home init {}`",
                config_path.display(),
                root.display()
            )
        })?;
        let config = Config::parse(&text)?;
        let mut departments = BTreeMap::new();
        let mut errors = Vec::new();
        let dir = root.join("departments");
        let entries =
            std::fs::read_dir(&dir).with_context(|| format!("cannot read {}", dir.display()))?;
        let mut paths: Vec<_> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.join(department::FILE).is_file())
            .collect();
        paths.sort();
        for path in paths {
            match Department::load(&path) {
                Ok(dept) => {
                    departments.insert(dept.id.clone(), dept);
                }
                Err(error) => errors.push(format!("{error:#}")),
            }
        }
        if !errors.is_empty() {
            bail!("{}", errors.join("\n"));
        }
        if !departments.contains_key("general") {
            bail!("department general is missing: it takes tasks no other department fits");
        }
        Ok(Self {
            root,
            config,
            departments,
        })
    }

    pub fn orchestrator_dir(&self) -> PathBuf {
        self.root.join("orchestrator")
    }

    pub fn orchestrator_rules(&self) -> String {
        std::fs::read_to_string(self.orchestrator_dir().join(department::RULES)).unwrap_or_default()
    }

    pub fn state_dir(&self) -> PathBuf {
        self.root.join(".agentsflow")
    }

    /// Warnings for `home check`.
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        for dept in self.departments.values() {
            warnings.extend(dept.warnings());
            for member in &dept.team.members {
                if member.kind != AgentKind::Codex {
                    continue;
                }
                let priced = member
                    .model
                    .as_ref()
                    .is_some_and(|model| self.config.prices.codex.contains_key(model));
                if !priced {
                    warnings.push(format!(
                        "{}: codex {} has no price in prices.codex, its spending is not counted",
                        dept.id,
                        member.model.as_deref().unwrap_or("(default model)")
                    ));
                }
            }
        }
        warnings
    }
}

/// Commits the given paths in the home repository. Git is optional: without
/// it the home still works, only history is missing.
pub fn commit(root: &Path, paths: &[&Path], message: &str) -> Result<()> {
    let git = |args: &[&std::ffi::OsStr]| -> Result<()> {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .context("git is not available")?;
        if !output.status.success() {
            bail!(
                "git {:?}: {}",
                args,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    };
    let mut add: Vec<&std::ffi::OsStr> = vec!["add".as_ref(), "--".as_ref()];
    add.extend(paths.iter().map(|path| path.as_os_str()));
    git(&add)?;
    git(&[
        "-c".as_ref(),
        "user.name=agentsflow".as_ref(),
        "-c".as_ref(),
        "user.email=agentsflow@localhost".as_ref(),
        "commit".as_ref(),
        "-q".as_ref(),
        "--allow-empty".as_ref(),
        "-m".as_ref(),
        message.as_ref(),
    ])
}

fn write_new(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("{} already exists", path.display()))?;
    file.write_all(text.as_bytes())?;
    Ok(())
}

pub fn init(root: &Path) -> Result<()> {
    if root.join(CONFIG).exists() {
        bail!(
            "{} already has {CONFIG}; existing homes are not overwritten",
            root.display()
        );
    }
    std::fs::create_dir_all(root)?;
    let general = root.join("departments").join("general");
    let files = [
        (root.join(CONFIG), CONFIG_TEMPLATE),
        (root.join(".gitignore"), GITIGNORE),
        (
            root.join("orchestrator").join(department::RULES),
            ORCHESTRATOR_RULES,
        ),
        (general.join(department::FILE), GENERAL),
        (general.join(department::RULES), GENERAL_RULES),
        (general.join("memory").join("notes.md"), NOTES),
    ];
    for (path, text) in &files {
        write_new(path, text)?;
    }
    Home::load(root).context("the new home does not load")?;
    let git_ready = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["init", "-q"])
        .status()
        .is_ok_and(|status| status.success());
    if git_ready {
        commit(root, &[Path::new(".")], "agentsflow home")?;
    } else {
        println!("git не найден: история правил вестись не будет");
    }
    println!(
        "{}\nзапустите: agentsflow serve --home {}",
        root.display(),
        root.display()
    );
    Ok(())
}

/// Writes a new department from an approved proposal: the roster and
/// workflow of the general department, the proposal's title, description and
/// rules. Returns the department folder.
/// Sets `limits.month_usd` of a department, keeping the rest of the file as
/// the owner wrote it, and commits the change.
pub fn set_month_limit(root: &Path, id: &str, usd: f64) -> Result<()> {
    if !department::valid_id(id) {
        bail!("недопустимый id отдела");
    }
    let dir = root.join("departments").join(id);
    let path = dir.join(department::FILE);
    let text = std::fs::read_to_string(&path).with_context(|| format!("нет отдела {id}"))?;
    let line = format!("month_usd = {usd:.2}");
    let mut out = Vec::new();
    let mut section = String::new();
    let mut done = false;
    for raw in text.lines() {
        let trimmed = raw.trim();
        if trimmed.starts_with('[') {
            if section == "[limits]" && !done {
                out.push(line.clone());
                done = true;
            }
            section = trimmed.to_owned();
        } else if section == "[limits]" && trimmed.starts_with("month_usd") {
            if !done {
                out.push(line.clone());
                done = true;
            }
            continue;
        }
        out.push(raw.to_owned());
    }
    if !done {
        if section != "[limits]" {
            out.push(String::new());
            out.push("[limits]".into());
        }
        out.push(line);
    }
    let mut updated = out.join("\n");
    updated.push('\n');
    Department::parse(&updated, &dir).context("лимит не записан")?;
    std::fs::write(&path, updated)?;
    if let Err(error) = commit(
        root,
        &[&path],
        &format!("limits({id}): month_usd = {usd:.2}"),
    ) {
        crate::server_diag!("limit commit failed: {error:#}");
    }
    Ok(())
}

pub fn create_department(
    root: &Path,
    proposal: &agentsflow_events::DepartmentProposal,
) -> Result<PathBuf> {
    if !department::valid_id(&proposal.id) {
        bail!("invalid department id {:?}", proposal.id);
    }
    let dir = root.join("departments").join(&proposal.id);
    if dir.exists() {
        bail!("department {} already exists", proposal.id);
    }
    let quote = |text: &str| toml::Value::String(text.trim().to_owned()).to_string();
    let risk = match proposal.default_risk {
        agentsflow_events::Risk::Internal => "internal",
        agentsflow_events::Risk::External => "external",
    };
    let body = GENERAL
        .split_once("\n[limits]")
        .map(|(_, rest)| format!("\n[limits]{rest}"))
        .expect("template has limits");
    let text = format!(
        "id = {}\ntitle = {}\ndescription = {}\ndefault_risk = \"{risk}\"\nmax_parallel = 1\n{body}",
        quote(&proposal.id),
        quote(&proposal.title),
        quote(&proposal.description),
    );
    let rules = if proposal.rules.trim().is_empty() {
        format!(
            "# Правила отдела «{}»\n\n{}",
            proposal.title.trim(),
            GENERAL_RULES
                .split_once('\n')
                .map_or("", |(_, rest)| rest)
                .trim_start()
        )
    } else {
        format!("{}\n", proposal.rules.trim())
    };
    let created = (|| -> Result<()> {
        write_new(&dir.join(department::FILE), &text)?;
        write_new(&dir.join(department::RULES), &rules)?;
        write_new(&dir.join("memory").join("notes.md"), NOTES)?;
        Department::load(&dir)?;
        Ok(())
    })();
    if let Err(error) = created {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(error);
    }
    if let Err(error) = commit(
        root,
        &[&dir],
        &format!("department({}): {}", proposal.id, proposal.title.trim()),
    ) {
        crate::server_diag!("department commit: {error:#}");
    }
    Ok(dir)
}

pub fn print_check(home: &Home) {
    println!(
        "дом: {} · одновременно запусков: {}",
        home.root.display(),
        home.config.max_parallel_runs
    );
    println!("отдел\tагенты\tлимит задачи\tлимит месяца\tпараллельно");
    for dept in home.departments.values() {
        let agents = dept
            .team
            .members
            .iter()
            .map(|m| {
                let kind = match m.kind {
                    AgentKind::Claude => "claude",
                    AgentKind::Codex => "codex",
                };
                format!(
                    "{}({kind}{}; {})",
                    m.id,
                    m.model
                        .as_deref()
                        .map(|model| format!(" {model}"))
                        .unwrap_or_default(),
                    m.roles.join(",")
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        println!(
            "{}\t{agents}\t${:.2}\t{}\t{}",
            dept.id,
            dept.limits.task_usd,
            dept.limits
                .month_usd
                .map_or_else(|| "-".into(), |month| format!("${month:.2}")),
            dept.max_parallel
        );
    }
    for warning in home.warnings() {
        println!("WARNING: {warning}");
    }
}

pub fn run_cli(args: &[String]) -> Result<()> {
    match args {
        [command, dir] if command == "init" => init(Path::new(dir)),
        [command] if command == "check" => {
            print_check(&Home::load(Path::new("."))?);
            println!("OK");
            Ok(())
        }
        [command, dir] if command == "check" => {
            print_check(&Home::load(Path::new(dir))?);
            println!("OK");
            Ok(())
        }
        _ => bail!("Usage: agentsflow home init <DIR> | home check [DIR]"),
    }
}

const CONFIG_TEMPLATE: &str = r#"# Настройки agentsflow. Отделы лежат в departments/<id>/.
# Сколько задач может идти одновременно во всех отделах.
max_parallel_runs = 3
# Сколько килобайт памяти отдела (хвост memory/notes.md) получает агент.
memory_digest_kb = 8

# Часовой пояс расписаний отделов (cron в department.toml), например "+05:00".
utc_offset = "+00:00"

[orchestrator]
kind = "claude"
model = "claude-sonnet-5-5"
effort = "medium"

# Codex не сообщает стоимость: она считается по токенам и этим ценам
# (долларов за миллион токенов). Модель без цены не учитывается в лимитах.
# [prices.codex."<модель>"]
# input = 0.0
# cached_input = 0.0
# output = 0.0
"#;

// Rule proposals wait for the owner; only approved rules are history.
const GITIGNORE: &str = ".agentsflow/\ndepartments/*/runs/\ndepartments/*/memory/proposals/\norchestrator/memory/proposals/\n";

const ORCHESTRATOR_RULES: &str = r#"# Правила оркестратора

Ты диспетчер и приёмщик. Сам работу не делаешь: разбираешь сообщения хозяина на задачи, раскладываешь их по отделам, принимаешь результат и коротко отчитываешься.

## Разбор сообщения
- Одно сообщение может содержать несколько задач разного формата. Каждая задача — отдельная карточка.
- Отдел выбирай по описанию отделов. Если ни один не подходит, предложи новый отдел; для разовой задачи без своего отдела используй `general`.
- Если без ответа хозяина результат будет угадыванием, задай вопросы одним сообщением. Не спрашивай то, что можно разумно предположить; предположения запиши в задачу.
- Если задача зависит от результата другой, укажи зависимость.

## Риск
- `internal` — отчёт, анализ, черновик, который остаётся у хозяина.
- `external` — то, что уходит наружу: пост, письмо, изменение в чужой системе.
- Сомневаешься — ставь `external`.

## Приёмка
- `internal`: сверь результат с задачей. Подходит — прими. Не подходит — верни один раз с конкретной причиной.
- `external`: всегда отдай хозяину на приёмку.

## Правила
- Хозяин просит «запомни» или повторяет одно и то же указание — предложи правило отделу (или себе, `orchestrator`) действием `propose_rule`. Правила меняет только хозяин.

## Как писать хозяину
Коротко, по-русски, без воды: что разложил, что нужно от хозяина, что готово, что застряло.
"#;

const GENERAL: &str = r#"id = "general"
title = "Общий отдел"
description = "Разовые задачи, для которых нет своего отдела: найти, разобраться, написать черновик, посчитать."
default_risk = "internal"
max_parallel = 1

[limits]
task_usd = 3.0
timeout_mins = 60
max_loops = 2

[[agent]]
id = "doer"
kind = "claude"
title = "исполнитель"
roles = ["doer"]

[[agent]]
id = "reviewer"
kind = "codex"
title = "ревьюер"
roles = ["reviewer"]

[[step]]
id = "do"
agent = "@doer"
prompt = "{brief}"

[[step]]
id = "review"
agent = "@reviewer"
prompt = """{brief}

Результат исполнителя:
{prev.output}

Проверь результат по задаче и правилам отдела, файл result.md в текущей папке тоже. Если всё верно, последней строкой напиши ровно VERDICT: APPROVED. Иначе перечисли конкретные замечания."""
until = "VERDICT: APPROVED"
otherwise = "fix"
next = "end"

[[step]]
id = "fix"
agent = "@doer"
prompt = """Замечания ревьюера:
{review.output}

Исправь результат и обнови result.md."""
next = "review"
"#;

const GENERAL_RULES: &str = r#"# Правила общего отдела

- Пиши по-русски, коротко и по делу.
- Итог работы всегда клади в `result.md` в текущей папке: сначала вывод в двух-трёх предложениях, потом подробности.
- Файлы результата (таблицы, черновики) клади в `artifacts/`.
- Ничего не отправляй наружу и не меняй в чужих системах: готовь черновик, отправит хозяин.
- Если чего-то не знаешь и не можешь проверить, так и напиши, не выдумывай.
"#;

const NOTES: &str =
    "# Память отдела\n\nЗаметки агентов: что узнали, где что лежит, что не сработало.\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        std::env::temp_dir().join(format!("agentsflow-home-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn init_creates_a_loadable_home_and_does_not_overwrite() {
        let root = temp();
        init(&root).unwrap();
        let home = Home::load(&root).unwrap();
        assert_eq!(home.config.max_parallel_runs, 3);
        assert!(home.departments.contains_key("general"));
        assert!(home.orchestrator_rules().contains("Риск"));
        assert!(init(&root).is_err());
        assert!(
            home.warnings()
                .iter()
                .any(|warning| warning.contains("no price"))
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn load_reports_every_broken_department() {
        let root = temp();
        init(&root).unwrap();
        for name in ["a", "b"] {
            let dir = root.join("departments").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(department::FILE), "id = 1").unwrap();
        }
        let error = format!("{:#}", Home::load(&root).unwrap_err());
        assert!(error.contains("departments") && error.matches("invalid").count() >= 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn config_rejects_bad_values() {
        assert!(Config::parse("max_parallel_runs = 0").is_err());
        assert!(Config::parse("[prices.codex.m]\ninput = -1\noutput = 1").is_err());
        assert!(Config::parse("bogus = 1").is_err());
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn month_limit_is_written_into_limits() {
        let root = temp();
        init(&root).unwrap();
        set_month_limit(&root, "general", 10.0).unwrap();
        let home = Home::load(&root).unwrap();
        assert_eq!(home.departments["general"].limits.month_usd, Some(10.0));
        set_month_limit(&root, "general", 15.0).unwrap();
        let text = std::fs::read_to_string(root.join("departments/general").join(department::FILE))
            .unwrap();
        assert_eq!(text.matches("month_usd").count(), 1);
        assert!(text.contains("month_usd = 15.00"));
        assert!(set_month_limit(&root, "general", -1.0).is_err());
        assert!(set_month_limit(&root, "nope", 1.0).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn approved_proposal_becomes_a_department() {
        let root = temp();
        init(&root).unwrap();
        let proposal = agentsflow_events::DepartmentProposal {
            id: "hr".into(),
            title: "Кадры".into(),
            description: "Найм".into(),
            rules: String::new(),
            default_risk: agentsflow_events::Risk::External,
        };
        create_department(&root, &proposal).unwrap();
        let home = Home::load(&root).unwrap();
        let hr = &home.departments["hr"];
        assert_eq!(hr.title, "Кадры");
        assert_eq!(hr.default_risk, agentsflow_events::Risk::External);
        assert!(!hr.rules().trim().is_empty());
        assert!(create_department(&root, &proposal).is_err());
        let bad = agentsflow_events::DepartmentProposal {
            id: "Bad Id".into(),
            ..proposal
        };
        assert!(create_department(&root, &bad).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
