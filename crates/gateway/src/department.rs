//! A department: one directory with its rules, memory, roster and workflow.
//! The roster reuses `team::Member`, the workflow reuses `workflow::Spec`.

use crate::team::{AccessMode, Member, MemberSandbox, Team};
use crate::workflow::Spec;
use agentsflow_events::{AgentId, AgentKind, Risk};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const FILE: &str = "department.toml";
pub const RULES: &str = "AGENTS.md";

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Dollars one task may spend before its run is stopped.
    #[serde(default = "default_task_usd")]
    pub task_usd: f64,
    /// Dollars per calendar month; no limit when absent.
    #[serde(default)]
    pub month_usd: Option<f64>,
    /// Share of `month_usd` after which the owner is warned once.
    #[serde(default = "default_warn_ratio")]
    pub warn_ratio: f64,
    #[serde(default = "default_timeout_mins")]
    pub timeout_mins: u64,
    /// Default `max_loops` for steps that do not set their own.
    #[serde(default = "default_max_loops")]
    pub max_loops: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            task_usd: default_task_usd(),
            month_usd: None,
            warn_ratio: default_warn_ratio(),
            timeout_mins: default_timeout_mins(),
            max_loops: default_max_loops(),
        }
    }
}
fn default_task_usd() -> f64 {
    3.0
}
fn default_warn_ratio() -> f64 {
    0.8
}
fn default_timeout_mins() -> u64 {
    60
}
fn default_max_loops() -> usize {
    2
}
fn default_parallel() -> usize {
    1
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Schedule {
    pub cron: String,
    pub task: String,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    id: String,
    title: String,
    description: String,
    #[serde(default = "default_parallel")]
    max_parallel: usize,
    #[serde(default)]
    default_risk: Risk,
    #[serde(default)]
    max_steps: Option<usize>,
    #[serde(default)]
    limits: Limits,
    #[serde(rename = "agent")]
    agents: Vec<Member>,
    #[serde(rename = "step")]
    steps: Vec<toml::Table>,
    #[serde(default, rename = "schedule")]
    schedules: Vec<Schedule>,
}

#[derive(Debug, Clone)]
pub struct Department {
    pub id: String,
    pub title: String,
    pub description: String,
    pub max_parallel: usize,
    pub default_risk: Risk,
    pub limits: Limits,
    /// Roster with defaults applied: `smart` access, Codex may write its run folder.
    pub team: Team,
    pub workflow: Spec,
    pub schedules: Vec<Schedule>,
    pub dir: PathBuf,
}

/// What one run needs: the workflow and roster with ids unique to the run,
/// so two runs of one department never see each other's events.
pub struct RunPlan {
    pub spec: Spec,
    pub team: Team,
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 24
        && id
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
}

pub fn agent_id(department: &str, member: &str, task: &str) -> AgentId {
    AgentId::from(format!("{department}:{member}:{task}"))
}

impl Department {
    pub fn parse(text: &str, dir: &Path) -> Result<Self> {
        let raw: Raw = toml::from_str(text).context("invalid department TOML")?;
        if !valid_id(&raw.id) {
            bail!(
                "invalid department id {:?}: 1-24 characters a-z, 0-9, _ and -",
                raw.id
            );
        }
        if raw.title.trim().is_empty() {
            bail!("title is empty");
        }
        if raw.description.trim().is_empty() {
            bail!("description is empty: the orchestrator routes tasks by it");
        }
        if !(1..=10).contains(&raw.max_parallel) {
            bail!("max_parallel must be 1..=10");
        }
        let limits = &raw.limits;
        if !(limits.task_usd > 0.0 && limits.task_usd <= 1000.0) {
            bail!("limits.task_usd must be in (0, 1000]");
        }
        if limits
            .month_usd
            .is_some_and(|month| !(month > 0.0 && month <= 100_000.0))
        {
            bail!("limits.month_usd must be in (0, 100000]");
        }
        if !(limits.warn_ratio > 0.0 && limits.warn_ratio <= 1.0) {
            bail!("limits.warn_ratio must be in (0, 1]");
        }
        if !(1..=1440).contains(&limits.timeout_mins) {
            bail!("limits.timeout_mins must be 1..=1440");
        }
        if limits.max_loops > 10 {
            bail!("limits.max_loops must be 0..=10");
        }
        if !(1..=3).contains(&raw.agents.len()) {
            bail!("a department has 1..=3 agents");
        }
        let mut members = raw.agents;
        for member in &mut members {
            if member.mode.is_none() {
                member.mode = Some(AccessMode::Smart);
            }
            if member.kind == AgentKind::Codex
                && member.sandbox.is_none()
                && member.mode != Some(AccessMode::Bypass)
            {
                member.sandbox = Some(MemberSandbox::WorkspaceWrite);
            }
        }
        if !members.iter().any(|m| m.roles.iter().any(|r| r == "doer")) {
            bail!("no agent has role doer");
        }
        if !members.iter().any(|m| m.lead) {
            let doer = members
                .iter_mut()
                .find(|m| m.roles.iter().any(|r| r == "doer"))
                .expect("checked above");
            doer.lead = true;
        }
        let team = Team { members };
        team.validate()?;
        for schedule in &raw.schedules {
            if schedule.cron.trim().is_empty() || schedule.task.trim().is_empty() {
                bail!("schedule needs cron and task");
            }
            crate::schedule::Cron::parse(&schedule.cron)
                .with_context(|| format!("schedule {:?}", schedule.cron))?;
        }
        let mut steps = raw.steps;
        for step in &mut steps {
            if !step.contains_key("max_loops") {
                step.insert(
                    "max_loops".into(),
                    toml::Value::Integer(limits.max_loops as i64),
                );
            }
        }
        let mut table = toml::Table::new();
        table.insert("name".into(), raw.id.clone().into());
        if let Some(max_steps) = raw.max_steps {
            table.insert("max_steps".into(), toml::Value::Integer(max_steps as i64));
        }
        table.insert(
            "step".into(),
            toml::Value::Array(steps.into_iter().map(toml::Value::Table).collect()),
        );
        let workflow = Spec::parse(&toml::to_string(&table)?).context("workflow")?;
        workflow.resolve_team(&team).context("workflow")?;
        Ok(Self {
            id: raw.id,
            title: raw.title.trim().into(),
            description: raw.description.trim().into(),
            max_parallel: raw.max_parallel,
            default_risk: raw.default_risk,
            limits: raw.limits,
            team,
            workflow,
            schedules: raw.schedules,
            dir: dir.to_path_buf(),
        })
    }

    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join(FILE);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let department =
            Self::parse(&text, dir).with_context(|| format!("invalid {}", path.display()))?;
        let name = dir.file_name().unwrap_or_default().to_string_lossy();
        if department.id != name {
            bail!(
                "{}: id {:?} differs from the folder name {:?}",
                path.display(),
                department.id,
                name
            );
        }
        Ok(department)
    }

    pub fn rules(&self) -> String {
        std::fs::read_to_string(self.dir.join(RULES)).unwrap_or_default()
    }

    pub fn memory_dir(&self) -> PathBuf {
        self.dir.join("memory")
    }

    pub fn run_dir(&self, task: &str) -> PathBuf {
        self.dir.join("runs").join(task)
    }

    /// Warnings for `home check`: things that work but are probably a mistake.
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        let with_role = |role: &'static str| {
            self.team
                .members
                .iter()
                .filter(move |m| m.roles.iter().any(|r| r == role))
        };
        let reviewers: Vec<_> = with_role("reviewer").collect();
        if reviewers.is_empty() {
            warnings.push(format!("{}: no reviewer, results are not checked", self.id));
        }
        for doer in with_role("doer") {
            for reviewer in &reviewers {
                if doer.id != reviewer.id
                    && doer.kind == reviewer.kind
                    && doer.model == reviewer.model
                {
                    warnings.push(format!(
                        "{}: reviewer {} is the same model as doer {}; a different model reviews better",
                        self.id, reviewer.id, doer.id
                    ));
                }
            }
        }
        for member in &self.team.members {
            if member.mode == Some(AccessMode::Bypass) {
                warnings.push(format!(
                    "{}: {} runs in bypass, guard is off",
                    self.id, member.id
                ));
            }
        }
        warnings
    }

    /// Roster and workflow for one task. Literal member ids in steps are
    /// rewritten to the run ids; `@role` addresses resolve as before.
    pub fn run_plan(&self, task: &str) -> Result<RunPlan> {
        let rename = |member: &str| agent_id(&self.id, member, task).to_string();
        let mut team = self.team.clone();
        for member in &mut team.members {
            member.id = AgentId::from(rename(member.id.as_str()));
        }
        let mut spec = self.workflow.clone();
        for step in &mut spec.step {
            if !step.agent.starts_with('@') {
                step.agent = rename(&step.agent);
            }
            for cc in &mut step.cc {
                if !cc.starts_with('@') {
                    *cc = rename(cc);
                }
            }
        }
        let spec = spec.resolve_team(&team)?;
        Ok(RunPlan { spec, team })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub const SAMPLE: &str = r#"
id = "pr"
title = "Пиар"
description = "Посты и анонсы"

[limits]
task_usd = 2.5
max_loops = 1

[[agent]]
id = "writer"
kind = "claude"
roles = ["doer"]

[[agent]]
id = "critic"
kind = "codex"
roles = ["reviewer"]

[[step]]
id = "do"
agent = "@doer"
prompt = "{brief}"

[[step]]
id = "review"
agent = "critic"
prompt = "{brief}\n{prev.output}"
until = "VERDICT: APPROVED"
otherwise = "fix"
next = "end"

[[step]]
id = "fix"
agent = "@doer"
prompt = "{review.output}"
next = "review"
"#;

    #[test]
    fn parses_with_defaults() {
        let dept = Department::parse(SAMPLE, Path::new("pr")).unwrap();
        assert_eq!(dept.id, "pr");
        assert_eq!(dept.max_parallel, 1);
        assert_eq!(dept.default_risk, Risk::External);
        assert_eq!(dept.limits.task_usd, 2.5);
        assert_eq!(dept.limits.timeout_mins, 60);
        let writer = &dept.team.members[0];
        assert!(writer.lead);
        assert_eq!(writer.mode, Some(AccessMode::Smart));
        assert_eq!(
            dept.team.members[1].sandbox,
            Some(MemberSandbox::WorkspaceWrite)
        );
        assert!(dept.workflow.step.iter().all(|step| step.max_loops == 1));
        assert!(dept.warnings().is_empty());
    }

    #[test]
    fn run_plan_uses_unique_ids() {
        let dept = Department::parse(SAMPLE, Path::new("pr")).unwrap();
        let plan = dept.run_plan("t1").unwrap();
        assert_eq!(plan.spec.step[0].agent, "pr:writer:t1");
        assert_eq!(plan.spec.step[1].agent, "pr:critic:t1");
        assert!(plan.team.get(&AgentId::from("pr:critic:t1")).is_some());
    }

    #[test]
    fn rejects_broken_departments() {
        for (bad, error) in [
            (
                SAMPLE.replace("id = \"pr\"", "id = \"PR\""),
                "invalid department id",
            ),
            (
                SAMPLE.replace("Посты и анонсы", " "),
                "description is empty",
            ),
            (
                SAMPLE.replace("roles = [\"doer\"]", "roles = []"),
                "no agent has role doer",
            ),
            (
                SAMPLE.replace("agent = \"critic\"", "agent = \"ghost\""),
                "not in the team",
            ),
            (SAMPLE.replace("task_usd = 2.5", "task_usd = 0"), "task_usd"),
            (
                SAMPLE.replace("[limits]", "bogus = 1\n[limits]"),
                "invalid department TOML",
            ),
            (
                SAMPLE.replace("next = \"review\"", "next = \"nowhere\""),
                "unknown step target",
            ),
        ] {
            let error_text = format!(
                "{:#}",
                Department::parse(&bad, Path::new("pr")).unwrap_err()
            );
            assert!(error_text.contains(error), "{error}: {error_text}");
        }
    }

    #[test]
    fn warns_about_same_model_review() {
        let same = SAMPLE.replace("kind = \"codex\"", "kind = \"claude\"");
        let dept = Department::parse(&same, Path::new("pr")).unwrap();
        assert!(dept.warnings()[0].contains("same model"));
    }
}
