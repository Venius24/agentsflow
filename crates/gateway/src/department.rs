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

/// Who does a run. `small` and `fallback` are spare roles: those agents take
/// no part in a normal run and start only for their case.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RunKind {
    Full,
    /// A small task: the agent of role `small` alone, without review.
    Small,
    /// The doer hit its limit: the agent of role `fallback` works in its place.
    Fallback,
}

fn has_role(member: &Member, role: &str) -> bool {
    member.roles.iter().any(|r| r == role)
}

fn is_spare(member: &Member) -> bool {
    has_role(member, "small") || has_role(member, "fallback")
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
        if !(1..=4).contains(&raw.agents.len()) {
            bail!("a department has 1..=4 agents");
        }
        let mut members = raw.agents;
        for member in &mut members {
            if member.mode.is_none() {
                member.mode = Some(AccessMode::Smart);
            }
            if member.sandbox.is_none() && member.mode != Some(AccessMode::Bypass) {
                // A pure reviewer only reads: given write access it follows the
                // department rule "put the result into result.md" and overwrites
                // the doer's result with its review.
                let reviewer_only = has_role(member, "reviewer") && !has_role(member, "doer");
                if reviewer_only {
                    member.sandbox = Some(MemberSandbox::ReadOnly);
                } else if member.kind == AgentKind::Codex {
                    member.sandbox = Some(MemberSandbox::WorkspaceWrite);
                }
            }
        }
        if members.iter().any(|m| is_spare(m) && m.lead) {
            bail!("agents of roles small and fallback cannot lead");
        }
        if members
            .iter()
            .any(|m| is_spare(m) && (has_role(m, "doer") || has_role(m, "reviewer")))
        {
            bail!("roles small and fallback do not mix with doer and reviewer");
        }
        if !members.iter().any(|m| has_role(m, "doer")) {
            bail!("no agent has role doer");
        }
        if !members.iter().any(|m| m.lead) {
            let doer = members
                .iter_mut()
                .find(|m| has_role(m, "doer"))
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
        let working = Team {
            members: team
                .members
                .iter()
                .filter(|m| !is_spare(m))
                .cloned()
                .collect(),
        };
        workflow.resolve_team(&working).context("workflow")?;
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

    fn with_role(&self, role: &str) -> Option<&Member> {
        self.team.members.iter().find(|m| has_role(m, role))
    }

    /// The kind of run for a task; a department without the spare agent runs
    /// as usual, and the fallback replaces only a Codex doer.
    pub fn run_kind(&self, small: bool, fallback: bool) -> RunKind {
        if small && self.with_role("small").is_some() {
            RunKind::Small
        } else if fallback && self.can_fall_back() {
            RunKind::Fallback
        } else {
            RunKind::Full
        }
    }

    pub fn can_fall_back(&self) -> bool {
        self.with_role("fallback").is_some()
            && self
                .team
                .members
                .iter()
                .any(|m| has_role(m, "doer") && m.kind == AgentKind::Codex)
    }

    /// Short name of the fallback agent for the owner's notice.
    pub fn fallback_name(&self) -> String {
        self.with_role("fallback")
            .map(|m| m.model.clone().unwrap_or_else(|| m.id.to_string()))
            .unwrap_or_default()
    }

    /// Only the agents this run needs, so the others never start.
    fn run_team(&self, kind: RunKind) -> Team {
        let mut members: Vec<Member> = match kind {
            RunKind::Small => {
                let mut small = self.with_role("small").expect("run_kind checked").clone();
                small.roles = vec!["doer".into()];
                small.lead = true;
                vec![small]
            }
            RunKind::Full | RunKind::Fallback => self
                .team
                .members
                .iter()
                .filter(|m| !is_spare(m))
                .cloned()
                .collect(),
        };
        if kind == RunKind::Fallback {
            let spare = self.with_role("fallback").expect("run_kind checked");
            for member in &mut members {
                // Keeps the id and roles: steps may name the doer literally.
                if has_role(member, "doer") && member.kind == AgentKind::Codex {
                    member.kind = spare.kind;
                    member.model = spare.model.clone();
                    member.effort = spare.effort.clone();
                    member.mode = spare.mode;
                    member.sandbox = spare.sandbox;
                }
            }
        }
        Team { members }
    }

    /// Roster and workflow for one task. Literal member ids in steps are
    /// rewritten to the run ids; `@role` addresses resolve as before.
    pub fn run_plan(&self, task: &str, kind: RunKind) -> Result<RunPlan> {
        let rename = |member: &str| agent_id(&self.id, member, task).to_string();
        let mut team = self.run_team(kind);
        for member in &mut team.members {
            member.id = AgentId::from(rename(member.id.as_str()));
        }
        let mut spec = if kind == RunKind::Small {
            Spec::parse(&format!(
                "name = \"{}\"\n[[step]]\nid = \"do\"\nagent = \"@doer\"\nprompt = \"{{brief}}\"\n",
                self.id
            ))?
        } else {
            self.workflow.clone()
        };
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
        assert_eq!(dept.team.members[1].sandbox, Some(MemberSandbox::ReadOnly));
        let doer = SAMPLE.replace("roles = [\"reviewer\"]", "roles = [\"doer\"]");
        let doer = Department::parse(&doer, Path::new("pr")).unwrap();
        assert_eq!(
            doer.team.members[1].sandbox,
            Some(MemberSandbox::WorkspaceWrite)
        );
        assert!(dept.workflow.step.iter().all(|step| step.max_loops == 1));
        assert!(dept.warnings().is_empty());
    }

    #[test]
    fn run_plan_uses_unique_ids() {
        let dept = Department::parse(SAMPLE, Path::new("pr")).unwrap();
        let plan = dept.run_plan("t1", RunKind::Full).unwrap();
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

    const SPARES: &str = r#"
[[agent]]
id = "fast"
kind = "claude"
model = "claude-haiku-5-5"
effort = "high"
roles = ["small"]

[[agent]]
id = "spare"
kind = "claude"
model = "claude-sonnet-5-5"
effort = "medium"
roles = ["fallback"]
"#;

    /// Codex writes, Claude reviews, Haiku takes small tasks, Sonnet the spare.
    pub fn sample_with_spares() -> String {
        SAMPLE
            .replace(
                "id = \"writer\"\nkind = \"claude\"",
                "id = \"writer\"\nkind = \"codex\"",
            )
            .replace(
                "id = \"critic\"\nkind = \"codex\"",
                "id = \"critic\"\nkind = \"claude\"",
            )
            .replace(
                "[[step]]\nid = \"do\"",
                &format!("{SPARES}\n[[step]]\nid = \"do\""),
            )
    }

    fn with_spares() -> Department {
        Department::parse(&sample_with_spares(), Path::new("pr")).unwrap()
    }

    #[test]
    fn claude_reviewer_only_reads() {
        let dept = with_spares();
        assert_eq!(dept.team.members[1].sandbox, Some(MemberSandbox::ReadOnly));
        assert_eq!(
            dept.team.members[0].sandbox,
            Some(MemberSandbox::WorkspaceWrite)
        );
        // Spare agents write as any Claude does.
        assert_eq!(dept.team.members[2].sandbox, None);
    }

    #[test]
    fn spare_agents_start_only_for_their_case() {
        let dept = with_spares();
        assert_eq!(dept.run_kind(false, false), RunKind::Full);
        let full = dept.run_plan("t1", RunKind::Full).unwrap();
        assert_eq!(full.team.members.len(), 2);
        assert_eq!(full.spec.step.len(), 3);

        assert_eq!(dept.run_kind(true, false), RunKind::Small);
        let small = dept.run_plan("t1", RunKind::Small).unwrap();
        assert_eq!(small.team.members.len(), 1);
        let agent = &small.team.members[0];
        assert_eq!(agent.model.as_deref(), Some("claude-haiku-5-5"));
        assert_eq!(agent.effort.as_deref(), Some("high"));
        assert!(agent.lead);
        assert_eq!(small.spec.step.len(), 1);
        assert_eq!(small.spec.step[0].agent, "pr:fast:t1");

        assert_eq!(dept.run_kind(false, true), RunKind::Fallback);
        let fallback = dept.run_plan("t1", RunKind::Fallback).unwrap();
        let doer = fallback.team.get(&AgentId::from("pr:writer:t1")).unwrap();
        assert_eq!(doer.kind, AgentKind::Claude);
        assert_eq!(doer.model.as_deref(), Some("claude-sonnet-5-5"));
        assert_eq!(doer.sandbox, None);
        assert!(doer.lead);
        assert_eq!(fallback.team.members.len(), 2);

        // Without spares everything is a normal run.
        let plain = Department::parse(SAMPLE, Path::new("pr")).unwrap();
        assert_eq!(plain.run_kind(true, true), RunKind::Full);
        assert!(!plain.can_fall_back());
        assert!(dept.can_fall_back());
    }

    #[test]
    fn warns_about_same_model_review() {
        let same = SAMPLE.replace("kind = \"codex\"", "kind = \"claude\"");
        let dept = Department::parse(&same, Path::new("pr")).unwrap();
        assert!(dept.warnings()[0].contains("same model"));
    }
}
