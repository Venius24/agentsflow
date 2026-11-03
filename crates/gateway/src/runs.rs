//! Department runs: a queue of department tasks. Each run starts fresh agent
//! sessions in `departments/<id>/runs/<task>/`, executes the department
//! workflow and is stopped by loops, time, budget or the owner.

use crate::{
    brief,
    department::{Department, RunKind},
    home::Home,
    hub::{Hub, TaskError},
    ledger::{self, Ledger},
    runtime::{Agents, SpawnEnv, spawn_member},
    workflow::{self, StepRecord},
};
use agentsflow_events::{
    AgentId, AgentKind, Event, Risk, RuleStatus, RunRef, TaskSnapshot, WorkInfo, WorkState,
};
use serde::Serialize;
use std::{
    cell::Cell,
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{broadcast::error::RecvError, mpsc, watch};
use uuid::Uuid;

/// Sender id of gateway notices in the owner's mailbox.
pub const NOTICE_FROM: &str = "agentsflow";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    Owner,
    Timeout,
    Budget,
    Month,
    Shutdown,
}

#[cfg(test)]
pub type Spawner = Rc<
    dyn Fn(
        &Hub,
        &crate::team::Member,
        &std::path::Path,
    ) -> (crate::hub::Actor, tokio::task::JoinHandle<()>),
>;

#[derive(Clone, Default)]
pub struct RunEnv {
    pub codex_bin: Option<PathBuf>,
    /// Replaces real CLI agents in tests.
    #[cfg(test)]
    pub spawner: Option<Spawner>,
}

impl RunEnv {
    pub fn new(codex_bin: Option<PathBuf>) -> Self {
        Self {
            codex_bin,
            #[cfg(test)]
            spawner: None,
        }
    }

    pub(crate) fn spawn(
        &self,
        hub: &Hub,
        member: &crate::team::Member,
        dir: &std::path::Path,
    ) -> (crate::hub::Actor, tokio::task::JoinHandle<()>) {
        #[cfg(test)]
        if let Some(spawner) = &self.spawner {
            return spawner(hub, member, dir);
        }
        spawn_member(
            hub,
            member,
            SpawnEnv {
                cwd: dir.to_path_buf(),
                model: member.model.clone(),
                effort: member.effort.clone(),
                codex_bin: self.codex_bin.clone(),
                mail: None,
            },
        )
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct RunInfo {
    pub task: String,
    pub run: String,
    pub department: String,
    pub started_ms: u64,
}

/// A new department task.
#[derive(Clone, Debug, Default)]
pub struct NewWork {
    pub title: String,
    pub text: String,
    pub department: String,
    pub risk: Option<Risk>,
    pub depends_on: Vec<String>,
    pub parent: Option<String>,
    pub budget_usd: Option<f64>,
    pub source: String,
    pub questions: Vec<agentsflow_events::Question>,
    pub proposal: Option<agentsflow_events::DepartmentProposal>,
    pub small: bool,
}

#[derive(Clone)]
pub struct Runs {
    hub: Hub,
    ledger: Ledger,
    shared: Arc<Mutex<Shared>>,
    tx: mpsc::UnboundedSender<Cmd>,
}
struct Shared {
    home: Arc<Home>,
    running: BTreeMap<String, RunInfo>,
    orchestrator: Option<crate::orchestrator::Orchestrator>,
    schedules: crate::schedule::State,
}

enum Cmd {
    Kick,
    Stop(String, StopReason),
    StopAll(StopReason),
    Finished(Finished),
}
struct Finished {
    task: String,
    result: Result<(), String>,
    history: Vec<StepRecord>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn short(text: &str, limit: usize) -> String {
    let text = text.trim();
    let mut out: String = text.chars().take(limit).collect();
    if text.chars().count() > limit {
        out.push('…');
    }
    out
}

impl Runs {
    /// Starts the run manager on the current `LocalSet`. Tasks left running
    /// by a previous server are marked stuck first.
    pub fn start(hub: Hub, home: Home, ledger: Ledger, env: RunEnv) -> Self {
        for task in hub.tasks(true) {
            let running = task
                .work
                .as_ref()
                .is_some_and(|work| work.state == WorkState::Running);
            if running {
                let _ = hub.update_work(AgentId::from(NOTICE_FROM), &task.id, |_, work| {
                    work.state = WorkState::Stuck;
                    work.stuck_reason = Some("сервер перезапущен".into());
                    if let Some(run) = work.runs.last_mut().filter(|run| run.finished_ms.is_none())
                    {
                        run.finished_ms = Some(now_ms());
                        run.outcome = "interrupted".into();
                    }
                    Ok(())
                });
            }
        }
        let _ = std::fs::create_dir_all(home.state_dir());
        let schedules = crate::schedule::State::open(home.state_dir().join("schedules.json"));
        let (tx, rx) = mpsc::unbounded_channel();
        let runs = Self {
            hub: hub.clone(),
            ledger: ledger.clone(),
            shared: Arc::new(Mutex::new(Shared {
                home: Arc::new(home),
                running: BTreeMap::new(),
                orchestrator: None,
                schedules,
            })),
            tx: tx.clone(),
        };
        let engine = Engine {
            hub,
            ledger,
            env,
            shared: runs.shared.clone(),
            tx,
            agents: HashMap::new(),
            active: HashMap::new(),
        };
        tokio::task::spawn_local(engine.run(rx));
        let ticker = runs.clone();
        tokio::task::spawn_local(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            loop {
                tick.tick().await;
                ticker.fire_schedules(now_ms());
            }
        });
        runs
    }

    pub fn set_orchestrator(&self, orchestrator: crate::orchestrator::Orchestrator) {
        self.shared.lock().unwrap().orchestrator = Some(orchestrator);
    }

    /// A free-form owner message for the orchestrator.
    pub fn message(&self, text: &str) -> Result<(), String> {
        let orchestrator = self.shared.lock().unwrap().orchestrator.clone();
        orchestrator
            .ok_or_else(|| "оркестратор не запущен".to_owned())?
            .message(text)
    }

    pub fn hub(&self) -> &Hub {
        &self.hub
    }

    pub(crate) fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    pub fn home(&self) -> Arc<Home> {
        self.shared.lock().unwrap().home.clone()
    }

    /// Re-reads the home from disk (after a department was added or changed).
    pub fn reload(&self) -> anyhow::Result<()> {
        let root = self.home().root.clone();
        let home = Home::load(&root)?;
        self.shared.lock().unwrap().home = Arc::new(home);
        self.kick();
        Ok(())
    }

    pub fn kick(&self) {
        let _ = self.tx.send(Cmd::Kick);
    }

    /// Asks the run of `task` to stop; false when it is not running.
    pub fn stop(&self, task: &str) -> bool {
        if !self.shared.lock().unwrap().running.contains_key(task) {
            return false;
        }
        self.tx
            .send(Cmd::Stop(task.to_owned(), StopReason::Owner))
            .is_ok()
    }

    /// Stops every run and waits (up to 20 s) for their agents to exit.
    pub async fn shutdown(&self) {
        let _ = self.tx.send(Cmd::StopAll(StopReason::Shutdown));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while !self.shared.lock().unwrap().running.is_empty()
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub fn running(&self) -> Vec<RunInfo> {
        self.shared
            .lock()
            .unwrap()
            .running
            .values()
            .cloned()
            .collect()
    }

    pub fn month_spend(&self) -> HashMap<String, f64> {
        self.ledger.month(&ledger::month_of(now_ms()))
    }

    pub fn month_tokens(&self) -> HashMap<String, ledger::Tokens> {
        self.ledger.month_tokens(&ledger::month_of(now_ms()))
    }

    /// Puts a department task in the queue.
    pub fn create(&self, by: AgentId, new: NewWork) -> Result<TaskSnapshot, String> {
        let home = self.home();
        let (department, default_risk) = match &new.proposal {
            // Waits for the owner to create the proposed department.
            Some(proposal) => {
                if home.departments.contains_key(&proposal.id) {
                    return Err(format!("отдел {} уже есть", proposal.id));
                }
                if !crate::department::valid_id(&proposal.id) {
                    return Err(format!("недопустимый id отдела {:?}", proposal.id));
                }
                (proposal.id.clone(), proposal.default_risk)
            }
            None => {
                let dept = home
                    .departments
                    .get(&new.department)
                    .ok_or_else(|| format!("нет отдела {}", new.department))?;
                (dept.id.clone(), dept.default_risk)
            }
        };
        for dep in &new.depends_on {
            if self.hub.task(dep).and_then(|t| t.work).is_none() {
                return Err(format!("нет задачи отдела {dep}"));
            }
        }
        if let Some(parent) = &new.parent {
            if self.hub.task(parent).is_none() {
                return Err(format!("нет задачи {parent}"));
            }
        }
        if new
            .budget_usd
            .is_some_and(|budget| !(budget > 0.0 && budget <= 1000.0))
        {
            return Err("бюджет задачи должен быть больше 0 и не больше 1000".into());
        }
        let risk = new.risk.unwrap_or(default_risk);
        let mut work = WorkInfo {
            department,
            risk,
            // What goes outside always gets the full department.
            small: new.small && risk != Risk::External,
            parent: new.parent,
            depends_on: new.depends_on,
            budget_usd: new.budget_usd,
            source: new.source,
            questions: new.questions,
            proposal: new.proposal,
            ..Default::default()
        };
        work.state = if ready(&work) {
            WorkState::Queued
        } else {
            WorkState::Waiting
        };
        let task = self
            .hub
            .create_work(by.clone(), &new.title, new.text, work)
            .map_err(|error| error.to_string())?;
        if let Some(work) = &task.work {
            crate::diag::diag(
                "INFO",
                &format!(
                    "задача {} [{}] создана ({}), {} «{}»",
                    task.id,
                    work.department,
                    by.as_str(),
                    format!("{:?}", work.state).to_lowercase(),
                    task.title
                ),
            );
        }
        self.kick();
        Ok(task)
    }

    /// The owner's answer to every open question of the task; a task with
    /// nothing else to wait for goes to the queue.
    pub fn answer(&self, task: &str, text: &str) -> Result<TaskSnapshot, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("пустой ответ".into());
        }
        let updated = self
            .hub
            .update_work(AgentId::orchestrator(), task, |_, work| {
                let open: Vec<_> = work
                    .questions
                    .iter_mut()
                    .filter(|q| q.answer.is_none())
                    .collect();
                if open.is_empty() {
                    return Err(TaskError::Invalid("no open questions"));
                }
                for question in open {
                    question.answer = Some(text.to_owned());
                }
                if work.state == WorkState::Waiting && ready(work) {
                    work.state = WorkState::Queued;
                }
                Ok(())
            })
            .map_err(|error| match error {
                TaskError::Invalid("no open questions") => "у задачи нет открытых вопросов".into(),
                other => other.to_string(),
            })?;
        self.kick();
        Ok(updated)
    }

    /// The owner's decision on a proposed department: create it, or hand the
    /// waiting tasks to `general`. Returns the department the tasks went to.
    pub fn resolve_proposal(&self, task: &str, create: bool) -> Result<String, String> {
        let proposal = self
            .hub
            .task(task)
            .and_then(|t| t.work)
            .and_then(|w| w.proposal)
            .ok_or("у задачи нет предложенного отдела")?;
        let target = if create {
            let root = self.home().root.clone();
            crate::home::create_department(&root, &proposal).map_err(|e| format!("{e:#}"))?;
            self.reload().map_err(|e| format!("{e:#}"))?;
            proposal.id.clone()
        } else {
            "general".to_owned()
        };
        for waiting in self.hub.tasks(false) {
            let matches = waiting
                .work
                .as_ref()
                .and_then(|w| w.proposal.as_ref())
                .is_some_and(|p| p.id == proposal.id);
            if !matches {
                continue;
            }
            let _ = self
                .hub
                .update_work(AgentId::orchestrator(), &waiting.id, |_, work| {
                    work.proposal = None;
                    work.department = target.clone();
                    if work.state == WorkState::Waiting && ready(work) {
                        work.state = WorkState::Queued;
                    }
                    Ok(())
                });
        }
        self.kick();
        Ok(target)
    }

    /// Acceptance of an internal result by the orchestrator.
    pub fn accept(&self, by: AgentId, task: &str) -> Result<TaskSnapshot, String> {
        self.hub
            .update_work(by, task, |_, work| {
                if work.state != WorkState::Review {
                    return Err(TaskError::Invalid("task is not waiting for acceptance"));
                }
                if work.risk == Risk::External || work.owner_review {
                    return Err(TaskError::Invalid("only the owner accepts this task"));
                }
                work.state = WorkState::Done;
                Ok(())
            })
            .map_err(|error| error.to_string())
    }

    /// Sends a result back once; the second time it goes to the owner.
    pub fn return_task(
        &self,
        by: AgentId,
        task: &str,
        reason: &str,
    ) -> Result<TaskSnapshot, String> {
        let updated = self
            .hub
            .update_work(by, task, |_, work| {
                if work.state != WorkState::Review {
                    return Err(TaskError::Invalid("task is not waiting for acceptance"));
                }
                work.review_notes.push(reason.trim().to_owned());
                if work.returns >= 1 {
                    work.owner_review = true;
                } else {
                    work.returns += 1;
                    work.state = WorkState::Queued;
                    // Not good enough alone: the full department takes it.
                    work.small = false;
                }
                Ok(())
            })
            .map_err(|error| error.to_string())?;
        self.kick();
        Ok(updated)
    }

    /// Hands a result to the owner's acceptance.
    pub fn to_owner(&self, by: AgentId, task: &str) -> Result<TaskSnapshot, String> {
        self.hub
            .update_work(by, task, |_, work| {
                if work.state != WorkState::Review {
                    return Err(TaskError::Invalid("task is not waiting for acceptance"));
                }
                work.owner_review = true;
                Ok(())
            })
            .map_err(|error| error.to_string())
    }

    /// The owner sends a result or a stuck task back with a note for the
    /// next run. The owner is not limited in returns.
    pub fn owner_return(&self, task: &str, note: &str) -> Result<TaskSnapshot, String> {
        let note = note.trim();
        if note.is_empty() {
            return Err("пустое замечание".into());
        }
        let updated = self
            .hub
            .update_work(AgentId::orchestrator(), task, |_, work| {
                if !matches!(
                    work.state,
                    WorkState::Review | WorkState::Stuck | WorkState::Done
                ) {
                    return Err(TaskError::Invalid("task has no result to return"));
                }
                work.review_notes.push(format!("хозяин: {note}"));
                work.state = WorkState::Queued;
                work.stuck_reason = None;
                Ok(())
            })
            .map_err(|error| error.to_string())?;
        self.kick();
        Ok(updated)
    }

    /// Questions from the orchestrator about an existing task.
    pub fn ask(
        &self,
        by: AgentId,
        task: &str,
        questions: &[String],
    ) -> Result<TaskSnapshot, String> {
        if questions.iter().all(|q| q.trim().is_empty()) {
            return Err("пустые вопросы".into());
        }
        self.hub
            .update_work(by.clone(), task, |_, work| {
                if work.state == WorkState::Running {
                    return Err(TaskError::Invalid("task is running"));
                }
                for text in questions.iter().filter(|q| !q.trim().is_empty()) {
                    work.questions.push(agentsflow_events::Question {
                        text: text.trim().to_owned(),
                        answer: None,
                        from: by.to_string(),
                    });
                }
                if matches!(
                    work.state,
                    WorkState::Queued | WorkState::Review | WorkState::Stuck
                ) {
                    work.state = WorkState::Waiting;
                }
                Ok(())
            })
            .map_err(|error| error.to_string())
    }
}

impl Runs {
    /// Raises a department's month limit by half, rounded up to a dollar.
    pub fn raise_month_limit(&self, department: &str) -> Result<(f64, f64), String> {
        let home = self.home();
        let dept = home
            .departments
            .get(department)
            .ok_or_else(|| format!("нет отдела {department}"))?;
        let old = dept
            .limits
            .month_usd
            .ok_or("у отдела нет месячного лимита")?;
        let new = (old * 1.5).ceil();
        crate::home::set_month_limit(&home.root, department, new).map_err(|e| format!("{e:#}"))?;
        self.reload().map_err(|e| format!("{e:#}"))?;
        self.kick();
        Ok((old, new))
    }

    pub fn rule_proposals(&self) -> Vec<crate::rules::Proposal> {
        crate::rules::proposals(&self.home())
    }

    pub fn recent_rules(&self, limit: usize) -> Vec<crate::rules::RuleCommit> {
        crate::rules::recent(&self.home(), limit)
    }

    /// A rule proposed by the orchestrator for a department or itself.
    pub fn propose_rule(&self, target: &str, text: &str) -> Result<crate::rules::Proposal, String> {
        let proposal =
            crate::rules::propose(&self.home(), target, text).map_err(|e| format!("{e:#}"))?;
        publish_rule(
            &self.hub,
            proposal.target.clone(),
            proposal.id.clone(),
            proposal.text.clone(),
            RuleStatus::Proposed,
        );
        Ok(proposal)
    }

    pub fn approve_rule(&self, target: &str, id: &str) -> Result<crate::rules::RuleCommit, String> {
        let rule = crate::rules::approve(&self.home(), target, id).map_err(|e| format!("{e:#}"))?;
        publish_rule(
            &self.hub,
            rule.target.clone(),
            rule.commit.clone(),
            rule.subject.clone(),
            RuleStatus::Added,
        );
        Ok(rule)
    }

    pub fn reject_rule(&self, target: &str, id: &str) -> Result<(), String> {
        crate::rules::reject(&self.home(), target, id).map_err(|e| format!("{e:#}"))?;
        publish_rule(
            &self.hub,
            target.to_owned(),
            id.to_owned(),
            String::new(),
            RuleStatus::Rejected,
        );
        Ok(())
    }

    pub fn revert_rule(&self, commit: &str) -> Result<crate::rules::RuleCommit, String> {
        let rule = crate::rules::revert(&self.home(), commit).map_err(|e| format!("{e:#}"))?;
        publish_rule(
            &self.hub,
            rule.target.clone(),
            rule.commit.clone(),
            rule.subject.clone(),
            RuleStatus::Reverted,
        );
        Ok(rule)
    }
}

impl Runs {
    fn find_schedule(&self, key: &str) -> Option<(String, crate::department::Schedule)> {
        let home = self.home();
        home.departments.values().find_map(|dept| {
            dept.schedules
                .iter()
                .find(|s| crate::schedule::key(&dept.id, &s.cron, &s.task) == key)
                .map(|s| (dept.id.clone(), s.clone()))
        })
    }

    pub fn schedules(&self) -> Vec<crate::schedule::Info> {
        let home = self.home();
        let offset = home.config.offset_minutes();
        let shared = self.shared.lock().unwrap();
        let now = now_ms();
        let mut out = Vec::new();
        for dept in home.departments.values() {
            for schedule in &dept.schedules {
                let key = crate::schedule::key(&dept.id, &schedule.cron, &schedule.task);
                let entry = shared
                    .schedules
                    .entries
                    .get(&key)
                    .cloned()
                    .unwrap_or_default();
                let next_ms = crate::schedule::Cron::parse(&schedule.cron)
                    .ok()
                    .and_then(|cron| {
                        cron.next_after(entry.last_ms.unwrap_or(now).max(now), offset)
                    });
                out.push(crate::schedule::Info {
                    key,
                    department: dept.id.clone(),
                    title: schedule_title(schedule),
                    cron: schedule.cron.clone(),
                    paused: entry.paused,
                    last_ms: entry.last_ms,
                    next_ms: if entry.paused { None } else { next_ms },
                });
            }
        }
        out
    }

    pub fn pause_schedule(&self, key: &str, paused: bool) -> Result<(), String> {
        if self.find_schedule(key).is_none() {
            return Err("нет такого расписания".into());
        }
        let mut shared = self.shared.lock().unwrap();
        let entry = shared.schedules.entries.entry(key.to_owned()).or_default();
        entry.paused = paused;
        if !paused {
            // Resuming does not catch up on what was skipped.
            entry.last_ms = Some(now_ms());
        }
        shared.schedules.save();
        Ok(())
    }

    /// Puts the scheduled task into the queue now, outside its schedule.
    pub fn run_schedule(&self, key: &str) -> Result<TaskSnapshot, String> {
        let (department, schedule) = self.find_schedule(key).ok_or("нет такого расписания")?;
        self.create_scheduled(&department, &schedule)
    }

    fn create_scheduled(
        &self,
        department: &str,
        schedule: &crate::department::Schedule,
    ) -> Result<TaskSnapshot, String> {
        self.create(
            AgentId::from(NOTICE_FROM),
            NewWork {
                title: schedule_title(schedule),
                text: schedule.task.clone(),
                department: department.to_owned(),
                source: "schedule".into(),
                ..Default::default()
            },
        )
    }

    /// Creates the tasks whose time has come. A schedule seen for the first
    /// time starts counting from now; one missed while the server was down
    /// fires once.
    pub(crate) fn fire_schedules(&self, now: u64) -> Vec<String> {
        let home = self.home();
        let offset = home.config.offset_minutes();
        let mut due = Vec::new();
        {
            let mut shared = self.shared.lock().unwrap();
            let mut changed = false;
            for dept in home.departments.values() {
                for schedule in &dept.schedules {
                    let key = crate::schedule::key(&dept.id, &schedule.cron, &schedule.task);
                    let entry = shared.schedules.entries.entry(key.clone()).or_default();
                    let Some(last) = entry.last_ms else {
                        entry.last_ms = Some(now);
                        changed = true;
                        continue;
                    };
                    if entry.paused {
                        continue;
                    }
                    let next = crate::schedule::Cron::parse(&schedule.cron)
                        .ok()
                        .and_then(|cron| cron.next_after(last, offset));
                    if next.is_some_and(|next| next <= now) {
                        entry.last_ms = Some(now);
                        changed = true;
                        due.push((dept.id.clone(), schedule.clone()));
                    }
                }
            }
            if changed {
                shared.schedules.save();
            }
        }
        let mut created = Vec::new();
        for (department, schedule) in due {
            match self.create_scheduled(&department, &schedule) {
                Ok(task) => created.push(task.id),
                Err(error) => crate::server_diag!("schedule {department}: {error}"),
            }
        }
        created
    }
}

fn schedule_title(schedule: &crate::department::Schedule) -> String {
    schedule
        .title
        .clone()
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| crate::server::title_from(&schedule.task))
}

/// How many levels of orders may stack: the owner's task, its order and
/// the order's order.
pub const MAX_REQUEST_DEPTH: usize = 2;
/// An order for another department an agent leaves in the run folder.
pub const REQUEST_FILE: &str = "request.md";

fn take_request(run_dir: &std::path::Path) -> Option<String> {
    let path = run_dir.join(REQUEST_FILE);
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    let text = text.trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// Questions for the owner an agent leaves in the run folder, one per line.
pub const QUESTIONS_FILE: &str = "questions.md";

fn take_questions(run_dir: &std::path::Path) -> Vec<String> {
    let path = run_dir.join(QUESTIONS_FILE);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let _ = std::fs::remove_file(&path);
    text.lines()
        .map(|line| {
            let line = line.trim().trim_start_matches(['-', '*', '•']).trim_start();
            let digits = line.len() - line.trim_start_matches(|c: char| c.is_ascii_digit()).len();
            match line[digits..].strip_prefix(['.', ')']) {
                Some(rest) if digits > 0 => rest.trim().to_owned(),
                _ => line.to_owned(),
            }
        })
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect()
}

/// Results of the tasks this one depends on, copied into `inputs/<task>/` of
/// the run folder: the run may not read other departments' folders.
fn copy_inputs(inputs: &[brief::Input], run_dir: &Path) -> anyhow::Result<Vec<brief::Input>> {
    let mut local = Vec::new();
    for input in inputs {
        let to = run_dir.join("inputs").join(&input.task);
        let _ = std::fs::remove_dir_all(&to);
        std::fs::create_dir_all(&to)?;
        let result = to.join("result.md");
        std::fs::copy(&input.result, &result)?;
        if let Some(from) = input.result.parent() {
            copy_dir(&from.join("artifacts"), &to.join("artifacts"))?;
        }
        local.push(brief::Input {
            task: input.task.clone(),
            title: input.title.clone(),
            result,
        });
    }
    Ok(local)
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    let Ok(entries) = std::fs::read_dir(from) else {
        return Ok(());
    };
    std::fs::create_dir_all(to)?;
    for entry in entries {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// The tasks that ordered this one, nearest first.
pub(crate) fn ancestors(hub: &Hub, task: &str) -> Vec<TaskSnapshot> {
    let mut out: Vec<TaskSnapshot> = Vec::new();
    let mut next = hub.task(task).and_then(|t| t.work).and_then(|w| w.parent);
    while let Some(id) = next {
        if out.iter().any(|t| t.id == id) || out.len() > MAX_REQUEST_DEPTH + 1 {
            break;
        }
        let Some(parent) = hub.task(&id) else { break };
        next = parent.work.as_ref().and_then(|w| w.parent.clone());
        out.push(parent);
    }
    out
}

/// Departments a run may order work from, for the brief; `None` when the
/// chain is already as deep as allowed.
fn request_catalog(home: &Home, hub: &Hub, task: &TaskSnapshot) -> Option<String> {
    let chain = ancestors(hub, &task.id);
    if chain.len() >= MAX_REQUEST_DEPTH {
        return None;
    }
    let mut busy: Vec<String> = chain
        .iter()
        .filter_map(|t| t.work.as_ref().map(|w| w.department.clone()))
        .collect();
    if let Some(work) = &task.work {
        busy.push(work.department.clone());
    }
    let lines: Vec<String> = home
        .departments
        .values()
        .filter(|dept| !busy.contains(&dept.id))
        .map(|dept| format!("- {} — {}: {}", dept.id, dept.title, dept.description))
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// Tells the board and the bot that a rule changed.
pub(crate) fn publish_rule(
    hub: &Hub,
    target: String,
    id: String,
    text: String,
    status: RuleStatus,
) {
    hub.publish(
        AgentId::from(NOTICE_FROM),
        "rules",
        Event::Rule {
            target,
            id,
            text,
            status,
        },
    );
}

/// A waiting task may run once every question is answered and its
/// department exists.
fn ready(work: &WorkInfo) -> bool {
    work.proposal.is_none() && work.questions.iter().all(|q| q.answer.is_some())
}

struct AgentMeta {
    task: String,
    run: String,
    department: String,
    kind: AgentKind,
    model: Option<String>,
}

struct Active {
    stop: Rc<watch::Sender<bool>>,
    reason: Rc<Cell<Option<StopReason>>>,
    department: String,
    run_dir: PathBuf,
    doers: Vec<AgentId>,
    can_fall_back: bool,
}

struct Engine {
    hub: Hub,
    ledger: Ledger,
    env: RunEnv,
    shared: Arc<Mutex<Shared>>,
    tx: mpsc::UnboundedSender<Cmd>,
    agents: HashMap<AgentId, AgentMeta>,
    active: HashMap<String, Active>,
}

impl Engine {
    fn by() -> AgentId {
        AgentId::from(NOTICE_FROM)
    }

    fn home(&self) -> Arc<Home> {
        self.shared.lock().unwrap().home.clone()
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Cmd>) {
        let (_, mut events) = self.hub.subscribe();
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        self.schedule();
        loop {
            tokio::select! {
                command = rx.recv() => match command {
                    Some(Cmd::Kick) => {}
                    Some(Cmd::Stop(task, reason)) => self.stop(&task, reason),
                    Some(Cmd::StopAll(reason)) => {
                        let tasks: Vec<_> = self.active.keys().cloned().collect();
                        for task in tasks {
                            self.stop(&task, reason);
                        }
                    }
                    Some(Cmd::Finished(finished)) => self.finish(finished),
                    None => break,
                },
                event = events.recv() => match event {
                    Ok(envelope) => match envelope.event {
                        Event::Usage { input_tokens, output_tokens, cost_usd } => {
                            self.account(&envelope.agent, input_tokens, output_tokens, cost_usd);
                            continue;
                        }
                        Event::Task { .. } => {}
                        _ => continue,
                    },
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => break,
                },
                _ = tick.tick() => {}
            }
            self.schedule();
        }
    }

    fn stop(&mut self, task: &str, reason: StopReason) {
        if let Some(active) = self.active.get(task) {
            if active.reason.get().is_none() {
                active.reason.set(Some(reason));
            }
            let _ = active.stop.send(true);
        }
    }

    fn notify(&self, subject: &str, body: String) {
        if let Err(error) = self.hub.send_mail(
            Self::by(),
            AgentId::orchestrator(),
            subject.into(),
            body,
            None,
        ) {
            crate::server_diag!("notice failed: {error}");
        }
    }

    fn account(&mut self, agent: &AgentId, input: u64, output: u64, reported: Option<f64>) {
        let Some(meta) = self.agents.get(agent) else {
            return;
        };
        let home = self.home();
        let (usd, source) = ledger::cost(
            meta.kind,
            meta.model.as_deref(),
            input,
            output,
            reported,
            &home.config.prices,
        );
        let ts_ms = now_ms();
        let month = ledger::month_of(ts_ms);
        let (before, after) = self.ledger.record(ledger::Entry {
            ts_ms,
            month: month.clone(),
            department: meta.department.clone(),
            task: meta.task.clone(),
            run: meta.run.clone(),
            agent: agent.to_string(),
            model: meta.model.clone(),
            input_tokens: input,
            output_tokens: output,
            usd,
            source: source.into(),
        });
        let (task, run, department) =
            (meta.task.clone(), meta.run.clone(), meta.department.clone());
        let updated = self.hub.update_work(Self::by(), &task, |_, work| {
            work.cost_usd += usd;
            work.input_tokens += input;
            work.output_tokens += output;
            if let Some(entry) = work.runs.iter_mut().find(|r| r.id == run) {
                entry.cost_usd += usd;
            }
            Ok(())
        });
        for ancestor in ancestors(&self.hub, &task) {
            let _ = self.hub.update_work(Self::by(), &ancestor.id, |_, work| {
                work.cost_usd += usd;
                work.input_tokens += input;
                work.output_tokens += output;
                Ok(())
            });
        }
        let Some(dept) = home.departments.get(&department) else {
            return;
        };
        if let Ok(task_snapshot) = updated {
            let work = task_snapshot.work.as_ref().expect("department task");
            let budget = work.budget_usd.unwrap_or(dept.limits.task_usd);
            if work.cost_usd >= budget {
                self.stop(&task, StopReason::Budget);
            }
        }
        if let Some(limit) = dept.limits.month_usd {
            let warn = limit * dept.limits.warn_ratio;
            if before < limit && after >= limit {
                self.notify(
                    "лимит отдела исчерпан",
                    format!(
                        "Отдел «{}» потратил ${after:.2} из ${limit:.2} за {month}. Текущие задачи остановлены, новые ждут. Поднимите limits.month_usd в departments/{}/department.toml, если нужно продолжить.",
                        dept.title, dept.id
                    ),
                );
            } else if warn < limit && before < warn && after >= warn {
                self.notify(
                    "лимит отдела кончается",
                    format!(
                        "Отдел «{}» потратил ${after:.2} из ${limit:.2} за {month}.",
                        dept.title
                    ),
                );
            }
            if after >= limit {
                let tasks: Vec<_> = self
                    .active
                    .iter()
                    .filter(|(_, active)| active.department == department)
                    .map(|(task, _)| task.clone())
                    .collect();
                for task in tasks {
                    self.stop(&task, StopReason::Month);
                }
            }
        }
    }

    fn mark_stuck(&self, task: &str, reason: String) {
        let _ = self.hub.update_work(Self::by(), task, |_, work| {
            work.state = WorkState::Stuck;
            work.stuck_reason = Some(reason);
            Ok(())
        });
    }

    fn schedule(&mut self) {
        let home = self.home();
        let tasks = self.hub.tasks(false);
        let mut queued: Vec<&TaskSnapshot> = tasks
            .iter()
            .filter(|t| {
                t.work
                    .as_ref()
                    .is_some_and(|w| w.state == WorkState::Queued)
            })
            .collect();
        queued.sort_by_key(|t| t.created_ms);
        let month = ledger::month_of(now_ms());
        for task in queued {
            if self.active.len() >= home.config.max_parallel_runs {
                break;
            }
            let work = task.work.as_ref().expect("filtered");
            let Some(dept) = home.departments.get(&work.department) else {
                self.mark_stuck(&task.id, format!("нет отдела {}", work.department));
                continue;
            };
            let busy = self
                .active
                .values()
                .filter(|active| active.department == dept.id)
                .count();
            if busy >= dept.max_parallel {
                continue;
            }
            if dept
                .limits
                .month_usd
                .is_some_and(|limit| self.ledger.month_total(&month, &dept.id) >= limit)
            {
                continue;
            }
            let mut ready = true;
            let mut inputs = Vec::new();
            for dep in &work.depends_on {
                let state = tasks
                    .iter()
                    .find(|t| &t.id == dep)
                    .and_then(|t| t.work.as_ref().map(|w| (t, w)));
                match state {
                    Some((dep_task, dep_work)) if dep_work.state == WorkState::Done => {
                        if let Some(path) = &dep_work.result_path {
                            inputs.push(brief::Input {
                                task: dep_task.id.clone(),
                                title: dep_task.title.clone(),
                                result: home.root.join(path),
                            });
                        }
                    }
                    Some((_, dep_work)) if dep_work.state == WorkState::Cancelled => {
                        self.mark_stuck(&task.id, format!("зависимость {dep} отменена"));
                        ready = false;
                        break;
                    }
                    Some(_) => {
                        ready = false;
                        break;
                    }
                    None => {
                        self.mark_stuck(&task.id, format!("зависимость {dep} не найдена"));
                        ready = false;
                        break;
                    }
                }
            }
            if ready {
                if let Err(error) = self.launch(&home, dept, task, &inputs) {
                    self.mark_stuck(&task.id, format!("запуск не удался: {error:#}"));
                }
            }
        }
    }

    fn launch(
        &mut self,
        home: &Home,
        dept: &Department,
        task: &TaskSnapshot,
        inputs: &[brief::Input],
    ) -> anyhow::Result<()> {
        let run_id = Uuid::new_v4().simple().to_string()[..8].to_owned();
        let run_dir = dept.run_dir(&task.id);
        std::fs::create_dir_all(run_dir.join("artifacts"))?;
        let inputs = copy_inputs(inputs, &run_dir)?;
        let work = task.work.clone().unwrap_or_default();
        let kind = dept.run_kind(work.small, work.fallback);
        crate::diag::diag(
            "INFO",
            &format!("запуск {run_id}: задача {} [{}] {kind:?}", task.id, dept.id),
        );
        let requests = request_catalog(home, &self.hub, task);
        let text = brief::build(
            dept,
            task,
            &inputs,
            home.config.memory_digest_kb,
            &run_dir,
            requests.as_deref(),
        );
        std::fs::write(run_dir.join("brief.md"), &text)?;
        let mut plan = dept.run_plan(&task.id, kind)?;
        plan.spec.bind("brief", text);
        for file in [REQUEST_FILE, QUESTIONS_FILE] {
            let _ = std::fs::remove_file(run_dir.join(file));
            plan.spec.halt_on(run_dir.join(file));
        }
        let started_ms = now_ms();
        self.hub
            .update_work(Self::by(), &task.id, |_, work| {
                work.state = WorkState::Running;
                work.stuck_reason = None;
                work.runs.push(RunRef {
                    id: run_id.clone(),
                    started_ms,
                    finished_ms: None,
                    outcome: String::new(),
                    cost_usd: 0.0,
                });
                Ok(())
            })
            .map_err(|error: TaskError| anyhow::anyhow!("{error}"))?;
        let mut doers = Vec::new();
        for member in &plan.team.members {
            self.agents.insert(
                member.id.clone(),
                AgentMeta {
                    task: task.id.clone(),
                    run: run_id.clone(),
                    department: dept.id.clone(),
                    kind: member.kind,
                    model: member.model.clone(),
                },
            );
            if member.roles.iter().any(|role| role == "doer") {
                doers.push(member.id.clone());
            }
        }
        let (stop_tx, stop_rx) = watch::channel(false);
        let stop = Rc::new(stop_tx);
        let reason = Rc::new(Cell::new(None));
        self.active.insert(
            task.id.clone(),
            Active {
                stop: stop.clone(),
                reason: reason.clone(),
                department: dept.id.clone(),
                run_dir: run_dir.clone(),
                doers,
                can_fall_back: kind == RunKind::Full && dept.can_fall_back(),
            },
        );
        self.shared.lock().unwrap().running.insert(
            task.id.clone(),
            RunInfo {
                task: task.id.clone(),
                run: run_id.clone(),
                department: dept.id.clone(),
                started_ms,
            },
        );
        let hub = self.hub.clone();
        let tx = self.tx.clone();
        let env = self.env.clone();
        let timeout = Duration::from_secs(dept.limits.timeout_mins * 60);
        let task_id = task.id.clone();
        let task_text = format!("{}\n\n{}", task.title, task.description);
        tokio::task::spawn_local(async move {
            let mut agents = Agents::new();
            let mut handles = Vec::new();
            for member in &plan.team.members {
                let (actor, handle) = env.spawn(&hub, member, &run_dir);
                agents.insert(member.id.clone(), actor);
                handles.push(handle);
            }
            let timer = {
                let stop = stop.clone();
                let reason = reason.clone();
                tokio::task::spawn_local(async move {
                    tokio::time::sleep(timeout).await;
                    if reason.get().is_none() {
                        reason.set(Some(StopReason::Timeout));
                    }
                    let _ = stop.send(true);
                })
            };
            let (result, history) =
                workflow::execute_traced(&hub, &agents, &plan.spec, &task_text, &run_id, stop_rx)
                    .await;
            timer.abort();
            for actor in agents.values() {
                actor.shutdown().await;
            }
            for handle in handles {
                let _ = handle.await;
            }
            let _ = tx.send(Cmd::Finished(Finished {
                task: task_id,
                result,
                history,
            }));
        });
        Ok(())
    }

    /// The run ordered work from another department: the task waits until
    /// the orchestrator files the order and the order is done.
    fn request(&mut self, task: &str, text: String, now: u64) {
        let depth = ancestors(&self.hub, task).len();
        let orchestrator = self.shared.lock().unwrap().orchestrator.clone();
        let refusal = if depth >= MAX_REQUEST_DEPTH {
            Some(format!(
                "заказ другому отделу не принят: цепочка заказов уже {MAX_REQUEST_DEPTH} уровня"
            ))
        } else if orchestrator.is_none() {
            Some("заказ другому отделу не принят: оркестратор не запущен".to_owned())
        } else {
            None
        };
        let summary = format!("Ждёт заказа другому отделу:\n{}", short(&text, 600));
        let _ = self.hub.update_work(Self::by(), task, |_, work| {
            match &refusal {
                Some(reason) => {
                    work.state = WorkState::Stuck;
                    work.stuck_reason = Some(reason.clone());
                }
                None => {
                    work.state = WorkState::Waiting;
                    work.stuck_reason = None;
                }
            }
            work.result_summary = summary.clone();
            if let Some(run) = work.runs.last_mut() {
                run.finished_ms = Some(now);
                run.outcome = "request".into();
            }
            Ok(())
        });
        if refusal.is_none()
            && let Some(orchestrator) = orchestrator
            && let Err(error) = orchestrator.request(task, &text)
        {
            self.mark_stuck(task, error);
        }
    }

    fn finish(&mut self, finished: Finished) {
        let Some(active) = self.active.remove(&finished.task) else {
            return;
        };
        self.agents.retain(|_, meta| meta.task != finished.task);
        self.shared.lock().unwrap().running.remove(&finished.task);
        let home = self.home();
        let now = now_ms();
        let task = finished.task.clone();
        if let Some(dept) = home.departments.get(&active.department) {
            match crate::rules::harvest(&dept.id, &dept.dir, &task, &active.run_dir) {
                Ok((_, Some(proposal))) => publish_rule(
                    &self.hub,
                    proposal.target,
                    proposal.id,
                    proposal.text,
                    RuleStatus::Proposed,
                ),
                Ok(_) => {}
                Err(error) => crate::server_diag!("memory harvest failed: {error:#}"),
            }
        }
        if finished.result.is_ok()
            && let Some(request) = take_request(&active.run_dir)
        {
            self.request(&task, request, now);
            return;
        }
        let questions = take_questions(&active.run_dir);
        if finished.result.is_ok() && !questions.is_empty() {
            let from = format!("отдел {}", active.department);
            let _ = self.hub.update_work(Self::by(), &task, |_, work| {
                for text in &questions {
                    work.questions.push(agentsflow_events::Question {
                        text: text.clone(),
                        answer: None,
                        from: from.clone(),
                    });
                }
                work.state = WorkState::Waiting;
                work.stuck_reason = None;
                work.result_summary = "Отдел ждёт ответов на вопросы.".into();
                if let Some(run) = work.runs.last_mut() {
                    run.finished_ms = Some(now);
                    run.outcome = "questions".into();
                }
                Ok(())
            });
            return;
        }
        match finished.result {
            Ok(()) => {
                let path = active.run_dir.join("result.md");
                let written = std::fs::read_to_string(&path)
                    .ok()
                    .filter(|text| !text.trim().is_empty());
                let result = written.unwrap_or_else(|| {
                    let fallback = finished
                        .history
                        .iter()
                        .rev()
                        .find(|record| active.doers.contains(&record.agent))
                        .or(finished.history.last())
                        .map(|record| record.output.clone())
                        .unwrap_or_default();
                    if let Err(error) = std::fs::write(&path, &fallback) {
                        crate::server_diag!("result write failed: {error}");
                    }
                    fallback
                });
                let relative = path
                    .strip_prefix(&home.root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                let _ = self.hub.update_work(Self::by(), &task, |_, work| {
                    work.state = WorkState::Review;
                    work.stuck_reason = None;
                    work.result_summary = short(&result, 700);
                    work.result_path = Some(relative);
                    if let Some(run) = work.runs.last_mut() {
                        run.finished_ms = Some(now);
                        run.outcome = "finished".into();
                    }
                    Ok(())
                });
            }
            Err(error)
                if active.can_fall_back
                    && active.reason.get().is_none()
                    && error.contains(agentsflow_adapters::codex::USAGE_LIMIT) =>
            {
                let name = home
                    .departments
                    .get(&active.department)
                    .map(|dept| dept.fallback_name())
                    .unwrap_or_default();
                crate::diag::diag(
                    "WARN",
                    &format!("задача {task}: кончился лимит Codex, дальше {name}"),
                );
                let updated = self.hub.update_work(Self::by(), &task, |_, work| {
                    work.state = WorkState::Queued;
                    work.fallback = true;
                    work.stuck_reason = None;
                    if let Some(run) = work.runs.last_mut() {
                        run.finished_ms = Some(now);
                        run.outcome = "usage_limit".into();
                    }
                    Ok(())
                });
                if let Ok(updated) = updated {
                    self.notify(
                        "Кончился лимит Codex",
                        format!("Задача «{}» продолжается на {name}.", updated.title),
                    );
                }
            }
            Err(error) => {
                let dept = home.departments.get(&active.department);
                let snapshot = self.hub.task(&task);
                let work = snapshot.as_ref().and_then(|t| t.work.as_ref());
                let reason = match active.reason.get() {
                    Some(StopReason::Owner) => "остановлено хозяином".to_owned(),
                    Some(StopReason::Timeout) => format!(
                        "вышло время ({} мин)",
                        dept.map_or(0, |d| d.limits.timeout_mins)
                    ),
                    Some(StopReason::Budget) => format!(
                        "превышен бюджет задачи ${:.2}",
                        work.and_then(|w| w.budget_usd)
                            .or(dept.map(|d| d.limits.task_usd))
                            .unwrap_or_default()
                    ),
                    Some(StopReason::Month) => "кончился месячный лимит отдела".to_owned(),
                    Some(StopReason::Shutdown) => "сервер остановлен".to_owned(),
                    None => human_error(&error),
                };
                let mut summary = String::from("Что сделано:\n");
                if finished.history.is_empty() {
                    summary.push_str("- ни один шаг не завершился\n");
                }
                for record in &finished.history {
                    summary.push_str(&format!(
                        "- {} ({}): {}\n",
                        record.step,
                        record
                            .agent
                            .as_str()
                            .split(':')
                            .nth(1)
                            .unwrap_or(record.agent.as_str()),
                        short(&record.output.replace('\n', " "), 160)
                    ));
                }
                summary.push_str(&format!(
                    "Потрачено: {}",
                    work.map_or_else(
                        || ledger::spent(0.0, 0, 0),
                        |w| ledger::spent(w.cost_usd, w.input_tokens, w.output_tokens)
                    )
                ));
                let _ = self.hub.update_work(Self::by(), &task, |_, work| {
                    work.state = WorkState::Stuck;
                    work.stuck_reason = Some(reason);
                    work.result_summary = summary;
                    if let Some(run) = work.runs.last_mut() {
                        run.finished_ms = Some(now);
                        run.outcome = "stuck".into();
                    }
                    Ok(())
                });
            }
        }
    }
}

fn human_error(error: &str) -> String {
    if error.starts_with("until not satisfied") {
        "ревью не пройдено за отведённые круги".into()
    } else if error == "max_steps exceeded" {
        "превышено число шагов workflow".into()
    } else if error == "stopped" {
        "остановлено".into()
    } else {
        format!("ошибка: {}", short(error, 300))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{Scripted, Silent};

    fn temp() -> PathBuf {
        std::env::temp_dir().join(format!("agentsflow-runs-{}", Uuid::new_v4()))
    }

    /// Doer and reviewer answer from their scripts; `None` makes the doer hang
    /// until stopped.
    fn scripted(
        doer: Option<&'static [&'static str]>,
        reviewer: &'static [&'static str],
        cost: Option<f64>,
    ) -> RunEnv {
        RunEnv {
            codex_bin: None,
            spawner: Some(Rc::new(move |hub, member, dir| {
                let dir = dir.to_path_buf();
                let doer_role = member.roles.iter().any(|r| r == "doer");
                if doer_role && doer.is_none() {
                    return crate::hub::spawn(hub.clone(), member.id.clone(), true, || async {
                        Ok(Silent::new(10, false))
                    });
                }
                let replies = if doer_role { doer.unwrap() } else { reviewer };
                crate::hub::spawn(hub.clone(), member.id.clone(), true, move || {
                    let dir = dir.clone();
                    async move { Ok(Scripted::new(replies, cost, &dir)) }
                })
            })),
        }
    }

    async fn wait_state(hub: &Hub, id: &str, state: WorkState) -> TaskSnapshot {
        for _ in 0..500 {
            if let Some(task) = hub.task(id) {
                if task.work.as_ref().is_some_and(|w| w.state == state) {
                    return task;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("task {id} did not reach {state:?}: {:?}", hub.task(id));
    }

    fn run_case(env: RunEnv, check: impl AsyncFnOnce(Runs, Hub, TaskSnapshot, PathBuf)) {
        run_department(crate::department::tests::SAMPLE, false, env, check);
    }

    fn run_department(
        department: &str,
        small: bool,
        env: RunEnv,
        check: impl AsyncFnOnce(Runs, Hub, TaskSnapshot, PathBuf),
    ) {
        let dir = temp();
        let home = home_with(&dir, department);
        let hub = Hub::new();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(tokio::task::LocalSet::new().run_until(async {
                let runs = Runs::start(hub.clone(), home, Ledger::memory(), env);
                let task = runs
                    .create(
                        AgentId::from("owner"),
                        NewWork {
                            title: "Пост".into(),
                            text: "Анонс".into(),
                            department: "pr".into(),
                            risk: small.then_some(Risk::Internal),
                            small,
                            ..Default::default()
                        },
                    )
                    .unwrap();
                check(runs, hub, task, dir.clone()).await;
            }));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn approved_run_goes_to_review_with_result() {
        run_case(
            scripted(
                Some(&["write:Готовый пост"]),
                &["VERDICT: APPROVED"],
                Some(0.1),
            ),
            async |runs, hub, task, dir| {
                let done = wait_state(&hub, &task.id, WorkState::Review).await;
                let work = done.work.unwrap();
                assert_eq!(work.result_summary, "Готовый пост");
                assert_eq!(
                    work.result_path.as_deref(),
                    Some(format!("departments/pr/runs/{}/result.md", task.id).as_str())
                );
                assert!(dir.join(work.result_path.unwrap()).is_file());
                assert!((work.cost_usd - 0.2).abs() < 1e-9);
                assert_eq!(work.runs[0].outcome, "finished");
                assert!(runs.running().is_empty());
                let brief = std::fs::read_to_string(
                    dir.join("departments/pr/runs")
                        .join(&task.id)
                        .join("brief.md"),
                )
                .unwrap();
                assert!(brief.contains("Анонс"));
            },
        );
    }

    #[test]
    fn failed_review_gets_stuck_with_summary() {
        run_case(
            scripted(Some(&["черновик"]), &["плохо"], None),
            async |_, hub, task, _| {
                let stuck = wait_state(&hub, &task.id, WorkState::Stuck).await;
                let work = stuck.work.unwrap();
                assert_eq!(
                    work.stuck_reason.as_deref(),
                    Some("ревью не пройдено за отведённые круги")
                );
                assert!(work.result_summary.contains("review (critic): плохо"));
                assert_eq!(work.runs[0].outcome, "stuck");
            },
        );
    }

    #[test]
    fn budget_stops_the_run() {
        run_case(
            scripted(Some(&["черновик"]), &["плохо"], Some(2.0)),
            async |_, hub, task, _| {
                let stuck = wait_state(&hub, &task.id, WorkState::Stuck).await;
                let work = stuck.work.unwrap();
                assert_eq!(
                    work.stuck_reason.as_deref(),
                    Some("превышен бюджет задачи $2.50")
                );
                assert!(work.cost_usd >= 2.5);
            },
        );
    }

    #[test]
    fn owner_stop_and_requeue() {
        run_case(
            scripted(None, &["VERDICT: APPROVED"], None),
            async |runs, hub, task, _| {
                wait_state(&hub, &task.id, WorkState::Running).await;
                for _ in 0..100 {
                    if runs.stop(&task.id) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                let stuck = wait_state(&hub, &task.id, WorkState::Stuck).await;
                assert_eq!(
                    stuck.work.unwrap().stuck_reason.as_deref(),
                    Some("остановлено хозяином")
                );
                // "Ещё круг": the owner puts it back; it runs again.
                hub.update_task(
                    AgentId::orchestrator(),
                    &task.id,
                    crate::hub::TaskPatch {
                        status: Some(agentsflow_events::TaskStatus::Todo),
                        ..Default::default()
                    },
                )
                .unwrap();
                let again = wait_state(&hub, &task.id, WorkState::Running).await;
                assert_eq!(again.work.unwrap().runs.len(), 2);
            },
        );
    }

    /// Every agent answers by its kind and role, whichever ones get started.
    fn by_member() -> RunEnv {
        RunEnv {
            codex_bin: None,
            spawner: Some(Rc::new(|hub, member, dir| {
                let dir = dir.to_path_buf();
                let replies: &'static [&'static str] =
                    if member.roles.iter().any(|r| r == "reviewer") {
                        &["VERDICT: APPROVED"]
                    } else if member.kind == AgentKind::Codex {
                        &["failed:usage_limit_exceeded: You've hit your usage limit."]
                    } else if member.model.as_deref() == Some("claude-haiku-5-5") {
                        &["write:Сделал Haiku"]
                    } else {
                        &["write:Сделал Sonnet"]
                    };
                crate::hub::spawn(hub.clone(), member.id.clone(), true, move || {
                    let dir = dir.clone();
                    async move { Ok(Scripted::new(replies, Some(0.01), &dir)) }
                })
            })),
        }
    }

    #[test]
    fn usage_limit_moves_the_task_to_the_fallback() {
        let department = crate::department::tests::sample_with_spares();
        run_department(
            &department,
            false,
            by_member(),
            async |_, hub, task, dir| {
                let done = wait_state(&hub, &task.id, WorkState::Review).await;
                let work = done.work.unwrap();
                assert_eq!(work.result_summary, "Сделал Sonnet");
                assert!(work.fallback);
                assert_eq!(work.runs.len(), 2);
                assert_eq!(work.runs[0].outcome, "usage_limit");
                assert_eq!(work.runs[1].outcome, "finished");
                let brief = std::fs::read_to_string(
                    dir.join("departments/pr/runs")
                        .join(&task.id)
                        .join("brief.md"),
                )
                .unwrap();
                assert!(brief.contains("кончился его лимит"));
                assert!(
                    hub.mail_all()
                        .iter()
                        .any(|mail| mail.subject == "Кончился лимит Codex"
                            && mail.body.contains("claude-sonnet-5-5"))
                );
            },
        );
    }

    #[test]
    fn other_codex_failures_get_stuck() {
        let department = crate::department::tests::sample_with_spares();
        let env = RunEnv {
            codex_bin: None,
            spawner: Some(Rc::new(|hub, member, dir| {
                let dir = dir.to_path_buf();
                crate::hub::spawn(hub.clone(), member.id.clone(), true, move || {
                    let dir = dir.clone();
                    async move { Ok(Scripted::new(&["failed:сеть упала"], None, &dir)) }
                })
            })),
        };
        run_department(&department, false, env, async |_, hub, task, _| {
            let stuck = wait_state(&hub, &task.id, WorkState::Stuck).await;
            let work = stuck.work.unwrap();
            assert!(!work.fallback);
            assert!(work.stuck_reason.unwrap().contains("сеть упала"));
        });
    }

    #[test]
    fn small_task_runs_alone_and_a_return_gets_the_full_department() {
        let department = crate::department::tests::sample_with_spares();
        run_department(
            &department,
            true,
            by_member(),
            async |runs, hub, task, _| {
                let done = wait_state(&hub, &task.id, WorkState::Review).await;
                let work = done.work.unwrap();
                assert!(work.small);
                assert_eq!(work.result_summary, "Сделал Haiku");
                assert_eq!(work.runs.len(), 1);
                // Haiku worked alone: no reviewer ever spoke in this run.
                assert!(
                    hub.mail_all()
                        .iter()
                        .all(|mail| mail.subject != "Кончился лимит Codex")
                );
                runs.return_task(AgentId::from("owner"), &task.id, "мало")
                    .unwrap();
                for _ in 0..500 {
                    let work = hub.task(&task.id).unwrap().work.unwrap();
                    if work.state == WorkState::Review && work.runs.len() >= 3 {
                        // The full department: Codex hit the limit, Sonnet finished.
                        assert!(!work.small);
                        assert!(work.fallback);
                        assert_eq!(work.result_summary, "Сделал Sonnet");
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                panic!("no full run: {:?}", hub.task(&task.id));
            },
        );
    }

    #[test]
    fn external_tasks_are_never_small() {
        let department = crate::department::tests::sample_with_spares();
        let dir = temp();
        let home = home_with(&dir, &department);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(tokio::task::LocalSet::new().run_until(async {
                let runs = Runs::start(Hub::new(), home, Ledger::memory(), by_member());
                let task = runs
                    .create(
                        AgentId::from("owner"),
                        NewWork {
                            title: "Пост".into(),
                            text: "Анонс".into(),
                            department: "pr".into(),
                            risk: Some(Risk::External),
                            small: true,
                            ..Default::default()
                        },
                    )
                    .unwrap();
                assert!(!task.work.unwrap().small);
            }));
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn home_with(dir: &std::path::Path, department: &str) -> Home {
        crate::home::init(dir).unwrap();
        let pr = dir.join("departments").join("pr");
        std::fs::create_dir_all(&pr).unwrap();
        std::fs::write(pr.join("department.toml"), department).unwrap();
        Home::load(dir).unwrap()
    }

    #[test]
    fn human_errors() {
        assert_eq!(
            human_error("until not satisfied after 2 loops"),
            "ревью не пройдено за отведённые круги"
        );
        assert!(human_error("agent x: Closed").starts_with("ошибка:"));
    }

    #[test]
    fn create_validates_department_and_dependencies() {
        let dir = temp();
        let home = home_with(&dir, crate::department::tests::SAMPLE);
        let hub = Hub::new();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(tokio::task::LocalSet::new().run_until(async {
                let runs = Runs::start(hub.clone(), home, Ledger::memory(), RunEnv::default());
                let new = |department: &str| NewWork {
                    title: "t".into(),
                    text: "x".into(),
                    department: department.into(),
                    ..Default::default()
                };
                assert!(runs.create(AgentId::from("owner"), new("nope")).is_err());
                let mut with_dep = new("pr");
                with_dep.depends_on = vec!["missing".into()];
                assert!(runs.create(AgentId::from("owner"), with_dep).is_err());
                let mut asking = new("pr");
                asking.questions = vec![agentsflow_events::Question {
                    text: "где?".into(),
                    answer: None,
                    from: "orchestrator".into(),
                }];
                let task = runs.create(AgentId::from("owner"), asking).unwrap();
                let work = task.work.unwrap();
                assert_eq!(work.state, WorkState::Waiting);
                assert_eq!(work.risk, Risk::External);
            }));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Engine without agents: tasks are kept Waiting or moved by hand, so
    /// nothing is spawned.
    fn offline(check: impl AsyncFnOnce(Runs, Hub, PathBuf)) {
        let dir = temp();
        let home = home_with(&dir, crate::department::tests::SAMPLE);
        let hub = Hub::new();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(tokio::task::LocalSet::new().run_until(async {
                let runs = Runs::start(hub.clone(), home, Ledger::memory(), RunEnv::default());
                check(runs, hub, dir.clone()).await;
            }));
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn waiting(department: &str) -> NewWork {
        NewWork {
            title: "t".into(),
            text: "x".into(),
            department: department.into(),
            questions: vec![agentsflow_events::Question {
                text: "где?".into(),
                answer: None,
                from: "orchestrator".into(),
            }],
            ..Default::default()
        }
    }

    /// Puts a waiting task into Review as if a run had finished.
    fn to_review(hub: &Hub, id: &str, risk: Risk) {
        hub.update_work(AgentId::orchestrator(), id, |_, work| {
            work.state = WorkState::Review;
            work.risk = risk;
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn answers_release_waiting_tasks() {
        offline(async |runs, hub, _| {
            let mut new = waiting("pr");
            new.depends_on = vec![];
            let task = runs.create(AgentId::from("owner"), new).unwrap();
            // A second open question keeps it waiting until answered too.
            runs.ask(AgentId::orchestrator(), &task.id, &["когда?".into()])
                .unwrap();
            // Block the queue so the released task is not launched.
            hub.update_work(AgentId::orchestrator(), &task.id, |_, work| {
                work.depends_on = vec!["missing".into()];
                Ok(())
            })
            .unwrap();
            let answered = runs.answer(&task.id, "в Алматы, завтра").unwrap();
            let work = answered.work.unwrap();
            assert!(work.questions.iter().all(|q| q.answer.is_some()));
            assert_eq!(work.questions.len(), 2);
            assert_eq!(work.state, WorkState::Queued);
            assert!(runs.answer(&task.id, "ещё").is_err());
            assert!(runs.answer(&task.id, "  ").is_err());
        });
    }

    #[test]
    fn proposals_create_departments_or_fall_back_to_general() {
        offline(async |runs, hub, dir| {
            let proposal = agentsflow_events::DepartmentProposal {
                id: "hr".into(),
                title: "Кадры".into(),
                description: "Поиск людей".into(),
                rules: "Пиши коротко.".into(),
                default_risk: Risk::Internal,
            };
            let mut first = waiting("");
            first.questions.clear();
            first.proposal = Some(proposal.clone());
            let first = runs.create(AgentId::from("owner"), first).unwrap();
            assert_eq!(first.work.as_ref().unwrap().state, WorkState::Waiting);
            assert_eq!(first.work.as_ref().unwrap().department, "hr");
            // Still asks a question: created department alone does not run it.
            let mut second = waiting("");
            second.proposal = Some(proposal.clone());
            let second = runs.create(AgentId::from("owner"), second).unwrap();

            assert_eq!(runs.resolve_proposal(&first.id, true).unwrap(), "hr");
            assert!(runs.home().departments.contains_key("hr"));
            let rules = std::fs::read_to_string(dir.join("departments/hr/AGENTS.md")).unwrap();
            assert!(rules.contains("Пиши коротко."));
            let second = hub.task(&second.id).unwrap().work.unwrap();
            assert!(second.proposal.is_none());
            assert_eq!(second.state, WorkState::Waiting);
            assert!(
                runs.create(AgentId::from("owner"), {
                    let mut again = waiting("");
                    again.proposal = Some(proposal.clone());
                    again
                })
                .is_err()
            );

            let mut other = waiting("");
            other.proposal = Some(agentsflow_events::DepartmentProposal {
                id: "legal".into(),
                ..proposal
            });
            let other = runs.create(AgentId::from("owner"), other).unwrap();
            assert_eq!(runs.resolve_proposal(&other.id, false).unwrap(), "general");
            let other = hub.task(&other.id).unwrap().work.unwrap();
            assert_eq!(other.department, "general");
            assert!(!dir.join("departments/legal").exists());
        });
    }

    #[test]
    fn acceptance_respects_risk_and_returns_once() {
        offline(async |runs, hub, _| {
            let internal = runs.create(AgentId::from("owner"), waiting("pr")).unwrap();
            to_review(&hub, &internal.id, Risk::Internal);
            let done = runs.accept(AgentId::orchestrator(), &internal.id).unwrap();
            assert_eq!(done.work.unwrap().state, WorkState::Done);

            let external = runs.create(AgentId::from("owner"), waiting("pr")).unwrap();
            to_review(&hub, &external.id, Risk::External);
            assert!(runs.accept(AgentId::orchestrator(), &external.id).is_err());
            let sent = runs
                .to_owner(AgentId::orchestrator(), &external.id)
                .unwrap();
            assert!(sent.work.unwrap().owner_review);

            let redo = runs.create(AgentId::from("owner"), waiting("pr")).unwrap();
            to_review(&hub, &redo.id, Risk::Internal);
            // Keep it from launching once requeued.
            hub.update_work(AgentId::orchestrator(), &redo.id, |_, work| {
                work.depends_on = vec!["missing".into()];
                Ok(())
            })
            .unwrap();
            let back = runs
                .return_task(AgentId::orchestrator(), &redo.id, "нет цифр")
                .unwrap()
                .work
                .unwrap();
            assert_eq!(back.returns, 1);
            assert_eq!(back.review_notes, vec!["нет цифр".to_owned()]);
            assert!(matches!(back.state, WorkState::Queued | WorkState::Stuck));
            to_review(&hub, &redo.id, Risk::Internal);
            let owner = runs
                .return_task(AgentId::orchestrator(), &redo.id, "опять нет")
                .unwrap()
                .work
                .unwrap();
            assert!(owner.owner_review);
            assert_eq!(owner.state, WorkState::Review);
            assert!(runs.accept(AgentId::orchestrator(), &redo.id).is_err());
        });
    }

    #[test]
    fn runs_leave_memory_and_rule_proposals() {
        let env = RunEnv {
            codex_bin: None,
            spawner: Some(Rc::new(|hub, member, dir| {
                let dir = dir.to_path_buf();
                let doer = member.roles.iter().any(|r| r == "doer");
                if doer {
                    std::fs::write(dir.join(crate::rules::NOTE_FILE), "Канал: @orca").unwrap();
                    std::fs::write(dir.join(crate::rules::RULE_FILE), "Посты до 600 знаков")
                        .unwrap();
                }
                let replies: &'static [&'static str] = if doer {
                    &["write:Пост"]
                } else {
                    &["VERDICT: APPROVED"]
                };
                crate::hub::spawn(hub.clone(), member.id.clone(), true, move || {
                    let dir = dir.clone();
                    async move { Ok(Scripted::new(replies, None, &dir)) }
                })
            })),
        };
        run_case(env, async |runs, hub, task, dir| {
            wait_state(&hub, &task.id, WorkState::Review).await;
            let (history, _) = hub.subscribe();
            let notes =
                std::fs::read_to_string(dir.join("departments/pr/memory/notes.md")).unwrap();
            assert!(notes.contains("Канал: @orca"));
            let proposals = runs.rule_proposals();
            assert_eq!(proposals.len(), 1);
            assert_eq!(proposals[0].target, "pr");
            assert_eq!(proposals[0].id, task.id);
            let announced = history.iter().any(|envelope| {
                matches!(
                    envelope.event,
                    Event::Rule {
                        status: RuleStatus::Proposed,
                        ..
                    }
                )
            });
            assert!(announced);
            runs.approve_rule("pr", &task.id).unwrap();
            let rules = std::fs::read_to_string(dir.join("departments/pr/AGENTS.md")).unwrap();
            assert!(rules.contains("Посты до 600 знаков"));
        });
    }

    #[test]
    fn inputs_are_copied_into_the_run_folder() {
        let dir = std::env::temp_dir().join(format!("agentsflow-inputs-{}", Uuid::new_v4()));
        let source = dir.join("research/runs/t1");
        std::fs::create_dir_all(source.join("artifacts/raw")).unwrap();
        std::fs::write(source.join("result.md"), "цены").unwrap();
        std::fs::write(source.join("artifacts/raw/prices.md"), "таблица").unwrap();
        let run = dir.join("general/runs/t2");
        let input = brief::Input {
            task: "t1".into(),
            title: "Цены".into(),
            result: source.join("result.md"),
        };
        let local = copy_inputs(&[input], &run).unwrap();
        assert_eq!(local[0].result, run.join("inputs/t1/result.md"));
        assert_eq!(std::fs::read_to_string(&local[0].result).unwrap(), "цены");
        let table = run.join("inputs/t1/artifacts/raw/prices.md");
        assert_eq!(std::fs::read_to_string(table).unwrap(), "таблица");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn questions_skip_review_and_wait_for_the_owner() {
        let env = RunEnv {
            codex_bin: None,
            spawner: Some(Rc::new(|hub, member, dir| {
                let dir = dir.to_path_buf();
                let doer = member.roles.iter().any(|r| r == "doer");
                if doer {
                    std::fs::write(
                        dir.join(QUESTIONS_FILE),
                        "1. Какие услуги?\n- Какой рынок?\n",
                    )
                    .unwrap();
                }
                // A reviewer that never approves: reaching it would loop the run into stuck.
                let replies: &'static [&'static str] = if doer {
                    &["нужны данные"]
                } else {
                    &["нет"]
                };
                crate::hub::spawn(hub.clone(), member.id.clone(), true, move || {
                    let dir = dir.clone();
                    async move { Ok(Scripted::new(replies, None, &dir)) }
                })
            })),
        };
        run_case(env, async |runs, hub, task, dir| {
            let waiting = wait_state(&hub, &task.id, WorkState::Waiting).await;
            let work = waiting.work.unwrap();
            let asked: Vec<_> = work.questions.iter().map(|q| q.text.as_str()).collect();
            assert_eq!(asked, ["Какие услуги?", "Какой рынок?"]);
            assert_eq!(work.runs.last().unwrap().outcome, "questions");
            assert!(
                !dir.join("departments/pr/runs")
                    .join(&task.id)
                    .join(QUESTIONS_FILE)
                    .exists()
            );
            runs.answer(&task.id, "на свой вкус").unwrap();
            let again = hub.task(&task.id).unwrap().work.unwrap();
            assert_ne!(again.state, WorkState::Waiting);
        });
    }

    #[test]
    fn owner_returns_with_a_note() {
        offline(async |runs, hub, _| {
            let task = runs.create(AgentId::from("owner"), waiting("pr")).unwrap();
            assert!(runs.owner_return(&task.id, "короче").is_err());
            to_review(&hub, &task.id, Risk::External);
            assert!(runs.owner_return(&task.id, "  ").is_err());
            hub.update_work(AgentId::orchestrator(), &task.id, |_, work| {
                work.depends_on = vec!["missing".into()];
                Ok(())
            })
            .unwrap();
            let back = runs.owner_return(&task.id, "короче").unwrap().work.unwrap();
            assert_eq!(back.review_notes, vec!["хозяин: короче".to_owned()]);
            assert_eq!(back.returns, 0);
            assert!(matches!(back.state, WorkState::Queued | WorkState::Stuck));
        });
    }

    #[test]
    fn schedules_fire_once_and_survive_restarts() {
        let dir = temp();
        let text = format!(
            "{}\n[[schedule]]\ncron = \"0 9 * * *\"\ntask = \"Утренний отчёт\"\n",
            crate::department::tests::SAMPLE
        );
        let home = home_with(&dir, &text);
        // 2026-11-02 08:30 UTC.
        let t0 = 1_793_577_600_000 + 8 * 3_600_000 + 30 * 60_000;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let key = rt.block_on(tokio::task::LocalSet::new().run_until(async {
            let hub = Hub::new();
            let env = scripted(None, &["VERDICT: APPROVED"], None);
            let runs = Runs::start(hub.clone(), home.clone(), Ledger::memory(), env);
            assert!(
                runs.fire_schedules(t0).is_empty(),
                "first sight only counts"
            );
            let fired = runs.fire_schedules(t0 + 3_600_000);
            assert_eq!(fired.len(), 1);
            let task = hub.task(&fired[0]).unwrap();
            assert_eq!(task.title, "Утренний отчёт");
            assert_eq!(task.work.unwrap().source, "schedule");
            assert!(runs.fire_schedules(t0 + 3_660_000).is_empty());
            let info = runs.schedules();
            assert_eq!(info.len(), 1);
            info[0].key.clone()
        }));
        rt.block_on(tokio::task::LocalSet::new().run_until(async {
            // A restart reads the last firing back.
            let hub = Hub::new();
            let env = scripted(None, &["VERDICT: APPROVED"], None);
            let runs = Runs::start(hub.clone(), home.clone(), Ledger::memory(), env);
            assert!(runs.fire_schedules(t0 + 3_720_000).is_empty());
            let next_day = t0 + 24 * 3_600_000 + 31 * 60_000;
            runs.pause_schedule(&key, true).unwrap();
            assert!(runs.fire_schedules(next_day).is_empty());
            assert!(runs.schedules()[0].paused);
            runs.pause_schedule(&key, false).unwrap();
            assert!(runs.run_schedule(&key).is_ok());
            assert!(runs.run_schedule("pr-00000000").is_err());
            assert!(runs.pause_schedule("nope", true).is_err());
        }));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
