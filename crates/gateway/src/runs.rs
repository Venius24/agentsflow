//! Department runs: a queue of department tasks. Each run starts fresh agent
//! sessions in `departments/<id>/runs/<task>/`, executes the department
//! workflow and is stopped by loops, time, budget or the owner.

use crate::{
    brief,
    department::Department,
    home::Home,
    hub::{Hub, TaskError},
    ledger::{self, Ledger},
    runtime::{Agents, SpawnEnv, spawn_member},
    workflow::{self, StepRecord},
};
use agentsflow_events::{
    AgentId, AgentKind, Event, Risk, RunRef, TaskSnapshot, WorkInfo, WorkState,
};
use serde::Serialize;
use std::{
    cell::Cell,
    collections::{BTreeMap, HashMap},
    path::PathBuf,
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
    fn spawn(
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
        let (tx, rx) = mpsc::unbounded_channel();
        let runs = Self {
            hub: hub.clone(),
            ledger: ledger.clone(),
            shared: Arc::new(Mutex::new(Shared {
                home: Arc::new(home),
                running: BTreeMap::new(),
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
        runs
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

    /// Puts a department task in the queue.
    pub fn create(&self, by: AgentId, new: NewWork) -> Result<TaskSnapshot, String> {
        let home = self.home();
        let dept = home
            .departments
            .get(&new.department)
            .ok_or_else(|| format!("нет отдела {}", new.department))?;
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
        let waiting = new.questions.iter().any(|q| q.answer.is_none());
        let work = WorkInfo {
            department: dept.id.clone(),
            state: if waiting {
                WorkState::Waiting
            } else {
                WorkState::Queued
            },
            risk: new.risk.unwrap_or(dept.default_risk),
            parent: new.parent,
            depends_on: new.depends_on,
            budget_usd: new.budget_usd,
            source: new.source,
            questions: new.questions,
            ..Default::default()
        };
        let task = self
            .hub
            .create_work(by, &new.title, new.text, work)
            .map_err(|error| error.to_string())?;
        self.kick();
        Ok(task)
    }
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
            if let Some(entry) = work.runs.iter_mut().find(|r| r.id == run) {
                entry.cost_usd += usd;
            }
            Ok(())
        });
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
        let text = brief::build(dept, task, inputs, home.config.memory_digest_kb, &run_dir);
        std::fs::write(run_dir.join("brief.md"), &text)?;
        let mut plan = dept.run_plan(&task.id)?;
        plan.spec.bind("brief", text);
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

    fn finish(&mut self, finished: Finished) {
        let Some(active) = self.active.remove(&finished.task) else {
            return;
        };
        self.agents.retain(|_, meta| meta.task != finished.task);
        self.shared.lock().unwrap().running.remove(&finished.task);
        let home = self.home();
        let now = now_ms();
        let task = finished.task.clone();
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
                    "Потрачено: ${:.2}",
                    work.map_or(0.0, |w| w.cost_usd)
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
        let dir = temp();
        let home = home_with(&dir, crate::department::tests::SAMPLE);
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
}
