use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fs::{File, OpenOptions},
    future::Future,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use agentsflow_adapters::{Agent, Interrupter};
use agentsflow_events::{AgentId, Envelope, Event, Role, TaskSnapshot, TaskStatus, WorkflowStatus};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{broadcast, mpsc, oneshot, watch},
    task::JoinHandle,
};
use uuid::Uuid;

#[derive(Clone)]
pub struct Hub {
    inner: Arc<Inner>,
}
struct Inner {
    seq: AtomicU64,
    data: Mutex<Data>,
    tx: broadcast::Sender<Envelope>,
    log: Option<LogWriter>,
}
struct Data {
    history: VecDeque<Envelope>,
    mail: Vec<MailMsg>,
    read: HashSet<String>,
    mail_thread_limit: usize,
    mail_enabled: bool,
    tasks: Vec<TaskSnapshot>,
    stats: Statistics,
    resolution_origins: HashMap<u64, ResolutionOrigin>,
}

#[derive(Clone, Copy)]
pub(crate) enum ResolutionOrigin {
    Response,
    Interrupt,
    Guard,
    Automatic,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AgentStats {
    turns: u64,
    turn_errors: u64,
    interrupted: u64,
    busy_ms: u64,
    tool_calls: u64,
    permission_requests: u64,
    guard_denied: u64,
    mails_sent: u64,
    mails_received: u64,
    last_activity_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct StatsTotals {
    workflows_total: u64,
    workflows_finished: u64,
    workflows_failed: u64,
    workflows_stopped: u64,
    mails_total: u64,
}

#[derive(Default)]
struct Statistics {
    agents: BTreeMap<AgentId, AgentStats>,
    totals: StatsTotals,
    turns: HashMap<AgentId, u64>,
}

#[derive(Serialize)]
pub struct StatsSnapshot {
    generated_ms: u64,
    pub agents: BTreeMap<AgentId, AgentStats>,
    totals: StatsTotals,
    tasks: BTreeMap<String, usize>,
}

impl Statistics {
    fn record(&mut self, envelope: &Envelope) {
        let agent = envelope.agent.clone();
        let stats = self.agents.entry(agent.clone()).or_default();
        stats.last_activity_ms = stats.last_activity_ms.max(envelope.ts_ms);
        match &envelope.event {
            Event::Message {
                role: Role::User, ..
            } => {
                self.turns.insert(agent, envelope.ts_ms);
            }
            Event::Done { reason } => {
                stats.turns += 1;
                stats.turn_errors += u64::from(reason == "error");
                stats.interrupted += u64::from(reason.starts_with("interrupted"));
                if let Some(start) = self.turns.remove(&agent) {
                    stats.busy_ms += envelope.ts_ms.saturating_sub(start);
                }
            }
            Event::ToolCall { .. } => stats.tool_calls += 1,
            Event::PermissionRequest { .. } => stats.permission_requests += 1,
            Event::PermissionResolved {
                allow: false,
                blocked_by_guard: true,
                ..
            } => stats.guard_denied += 1,
            Event::Mail { from, to, .. } => {
                self.agents.entry(from.clone()).or_default().mails_sent += 1;
                let recipient = self.agents.entry(to.clone()).or_default();
                recipient.mails_received += 1;
                recipient.last_activity_ms = recipient.last_activity_ms.max(envelope.ts_ms);
                self.totals.mails_total += 1;
            }
            Event::Workflow { status, detail, .. } => match status {
                WorkflowStatus::Started => self.totals.workflows_total += 1,
                WorkflowStatus::Finished => self.totals.workflows_finished += 1,
                WorkflowStatus::Failed
                    if detail == "stopped" || detail.starts_with("interrupted") =>
                {
                    self.totals.workflows_stopped += 1
                }
                WorkflowStatus::Failed => self.totals.workflows_failed += 1,
                _ => {}
            },
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewTask {
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub assignee: Option<AgentId>,
    #[serde(default)]
    pub status: Option<TaskStatus>,
    #[serde(default)]
    pub labels: Vec<String>,
}

fn nullable_assignee<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<Option<AgentId>>, D::Error> {
    Option::<AgentId>::deserialize(d).map(Some)
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskPatch {
    pub title: Option<String>,
    pub description: Option<String>,
    #[serde(default, deserialize_with = "nullable_assignee")]
    pub assignee: Option<Option<AgentId>>,
    pub status: Option<TaskStatus>,
    pub progress: Option<u8>,
    pub labels: Option<Vec<String>>,
    pub note: Option<String>,
    #[serde(skip)]
    pub thread: Option<Option<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskError {
    NotFound,
    Limit,
    Forbidden,
    MailDisabled,
    NoAssignee,
    Mail(MailError),
    Invalid(&'static str),
}
impl std::fmt::Display for TaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => f.write_str("task not found"),
            Self::Limit => f.write_str("task limit exceeded"),
            Self::Forbidden => f.write_str("task is assigned to another agent"),
            Self::MailDisabled => f.write_str("mail is disabled"),
            Self::NoAssignee => f.write_str("no assignee"),
            Self::Mail(error) => error.fmt(f),
            Self::Invalid(s) => f.write_str(s),
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
fn validate_task(task: &TaskSnapshot) -> std::result::Result<(), TaskError> {
    if task.title.trim().is_empty() || task.title.trim().chars().count() > 200 {
        return Err(TaskError::Invalid("title must contain 1 to 200 characters"));
    }
    if task.description.chars().count() > 8000 {
        return Err(TaskError::Invalid("description exceeds 8000 characters"));
    }
    if task.note.chars().count() > 500 {
        return Err(TaskError::Invalid("note exceeds 500 characters"));
    }
    if task.progress > 100 {
        return Err(TaskError::Invalid("progress exceeds 100"));
    }
    if task.labels.len() > 8
        || task.labels.iter().any(|label| {
            label.is_empty()
                || label.len() > 24
                || !label
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
        })
    {
        return Err(TaskError::Invalid("invalid labels"));
    }
    Ok(())
}
fn store_task(tasks: &mut Vec<TaskSnapshot>, task: &TaskSnapshot) {
    if let Some(existing) = tasks.iter_mut().find(|item| item.id == task.id) {
        *existing = task.clone();
    } else {
        tasks.push(task.clone());
    }
    tasks.sort_by_key(|item| item.created_ms);
}

pub const MAX_THREAD_MAILS: usize = 12;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MailMsg {
    pub id: String,
    pub thread: String,
    pub from: AgentId,
    pub to: AgentId,
    pub subject: String,
    pub body: String,
    pub seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MailError {
    SameAgent,
    Subject,
    Body,
    UnknownThread,
    ThreadLimit,
}

impl std::fmt::Display for MailError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::SameAgent => "sender and recipient must differ",
            Self::Subject => "subject must contain 1 to 200 characters",
            Self::Body => "body must contain 1 to 65536 bytes",
            Self::UnknownThread => "unknown mail thread",
            Self::ThreadLimit => "mail thread limit exceeded",
        })
    }
}

fn mail_state(history: &VecDeque<Envelope>) -> (Vec<MailMsg>, HashSet<String>) {
    let mut mail = Vec::new();
    let mut read = HashSet::new();
    for envelope in history {
        match &envelope.event {
            Event::Mail {
                id,
                thread,
                from,
                to,
                subject,
                body,
            } => mail.push(MailMsg {
                id: id.clone(),
                thread: thread.clone(),
                from: from.clone(),
                to: to.clone(),
                subject: subject.clone(),
                body: body.clone(),
                seq: envelope.seq,
            }),
            Event::MailRead { ids, .. } => read.extend(ids.iter().cloned()),
            _ => {}
        }
    }
    (mail, read)
}

const HISTORY_LIMIT: usize = 5000;

struct LogWriter {
    tx: Option<std::sync::mpsc::Sender<Envelope>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for LogWriter {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl LogWriter {
    fn start(path: &Path) -> Self {
        let path = path.to_owned();
        let (tx, rx) = std::sync::mpsc::channel::<Envelope>();
        let thread = std::thread::spawn(move || {
            let result = (|| -> std::io::Result<()> {
                let mut file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .read(true)
                    .open(path)?;
                if file.metadata()?.len() > 0 {
                    file.seek(SeekFrom::End(-1))?;
                    let mut last = [0];
                    file.read_exact(&mut last)?;
                    if last[0] != b'\n' {
                        file.write_all(b"\n")?;
                    }
                }
                for envelope in rx {
                    let mut line = serde_json::to_vec(&envelope)?;
                    line.push(b'\n');
                    file.write_all(&line)?;
                }
                Ok(())
            })();
            if let Err(error) = result {
                eprintln!("event log write failed: {error}");
            }
        });
        Self {
            tx: Some(tx),
            thread: Some(thread),
        }
    }
}

/// Events that close whatever a previous run left open (permission requests,
/// agent turns, workflows), so a replayed history does not show them as live.
fn interrupted(history: &VecDeque<Envelope>) -> Vec<(AgentId, String, Event)> {
    const REASON: &str = "interrupted: server restarted";
    let mut permissions: Vec<(AgentId, String, String)> = Vec::new();
    let mut turns: Vec<(AgentId, String, bool)> = Vec::new();
    let mut workflows: Vec<(String, String)> = Vec::new();
    for envelope in history {
        let agent = envelope.agent.clone();
        match &envelope.event {
            Event::PermissionRequest { id, .. } => {
                permissions.push((agent.clone(), envelope.session.clone(), id.clone()));
            }
            Event::PermissionResolved { id, .. } => permissions.retain(|(_, _, open)| open != id),
            Event::Workflow { name, status, .. } => match status {
                WorkflowStatus::Started => workflows.push((name.clone(), envelope.session.clone())),
                WorkflowStatus::Finished | WorkflowStatus::Failed => {
                    workflows.retain(|(open, _)| open != name);
                }
                _ => {}
            },
            _ => {}
        }
        if agent != AgentId::orchestrator()
            && !matches!(envelope.event, Event::Mail { .. } | Event::MailRead { .. })
        {
            let done = matches!(envelope.event, Event::Done { .. });
            match turns.iter_mut().find(|(known, _, _)| *known == agent) {
                Some(turn) => *turn = (agent, envelope.session.clone(), done),
                None => turns.push((agent, envelope.session.clone(), done)),
            }
        }
    }
    let mut closers = Vec::new();
    for (agent, session, id) in permissions {
        let event = Event::PermissionResolved {
            id,
            allow: false,
            blocked_by_guard: false,
        };
        closers.push((agent, session, event));
    }
    for (agent, session, done) in turns {
        if !done {
            closers.push((
                agent,
                session,
                Event::Done {
                    reason: REASON.into(),
                },
            ));
        }
    }
    for (name, session) in workflows {
        let event = Event::Workflow {
            name,
            step: None,
            status: WorkflowStatus::Failed,
            detail: REASON.into(),
        };
        closers.push((AgentId::orchestrator(), session, event));
    }
    closers
}

fn compact(path: &Path, history: &VecDeque<Envelope>) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    let mut bytes = Vec::new();
    for envelope in history {
        serde_json::to_writer(&mut bytes, envelope)?;
        bytes.push(b'\n');
    }
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

impl Hub {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self {
            inner: Arc::new(Inner {
                seq: AtomicU64::new(0),
                data: Mutex::new(Data {
                    history: VecDeque::new(),
                    mail: Vec::new(),
                    read: HashSet::new(),
                    mail_thread_limit: MAX_THREAD_MAILS,
                    mail_enabled: false,
                    tasks: Vec::new(),
                    stats: Statistics::default(),
                    resolution_origins: HashMap::new(),
                }),
                tx,
                log: None,
            }),
        }
    }
    pub fn with_log(path: &Path) -> Self {
        let mut history = VecDeque::new();
        let mut seq = 0;
        let mut lines = 0;
        match File::open(path) {
            Ok(file) => {
                let mut reader = BufReader::new(file);
                let mut line = Vec::new();
                let mut warned = false;
                loop {
                    line.clear();
                    match reader.read_until(b'\n', &mut line) {
                        Ok(0) => break,
                        Ok(_) => match serde_json::from_slice::<Envelope>(&line) {
                            Ok(envelope) => {
                                lines += 1;
                                seq = seq.max(envelope.seq);
                                if history.len() == HISTORY_LIMIT {
                                    history.pop_front();
                                }
                                history.push_back(envelope);
                            }
                            Err(_) => {
                                if !warned {
                                    eprintln!("event log contains invalid lines; skipping them");
                                    warned = true;
                                }
                            }
                        },
                        Err(error) => {
                            eprintln!("event log read failed: {error}");
                            break;
                        }
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => eprintln!("event log read failed: {error}"),
        }
        // Keep the file bounded: drop what no longer fits the replayed history.
        if lines > history.len() {
            if let Err(error) = compact(path, &history) {
                eprintln!("event log compaction failed: {error}");
            }
        }
        let closers = interrupted(&history);
        let (mail, read) = mail_state(&history);
        let mut tasks = Vec::new();
        let mut stats = Statistics::default();
        for envelope in &history {
            stats.record(envelope);
            if let Event::Task { task } = &envelope.event {
                store_task(&mut tasks, task);
            }
        }
        let (tx, _) = broadcast::channel(1024);
        let hub = Self {
            inner: Arc::new(Inner {
                seq: AtomicU64::new(seq),
                data: Mutex::new(Data {
                    history,
                    mail,
                    read,
                    mail_thread_limit: MAX_THREAD_MAILS,
                    mail_enabled: false,
                    tasks,
                    stats,
                    resolution_origins: HashMap::new(),
                }),
                tx,
                log: Some(LogWriter::start(path)),
            }),
        };
        for (agent, session, event) in closers {
            hub.publish(agent, &session, event);
        }
        hub
    }
    pub fn publish(&self, agent: AgentId, session: &str, event: Event) {
        let mut data = self.inner.data.lock().unwrap();
        self.publish_locked(&mut data, agent, session, event);
    }
    fn publish_resolution(
        &self,
        agent: AgentId,
        session: &str,
        event: Event,
        origin: ResolutionOrigin,
    ) {
        let mut data = self.inner.data.lock().unwrap();
        let seq = self.inner.seq.load(Ordering::Relaxed) + 1;
        data.resolution_origins.insert(seq, origin);
        self.publish_locked(&mut data, agent, session, event);
    }
    pub(crate) fn resolution_origin(&self, seq: u64) -> Option<ResolutionOrigin> {
        self.inner
            .data
            .lock()
            .unwrap()
            .resolution_origins
            .get(&seq)
            .copied()
    }
    fn publish_locked(&self, data: &mut Data, agent: AgentId, session: &str, event: Event) -> u64 {
        let envelope = Envelope {
            session: session.into(),
            agent,
            seq: self.inner.seq.fetch_add(1, Ordering::Relaxed) + 1,
            ts_ms: now_ms(),
            event,
        };
        data.stats.record(&envelope);
        if let Event::Task { task } = &envelope.event {
            store_task(&mut data.tasks, task);
        }
        if data.history.len() == HISTORY_LIMIT {
            if let Some(old) = data.history.pop_front() {
                data.resolution_origins.remove(&old.seq);
            }
        }
        data.history.push_back(envelope.clone());
        if let Some(log) = &self.inner.log {
            if let Some(tx) = &log.tx {
                let _ = tx.send(envelope.clone());
            }
        }
        let seq = envelope.seq;
        let _ = self.inner.tx.send(envelope);
        seq
    }
    pub fn set_mail_thread_limit(&self, limit: usize) {
        self.inner.data.lock().unwrap().mail_thread_limit = limit;
    }
    pub fn set_mail_enabled(&self, enabled: bool) {
        self.inner.data.lock().unwrap().mail_enabled = enabled;
    }
    pub fn mail_all(&self) -> Vec<MailMsg> {
        self.inner.data.lock().unwrap().mail.clone()
    }
    pub fn mail_enabled(&self) -> bool {
        self.inner.data.lock().unwrap().mail_enabled
    }
    pub fn stats(&self) -> StatsSnapshot {
        let data = self.inner.data.lock().unwrap();
        let mut tasks = BTreeMap::new();
        for status in [
            "backlog",
            "todo",
            "in_progress",
            "review",
            "done",
            "blocked",
            "archived",
        ] {
            tasks.insert(status.to_string(), 0);
        }
        for task in &data.tasks {
            if let Ok(serde_json::Value::String(status)) = serde_json::to_value(&task.status) {
                *tasks.entry(status).or_default() += 1;
            }
        }
        StatsSnapshot {
            generated_ms: now_ms(),
            agents: data.stats.agents.clone(),
            totals: data.stats.totals.clone(),
            tasks,
        }
    }
    pub fn tasks(&self, include_archived: bool) -> Vec<TaskSnapshot> {
        self.inner
            .data
            .lock()
            .unwrap()
            .tasks
            .iter()
            .filter(|t| include_archived || t.status != TaskStatus::Archived)
            .cloned()
            .collect()
    }
    #[allow(dead_code)]
    pub fn task(&self, id: &str) -> Option<TaskSnapshot> {
        self.inner
            .data
            .lock()
            .unwrap()
            .tasks
            .iter()
            .find(|task| task.id == id)
            .cloned()
    }
    pub fn create_task(
        &self,
        by: AgentId,
        new: NewTask,
    ) -> std::result::Result<TaskSnapshot, TaskError> {
        let mut data = self.inner.data.lock().unwrap();
        let status = new.status.unwrap_or(if new.assignee.is_some() {
            TaskStatus::Todo
        } else {
            TaskStatus::Backlog
        });
        if status != TaskStatus::Archived
            && data
                .tasks
                .iter()
                .filter(|t| t.status != TaskStatus::Archived)
                .count()
                >= 500
        {
            return Err(TaskError::Limit);
        }
        let id = loop {
            let id = Uuid::new_v4().simple().to_string()[..8].to_owned();
            if !data.tasks.iter().any(|t| t.id == id) {
                break id;
            }
        };
        let now = now_ms();
        let task = TaskSnapshot {
            id,
            title: new.title.trim().into(),
            description: new.description,
            status,
            assignee: new.assignee,
            progress: 0,
            labels: new.labels,
            note: String::new(),
            thread: None,
            created_ms: now,
            updated_ms: now,
        };
        let mut task = task;
        if task.status == TaskStatus::Done {
            task.progress = 100;
        }
        validate_task(&task)?;
        self.publish_locked(&mut data, by, "tasks", Event::Task { task: task.clone() });
        Ok(task)
    }
    pub fn update_task(
        &self,
        by: AgentId,
        id: &str,
        patch: TaskPatch,
    ) -> std::result::Result<TaskSnapshot, TaskError> {
        self.update_task_inner(by, id, patch, false)
    }
    pub fn update_task_owned(
        &self,
        by: AgentId,
        id: &str,
        patch: TaskPatch,
    ) -> std::result::Result<TaskSnapshot, TaskError> {
        self.update_task_inner(by, id, patch, true)
    }
    fn update_task_inner(
        &self,
        by: AgentId,
        id: &str,
        patch: TaskPatch,
        owned: bool,
    ) -> std::result::Result<TaskSnapshot, TaskError> {
        let mut data = self.inner.data.lock().unwrap();
        let mut task = data
            .tasks
            .iter()
            .find(|t| t.id == id)
            .cloned()
            .ok_or(TaskError::NotFound)?;
        if owned && task.assignee.as_ref() != Some(&by) {
            return Err(TaskError::Forbidden);
        }
        let was_archived = task.status == TaskStatus::Archived;
        if let Some(title) = patch.title {
            task.title = title.trim().into();
        }
        if let Some(description) = patch.description {
            task.description = description;
        }
        if let Some(assignee) = patch.assignee {
            task.assignee = assignee;
        }
        if let Some(status) = patch.status {
            task.status = status;
        }
        if let Some(progress) = patch.progress {
            if progress > 100 {
                return Err(TaskError::Invalid("progress exceeds 100"));
            }
            task.progress = progress;
        }
        if let Some(labels) = patch.labels {
            task.labels = labels;
        }
        if let Some(note) = patch.note {
            task.note = note;
        }
        if let Some(thread) = patch.thread {
            task.thread = thread;
        }
        if task.status == TaskStatus::Done {
            task.progress = 100;
        }
        if was_archived
            && task.status != TaskStatus::Archived
            && data
                .tasks
                .iter()
                .filter(|t| t.status != TaskStatus::Archived)
                .count()
                >= 500
        {
            return Err(TaskError::Limit);
        }
        task.updated_ms = now_ms();
        validate_task(&task)?;
        self.publish_locked(&mut data, by, "tasks", Event::Task { task: task.clone() });
        Ok(task)
    }
    pub fn send_mail(
        &self,
        from: AgentId,
        to: AgentId,
        subject: String,
        body: String,
        thread: Option<String>,
    ) -> std::result::Result<MailMsg, MailError> {
        let mut data = self.inner.data.lock().unwrap();
        self.send_mail_locked(&mut data, from, to, subject, body, thread)
    }
    fn send_mail_locked(
        &self,
        data: &mut Data,
        from: AgentId,
        to: AgentId,
        subject: String,
        body: String,
        thread: Option<String>,
    ) -> std::result::Result<MailMsg, MailError> {
        if from == to {
            return Err(MailError::SameAgent);
        }
        if subject.is_empty() || subject.chars().count() > 200 {
            return Err(MailError::Subject);
        }
        if body.is_empty() || body.len() > 65536 {
            return Err(MailError::Body);
        }
        let short_id = || Uuid::new_v4().simple().to_string()[..8].to_owned();
        let thread = if let Some(thread) = thread {
            if !data.mail.iter().any(|mail| mail.thread == thread) {
                return Err(MailError::UnknownThread);
            }
            thread
        } else {
            loop {
                let id = short_id();
                if !data.mail.iter().any(|mail| mail.thread == id) {
                    break id;
                }
            }
        };
        if data
            .mail
            .iter()
            .filter(|mail| mail.thread == thread)
            .count()
            >= data.mail_thread_limit
        {
            self.publish_locked(
                data,
                AgentId::orchestrator(),
                "mail",
                Event::Error {
                    message: MailError::ThreadLimit.to_string(),
                },
            );
            return Err(MailError::ThreadLimit);
        }
        let id = loop {
            let id = short_id();
            if !data.mail.iter().any(|mail| mail.id == id) {
                break id;
            }
        };
        let event = Event::Mail {
            id: id.clone(),
            thread: thread.clone(),
            from: from.clone(),
            to: to.clone(),
            subject: subject.clone(),
            body: body.clone(),
        };
        let seq = self.publish_locked(data, from.clone(), "mail", event);
        let mail = MailMsg {
            id,
            thread,
            from,
            to,
            subject,
            body,
            seq,
        };
        data.mail.push(mail.clone());
        Ok(mail)
    }
    pub fn dispatch_task(&self, id: &str) -> std::result::Result<TaskSnapshot, TaskError> {
        let mut data = self.inner.data.lock().unwrap();
        let mut task = data
            .tasks
            .iter()
            .find(|task| task.id == id)
            .cloned()
            .ok_or(TaskError::NotFound)?;
        let assignee = task.assignee.clone().ok_or(TaskError::NoAssignee)?;
        if !data.mail_enabled {
            return Err(TaskError::MailDisabled);
        }
        let prefix = format!("task {}: ", task.id);
        let subject = format!(
            "{}{}",
            prefix,
            task.title
                .chars()
                .take(200 - prefix.chars().count())
                .collect::<String>()
        );
        let body = format!(
            "{}\n\nUpdate your card with the update_task tool (id {}): status, progress 0-100 and a short note. When you finish set status to review.",
            task.description, task.id
        );
        let mail = self
            .send_mail_locked(
                &mut data,
                AgentId::orchestrator(),
                assignee,
                subject,
                body,
                None,
            )
            .map_err(TaskError::Mail)?;
        task.thread = Some(mail.thread);
        if matches!(task.status, TaskStatus::Backlog | TaskStatus::Todo) {
            task.status = TaskStatus::InProgress;
        }
        task.updated_ms = now_ms();
        self.publish_locked(
            &mut data,
            AgentId::orchestrator(),
            "tasks",
            Event::Task { task: task.clone() },
        );
        Ok(task)
    }
    pub fn unread(&self, agent: AgentId) -> Vec<MailMsg> {
        let data = self.inner.data.lock().unwrap();
        data.mail
            .iter()
            .filter(|mail| mail.to == agent && !data.read.contains(&mail.id))
            .cloned()
            .collect()
    }
    pub fn unread_count(&self, agent: AgentId) -> usize {
        self.unread(agent).len()
    }
    pub fn take_inbox(&self, agent: AgentId) -> Vec<MailMsg> {
        let mut data = self.inner.data.lock().unwrap();
        let mail: Vec<_> = data
            .mail
            .iter()
            .filter(|mail| mail.to == agent && !data.read.contains(&mail.id))
            .cloned()
            .collect();
        if !mail.is_empty() {
            let ids: Vec<_> = mail.iter().map(|mail| mail.id.clone()).collect();
            data.read.extend(ids.iter().cloned());
            self.publish_locked(
                &mut data,
                agent.clone(),
                "mail",
                Event::MailRead { agent, ids },
            );
        }
        mail
    }
    pub fn take_thread_for(&self, agent: AgentId, thread: &str) -> Option<MailMsg> {
        let mut data = self.inner.data.lock().unwrap();
        let mail = data
            .mail
            .iter()
            .find(|mail| mail.to == agent && mail.thread == thread && !data.read.contains(&mail.id))
            .cloned()?;
        data.read.insert(mail.id.clone());
        self.publish_locked(
            &mut data,
            agent.clone(),
            "mail",
            Event::MailRead {
                agent,
                ids: vec![mail.id.clone()],
            },
        );
        Some(mail)
    }
    pub fn subscribe(&self) -> (Vec<Envelope>, broadcast::Receiver<Envelope>) {
        let data = self.inner.data.lock().unwrap();
        let rx = self.inner.tx.subscribe();
        (data.history.iter().cloned().collect(), rx)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HubError {
    Busy,
    NotBusy,
    NotFound,
    Closed,
}
#[derive(Clone, Default, Serialize)]
pub struct AgentState {
    pub enabled: bool,
    pub busy: bool,
    pub pending: Vec<String>,
}
#[derive(Clone)]
pub struct Actor {
    tx: mpsc::UnboundedSender<Command>,
    stop: Arc<watch::Sender<bool>>,
    state: Arc<Mutex<AgentState>>,
}
type Reply = oneshot::Sender<std::result::Result<(), HubError>>;
enum Command {
    Prompt(String, Reply),
    Respond(String, bool, Reply),
    Interrupt(Reply),
    Shutdown(Reply),
}

impl Actor {
    pub fn state(&self) -> AgentState {
        self.state.lock().unwrap().clone()
    }
    pub async fn prompt(&self, prompt: String) -> std::result::Result<(), HubError> {
        let (tx, rx) = oneshot::channel();
        {
            let mut state = self.state.lock().unwrap();
            if state.busy {
                return Err(HubError::Busy);
            }
            state.busy = true;
            if self.tx.send(Command::Prompt(prompt, tx)).is_err() {
                state.busy = false;
                return Err(HubError::Closed);
            }
        }
        rx.await.unwrap_or(Err(HubError::Closed))
    }
    pub async fn respond(&self, id: String, allow: bool) -> std::result::Result<(), HubError> {
        let (tx, rx) = oneshot::channel();
        {
            let mut state = self.state.lock().unwrap();
            let Some(index) = state.pending.iter().position(|pending| pending == &id) else {
                return Err(HubError::NotFound);
            };
            state.pending.remove(index);
            if self
                .tx
                .send(Command::Respond(id.clone(), allow, tx))
                .is_err()
            {
                state.pending.push(id);
                return Err(HubError::Closed);
            }
        }
        rx.await.unwrap_or(Err(HubError::Closed))
    }
    /// Asks the agent to stop the turn it is working on; the turn then ends
    /// with a `done` event whose reason is "interrupted".
    pub async fn interrupt(&self) -> std::result::Result<(), HubError> {
        let (tx, rx) = oneshot::channel();
        {
            let state = self.state.lock().unwrap();
            if !state.busy {
                return Err(HubError::NotBusy);
            }
            if self.tx.send(Command::Interrupt(tx)).is_err() {
                return Err(HubError::Closed);
            }
        }
        rx.await.unwrap_or(Err(HubError::Closed))
    }
    pub async fn shutdown(&self) {
        // Interrupts a turn that is blocked in `next_events`, which cannot
        // observe the command channel; the session is discarded anyway.
        let _ = self.stop.send(true);
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Command::Shutdown(tx)).is_ok() {
            let _ = rx.await;
        }
    }
}

/// How long an interrupted turn may take to end before its session is dropped.
const INTERRUPT_GRACE: Duration = Duration::from_secs(10);

#[cfg(test)]
pub fn spawn<A, F, Fut>(hub: Hub, agent: AgentId, yes: bool, factory: F) -> (Actor, JoinHandle<()>)
where
    A: Agent + 'static,
    F: Fn() -> Fut + 'static,
    Fut: Future<Output = Result<A>> + 'static,
{
    spawn_with(hub, agent, yes, factory, INTERRUPT_GRACE)
}

pub fn spawn_guarded<A, F, Fut>(
    hub: Hub,
    agent: AgentId,
    yes: bool,
    cwd: std::path::PathBuf,
    enabled: bool,
    smart: bool,
    factory: F,
) -> (Actor, JoinHandle<()>)
where
    A: Agent + 'static,
    F: Fn() -> Fut + 'static,
    Fut: Future<Output = Result<A>> + 'static,
{
    spawn_configured(
        hub,
        agent,
        yes,
        factory,
        INTERRUPT_GRACE,
        Some((cwd, enabled)),
        smart,
    )
}

#[cfg(test)]
fn spawn_with<A, F, Fut>(
    hub: Hub,
    agent: AgentId,
    yes: bool,
    factory: F,
    grace: Duration,
) -> (Actor, JoinHandle<()>)
where
    A: Agent + 'static,
    F: Fn() -> Fut + 'static,
    Fut: Future<Output = Result<A>> + 'static,
{
    spawn_configured(hub, agent, yes, factory, grace, None, false)
}

fn spawn_configured<A, F, Fut>(
    hub: Hub,
    agent: AgentId,
    yes: bool,
    factory: F,
    grace: Duration,
    guard: Option<(std::path::PathBuf, bool)>,
    smart: bool,
) -> (Actor, JoinHandle<()>)
where
    A: Agent + 'static,
    F: Fn() -> Fut + 'static,
    Fut: Future<Output = Result<A>> + 'static,
{
    let (tx, rx) = mpsc::unbounded_channel();
    let (stop_tx, stop_rx) = watch::channel(false);
    let state = Arc::new(Mutex::new(AgentState {
        enabled: true,
        ..Default::default()
    }));
    let actor = Actor {
        tx,
        stop: Arc::new(stop_tx),
        state: state.clone(),
    };
    let task = tokio::task::spawn_local(run_actor(
        hub, agent, yes, factory, grace, state, rx, stop_rx, guard, smart,
    ));
    (actor, task)
}

async fn close<A: Agent>(session: &mut Option<A>) {
    if let Some(session) = session.take() {
        if let Err(error) = session.shutdown().await {
            eprintln!("agent shutdown: {error:#}");
        }
    }
}

enum Turn {
    Finished,
    /// The session cannot go on (interrupt never completed): drop it, keep the actor.
    Discard,
    Shutdown,
}

fn request_interrupt(
    interrupter: &Interrupter,
    interrupted: &mut Option<tokio::time::Instant>,
    grace: Duration,
) -> std::result::Result<(), HubError> {
    if interrupted.is_some() {
        return Ok(());
    }
    match interrupter.interrupt() {
        Ok(()) => {
            *interrupted = Some(tokio::time::Instant::now() + grace);
            Ok(())
        }
        Err(error) => {
            eprintln!("agent interrupt: {error:#}");
            Err(HubError::Closed)
        }
    }
}

async fn grace_elapsed(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_actor<A, F, Fut>(
    hub: Hub,
    agent: AgentId,
    yes: bool,
    factory: F,
    grace: Duration,
    state: Arc<Mutex<AgentState>>,
    mut rx: mpsc::UnboundedReceiver<Command>,
    mut stop: watch::Receiver<bool>,
    guard: Option<(std::path::PathBuf, bool)>,
    smart: bool,
) where
    A: Agent,
    F: Fn() -> Fut,
    Fut: Future<Output = Result<A>>,
{
    let mut session = None;
    let mut session_id = String::new();
    while let Some(command) = rx.recv().await {
        match command {
            Command::Prompt(prompt, reply) => {
                if session.is_none() {
                    session_id = Uuid::new_v4().to_string();
                }
                hub.publish(
                    agent.clone(),
                    &session_id,
                    Event::Message {
                        role: Role::User,
                        text: prompt.clone(),
                        delta: false,
                    },
                );
                let _ = reply.send(Ok(()));
                let mut interrupted = None;
                let result = async {
                    if session.is_none() {
                        session = Some(factory().await?);
                    }
                    let active = session.as_mut().expect("session was created");
                    active.send(&prompt).await?;
                    let interrupter = active.interrupter();
                    loop {
                        // `next_events` is not cancel-safe: once started it is only
                        // dropped together with the session (shutdown, stuck interrupt).
                        let events = {
                            let mut next = std::pin::pin!(active.next_events());
                            loop {
                                tokio::select! {
                                    biased;
                                    _ = stop.wait_for(|stopped| *stopped) => return Ok(Turn::Shutdown),
                                    command = rx.recv() => match command {
                                        Some(Command::Interrupt(reply)) => {
                                            let _ = reply.send(request_interrupt(&interrupter, &mut interrupted, grace));
                                        }
                                        Some(Command::Prompt(_, reply)) => {
                                            let _ = reply.send(Err(HubError::Busy));
                                        }
                                        Some(Command::Respond(_, _, reply)) => {
                                            let _ = reply.send(Err(HubError::NotFound));
                                        }
                                        Some(Command::Shutdown(reply)) => {
                                            let _ = reply.send(Ok(()));
                                            return Ok(Turn::Shutdown);
                                        }
                                        None => return Ok(Turn::Shutdown),
                                    },
                                    _ = grace_elapsed(interrupted) => {
                                        hub.publish(
                                            agent.clone(),
                                            &session_id,
                                            Event::Done { reason: "interrupted".into() },
                                        );
                                        return Ok(Turn::Discard);
                                    }
                                    events = &mut next => break events?,
                                }
                            }
                        };
                        for event in events {
                            if interrupted.is_some() && matches!(event, Event::Error { .. }) {
                                continue;
                            }
                            let mut event = match event {
                                Event::Done { .. } if interrupted.is_some() => Event::Done {
                                    reason: "interrupted".into(),
                                },
                                event => event,
                            };
                            if let (Some((cwd, true)), Event::PermissionRequest { tool, input, guard: note, .. }) = (&guard, &mut event) {
                                if matches!(tool.to_ascii_lowercase().as_str(), "bash" | "powershell" | "shell") {
                                    if let Some(command) = input.get("command").and_then(serde_json::Value::as_str) {
                                        *note = crate::guard::check(command, cwd).map(|violation| agentsflow_events::GuardNote { rule: violation.rule.into(), reason: violation.reason });
                                    }
                                }
                            }
                            let flagged = matches!(&event, Event::PermissionRequest { guard: Some(_), .. });
                            // Smart mode answers safe requests itself and hands guard-flagged ones to the owner.
                            let human = !yes || (smart && flagged);
                            let blocked = yes && !smart && flagged;
                            let deny_reason = if blocked {
                                if let Event::PermissionRequest { guard: Some(note), .. } = &event { Some(format!("blocked by agentsflow guard ({}): {}", note.rule, note.reason)) } else { None }
                            } else { None };
                            let done = matches!(event, Event::Done { .. });
                            let permission = match &event {
                                Event::PermissionRequest { id, .. } => Some(id.clone()),
                                _ => None,
                            };
                            if let Some(id) = &permission {
                                if human && interrupted.is_none() {
                                    state.lock().unwrap().pending.push(id.clone());
                                }
                            }
                            hub.publish(agent.clone(), &session_id, event);
                            if let Some(id) = permission {
                                let mut interrupt_reply = None;
                                let (allow, response) = if interrupted.is_some() || blocked {
                                    (false, None)
                                } else if !human {
                                    (true, None)
                                } else {
                                    loop {
                                        match rx.recv().await {
                                            Some(Command::Respond(request_id, allow, reply))
                                                if request_id == id =>
                                            {
                                                break (allow, Some(reply));
                                            }
                                            Some(Command::Respond(_, _, reply)) => {
                                                let _ = reply.send(Err(HubError::NotFound));
                                            }
                                            Some(Command::Prompt(_, reply)) => {
                                                let _ = reply.send(Err(HubError::Busy));
                                            }
                                            // The pending request is denied below, then
                                            // the interrupt is passed on to the agent.
                                            Some(Command::Interrupt(reply)) => {
                                                interrupt_reply = Some(reply);
                                                break (false, None);
                                            }
                                            Some(Command::Shutdown(reply)) => {
                                                let _ = reply.send(Ok(()));
                                                return Ok(Turn::Shutdown);
                                            }
                                            None => return Ok(Turn::Shutdown),
                                        }
                                    }
                                };
                                match active.respond_with_reason(&id, allow, deny_reason.as_deref()).await {
                                    Ok(()) => {
                                        state.lock().unwrap().pending.retain(|open| open != &id);
                                        hub.publish_resolution(
                                            agent.clone(),
                                            &session_id,
                                            Event::PermissionResolved { id, allow, blocked_by_guard: blocked },
                                            if blocked { ResolutionOrigin::Guard } else if interrupt_reply.is_some() || interrupted.is_some() { ResolutionOrigin::Interrupt } else if response.is_some() { ResolutionOrigin::Response } else { ResolutionOrigin::Automatic },
                                        );
                                        if let Some(reply) = response {
                                            let _ = reply.send(Ok(()));
                                        }
                                        if let Some(reply) = interrupt_reply {
                                            let _ = reply.send(request_interrupt(
                                                &interrupter,
                                                &mut interrupted,
                                                grace,
                                            ));
                                        }
                                    }
                                    Err(error) => {
                                        if let Some(reply) = response {
                                            let _ = reply.send(Err(HubError::Closed));
                                        }
                                        return Err(error);
                                    }
                                }
                            }
                            if done {
                                return Ok(Turn::Finished);
                            }
                        }
                    }
                }
                .await;
                state.lock().unwrap().pending.clear();
                match result {
                    Ok(Turn::Shutdown) => {
                        close(&mut session).await;
                        state.lock().unwrap().busy = false;
                        return;
                    }
                    Ok(Turn::Finished) => {}
                    Ok(Turn::Discard) => close(&mut session).await,
                    // A failure after an accepted interrupt is part of stopping the turn.
                    Err(error) if interrupted.is_some() => {
                        eprintln!("agent error after interrupt: {error:#}");
                        hub.publish(
                            agent.clone(),
                            &session_id,
                            Event::Done {
                                reason: "interrupted".into(),
                            },
                        );
                        close(&mut session).await;
                    }
                    Err(error) => {
                        eprintln!("agent error: {error:#}");
                        hub.publish(
                            agent.clone(),
                            &session_id,
                            Event::Error {
                                message: format!("{error:#}"),
                            },
                        );
                        hub.publish(
                            agent.clone(),
                            &session_id,
                            Event::Done {
                                reason: "error".into(),
                            },
                        );
                        close(&mut session).await;
                    }
                }
                state.lock().unwrap().busy = false;
            }
            Command::Respond(_, _, reply) => {
                let _ = reply.send(Err(HubError::NotFound));
            }
            Command::Interrupt(reply) => {
                let _ = reply.send(Err(HubError::NotBusy));
            }
            Command::Shutdown(reply) => {
                close(&mut session).await;
                let _ = reply.send(Ok(()));
                return;
            }
        }
    }
    close(&mut session).await;
}

#[cfg(test)]
mod tests {
    #[test]
    fn statistics_count_and_replay() {
        use super::*;
        let path = std::env::temp_dir().join(format!("agentsflow-stats-{}.jsonl", Uuid::new_v4()));
        let before;
        {
            let hub = Hub::with_log(&path);
            let agent = AgentId::from("claude");
            hub.publish(
                agent.clone(),
                "session",
                Event::Message {
                    role: Role::User,
                    text: "work".into(),
                    delta: false,
                },
            );
            hub.publish(
                agent.clone(),
                "session",
                Event::ToolCall {
                    id: "t".into(),
                    name: "shell".into(),
                    input: serde_json::json!({}),
                },
            );
            hub.publish(
                agent.clone(),
                "session",
                Event::PermissionRequest {
                    id: "p".into(),
                    tool: "shell".into(),
                    input: serde_json::json!({}),
                    guard: None,
                },
            );
            hub.publish(
                agent.clone(),
                "session",
                Event::PermissionResolved {
                    id: "p".into(),
                    allow: false,
                    blocked_by_guard: true,
                },
            );
            hub.send_mail(
                agent.clone(),
                AgentId::orchestrator(),
                "hi".into(),
                "body".into(),
                None,
            )
            .unwrap();
            hub.publish(
                agent,
                "session",
                Event::Done {
                    reason: "error".into(),
                },
            );
            before = serde_json::to_value(hub.stats()).unwrap();
            assert_eq!(before["agents"]["claude"]["turns"], 1);
            assert_eq!(before["agents"]["claude"]["turn_errors"], 1);
            assert_eq!(before["agents"]["claude"]["tool_calls"], 1);
            assert_eq!(before["agents"]["claude"]["permission_requests"], 1);
            assert_eq!(before["agents"]["claude"]["guard_denied"], 1);
            assert_eq!(before["agents"]["orchestrator"]["mails_received"], 1);
        }
        let hub = Hub::with_log(&path);
        let after = serde_json::to_value(hub.stats()).unwrap();
        assert_eq!(before["agents"], after["agents"]);
        assert_eq!(before["totals"], after["totals"]);
        drop(hub);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn tasks_validate_limit_and_replay() {
        use super::*;
        let path = std::env::temp_dir().join(format!("agentsflow-tasks-{}.jsonl", Uuid::new_v4()));
        let id;
        {
            let hub = Hub::with_log(&path);
            let new = || NewTask {
                title: " work ".into(),
                description: String::new(),
                assignee: Some(AgentId::from("claude")),
                status: None,
                labels: vec!["test".into()],
            };
            let task = hub.create_task(AgentId::orchestrator(), new()).unwrap();
            id = task.id.clone();
            assert_eq!(task.title, "work");
            assert_eq!(task.status, TaskStatus::Todo);
            for patch in [
                TaskPatch {
                    title: Some(" ".into()),
                    ..Default::default()
                },
                TaskPatch {
                    title: Some("x".repeat(201)),
                    ..Default::default()
                },
                TaskPatch {
                    description: Some("x".repeat(8001)),
                    ..Default::default()
                },
                TaskPatch {
                    note: Some("x".repeat(501)),
                    ..Default::default()
                },
                TaskPatch {
                    labels: Some(vec!["tag".into(); 9]),
                    ..Default::default()
                },
                TaskPatch {
                    labels: Some(vec!["x".repeat(25)]),
                    ..Default::default()
                },
            ] {
                assert!(
                    hub.update_task(AgentId::orchestrator(), &id, patch)
                        .is_err()
                );
            }
            let progress = hub
                .update_task(
                    AgentId::orchestrator(),
                    &id,
                    TaskPatch {
                        progress: Some(60),
                        status: Some(TaskStatus::Backlog),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(progress.progress, 60);
            assert!(
                hub.update_task(
                    AgentId::orchestrator(),
                    &id,
                    TaskPatch {
                        progress: Some(101),
                        ..Default::default()
                    }
                )
                .is_err()
            );
            assert!(
                hub.update_task(
                    AgentId::orchestrator(),
                    &id,
                    TaskPatch {
                        labels: Some(vec!["BAD".into()]),
                        ..Default::default()
                    }
                )
                .is_err()
            );
            let done = hub
                .update_task(
                    AgentId::orchestrator(),
                    &id,
                    TaskPatch {
                        status: Some(TaskStatus::Done),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(done.progress, 100);
            hub.update_task(
                AgentId::orchestrator(),
                &id,
                TaskPatch {
                    status: Some(TaskStatus::Archived),
                    ..Default::default()
                },
            )
            .unwrap();
            assert!(hub.tasks(false).is_empty());
            assert_eq!(hub.tasks(true).len(), 1);
            hub.update_task(
                AgentId::orchestrator(),
                &id,
                TaskPatch {
                    status: Some(TaskStatus::Done),
                    ..Default::default()
                },
            )
            .unwrap();
            for _ in 1..500 {
                hub.create_task(AgentId::orchestrator(), new()).unwrap();
            }
            assert_eq!(
                hub.create_task(AgentId::orchestrator(), new()),
                Err(TaskError::Limit)
            );
        }
        let hub = Hub::with_log(&path);
        assert_eq!(hub.task(&id).unwrap().status, TaskStatus::Done);
        assert_eq!(hub.tasks(false).len(), 500);
        drop(hub);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn mailbox_limits_and_replay() {
        use super::*;
        let path = std::env::temp_dir().join(format!("agentsflow-mail-{}.jsonl", Uuid::new_v4()));
        let (first, second);
        {
            let hub = Hub::with_log(&path);
            hub.set_mail_thread_limit(2);
            assert_eq!(
                hub.send_mail(
                    AgentId::from("claude"),
                    AgentId::from("claude"),
                    "s".into(),
                    "b".into(),
                    None
                ),
                Err(MailError::SameAgent)
            );
            assert_eq!(
                hub.send_mail(
                    AgentId::from("claude"),
                    AgentId::from("codex"),
                    "".into(),
                    "b".into(),
                    None
                ),
                Err(MailError::Subject)
            );
            assert_eq!(
                hub.send_mail(
                    AgentId::from("claude"),
                    AgentId::from("codex"),
                    "я".repeat(201),
                    "b".into(),
                    None
                ),
                Err(MailError::Subject)
            );
            assert_eq!(
                hub.send_mail(
                    AgentId::from("claude"),
                    AgentId::from("codex"),
                    "s".into(),
                    "я".repeat(32769),
                    None
                ),
                Err(MailError::Body)
            );
            assert_eq!(
                hub.send_mail(
                    AgentId::from("claude"),
                    AgentId::from("codex"),
                    "s".into(),
                    "".into(),
                    None
                ),
                Err(MailError::Body)
            );
            assert_eq!(
                hub.send_mail(
                    AgentId::from("claude"),
                    AgentId::from("codex"),
                    "s".into(),
                    "b".into(),
                    Some("missing".into())
                ),
                Err(MailError::UnknownThread)
            );
            first = hub
                .send_mail(
                    AgentId::from("claude"),
                    AgentId::from("codex"),
                    "s".into(),
                    "b".into(),
                    None,
                )
                .unwrap();
            second = hub
                .send_mail(
                    AgentId::from("claude"),
                    AgentId::from("codex"),
                    "s2".into(),
                    "b2".into(),
                    Some(first.thread.clone()),
                )
                .unwrap();
            assert!(first.seq < second.seq);
            assert_eq!(
                hub.send_mail(
                    AgentId::from("codex"),
                    AgentId::from("claude"),
                    "s".into(),
                    "b".into(),
                    Some(first.thread.clone())
                ),
                Err(MailError::ThreadLimit)
            );
            assert_eq!(hub.unread_count(AgentId::from("codex")), 2);
            assert_eq!(
                hub.take_inbox(AgentId::from("codex")),
                vec![first.clone(), second.clone()]
            );
            assert!(hub.take_inbox(AgentId::from("codex")).is_empty());
            hub.send_mail(
                AgentId::from("claude"),
                AgentId::from("codex"),
                "new".into(),
                "body".into(),
                None,
            )
            .unwrap();
        }
        {
            let hub = Hub::with_log(&path);
            let unread = hub.unread(AgentId::from("codex"));
            assert_eq!(unread.len(), 1);
            assert_eq!(unread[0].subject, "new");
            let (history, _) = hub.subscribe();
            assert!(
                history
                    .iter()
                    .any(|e| matches!(e.event, Event::MailRead { .. }))
            );
            assert!(
                history
                    .iter()
                    .any(|e| matches!(e.event, Event::Error { .. }))
            );
        }
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn log_restores_history_and_continues_sequence() {
        use super::*;
        let path = std::env::temp_dir().join(format!("agentsflow-{}.jsonl", Uuid::new_v4()));
        {
            let hub = Hub::with_log(&path);
            for _ in 0..5002 {
                hub.publish(
                    AgentId::from("claude"),
                    "test",
                    Event::Done {
                        reason: "ok".into(),
                    },
                );
            }
        }
        {
            let mut file = OpenOptions::new().append(true).open(&path).unwrap();
            writeln!(file, "invalid json").unwrap();
            writeln!(file, "another invalid line").unwrap();
        }
        {
            let hub = Hub::with_log(&path);
            let (history, _) = hub.subscribe();
            assert_eq!(history.len(), 5000);
            assert_eq!(history.first().unwrap().seq, 3);
            assert_eq!(history.last().unwrap().seq, 5002);
            hub.publish(
                AgentId::from("codex"),
                "test",
                Event::Done {
                    reason: "new".into(),
                },
            );
            assert_eq!(hub.subscribe().0.last().unwrap().seq, 5003);
        }
        let lines = std::fs::read_to_string(&path).unwrap();
        // 5002 + 2 invalid lines were compacted to 5000 at start, plus one new event.
        assert_eq!(lines.lines().count(), HISTORY_LIMIT + 1);
        let last: Envelope = serde_json::from_str(lines.lines().last().unwrap()).unwrap();
        assert_eq!(last.seq, 5003);
        assert!(matches!(last.event, Event::Done { ref reason } if reason == "new"));
        std::fs::remove_file(path).unwrap();
    }

    use super::*;

    #[test]
    fn log_closes_what_a_crash_left_open() {
        let path = std::env::temp_dir().join(format!("agentsflow-{}.jsonl", Uuid::new_v4()));
        {
            let hub = Hub::with_log(&path);
            let workflow = |status| Event::Workflow {
                name: "flow".into(),
                step: None,
                status,
                detail: String::new(),
            };
            hub.publish(
                AgentId::orchestrator(),
                "w",
                workflow(WorkflowStatus::Started),
            );
            hub.publish(AgentId::from("claude"), "c", permission());
            hub.publish(
                AgentId::from("codex"),
                "x",
                Event::Done {
                    reason: "ok".into(),
                },
            );
        }
        let hub = Hub::with_log(&path);
        let (history, _) = hub.subscribe();
        let tail: Vec<_> = history
            .iter()
            .skip(3)
            .map(|e| (e.agent.clone(), &e.event))
            .collect();
        assert_eq!(tail.len(), 3);
        assert_eq!(tail[0].0, AgentId::from("claude"));
        assert!(matches!(
            tail[0].1,
            Event::PermissionResolved { allow: false, .. }
        ));
        assert_eq!(tail[1].0, AgentId::from("claude"));
        assert!(matches!(tail[1].1, Event::Done { .. }));
        assert_eq!(tail[2].0, AgentId::orchestrator());
        assert!(matches!(
            tail[2].1,
            Event::Workflow {
                status: WorkflowStatus::Failed,
                ..
            }
        ));
        drop(hub);
        // A second restart finds everything closed and adds nothing.
        let hub = Hub::with_log(&path);
        assert_eq!(hub.subscribe().0.len(), 6);
        drop(hub);
        std::fs::remove_file(path).unwrap();
    }

    use std::sync::atomic::AtomicUsize;
    use tokio::task::LocalSet;

    struct Fake {
        steps: VecDeque<anyhow::Result<Vec<Event>>>,
        answers: Arc<Mutex<Vec<bool>>>,
    }
    type GuardAnswers = Arc<Mutex<Vec<(bool, Option<String>)>>>;
    struct GuardFake {
        event: Option<Event>,
        answers: GuardAnswers,
    }
    impl Agent for GuardFake {
        async fn send(&mut self, _: &str) -> Result<()> {
            Ok(())
        }
        async fn next_events(&mut self) -> Result<Vec<Event>> {
            Ok(vec![self.event.take().unwrap_or(Event::Done {
                reason: "ok".into(),
            })])
        }
        async fn respond(&mut self, _: &str, allow: bool) -> Result<()> {
            self.answers.lock().unwrap().push((allow, None));
            Ok(())
        }
        async fn respond_with_reason(
            &mut self,
            _: &str,
            allow: bool,
            reason: Option<&str>,
        ) -> Result<()> {
            self.answers
                .lock()
                .unwrap()
                .push((allow, reason.map(str::to_owned)));
            Ok(())
        }
        async fn shutdown(self) -> Result<()> {
            Ok(())
        }
    }
    #[tokio::test]
    async fn guard_checks_permissions_and_auto_denies() {
        LocalSet::new().run_until(async {
            for (yes, smart, enabled, command, warned, expected) in [
                (true, false, true, Some("rm -rf /"), true, false),
                (true, false, true, Some("cargo build"), false, true),
                (false, false, true, Some("rm -rf /"), true, true),
                (true, false, false, Some("rm -rf /"), false, true),
                (true, false, true, None, false, true),
                // smart: safe commands pass by themselves, flagged ones wait for the owner
                (true, true, true, Some("cargo build"), false, true),
                (true, true, true, Some("rm -rf /"), true, true),
                (true, true, false, Some("rm -rf /"), false, true),
            ] {
                let hub = Hub::new();
                let (_, mut rx) = hub.subscribe();
                let answers = Arc::new(Mutex::new(Vec::new()));
                let (actor, task) = spawn_guarded(hub, AgentId::from("agent"), yes, std::env::current_dir().unwrap(), enabled, smart, {
                    let answers = answers.clone();
                    move || {
                        let answers = answers.clone();
                        async move { Ok(GuardFake { answers, event: Some(Event::PermissionRequest {
                            id: "p".into(), tool: "Bash".into(),
                            input: command.map_or_else(|| serde_json::json!({}), |c| serde_json::json!({"command":c})), guard: None,
                        }) }) }
                    }
                });
                actor.prompt("test".into()).await.unwrap();
                let request = wait_event(&mut rx, |event| matches!(event, Event::PermissionRequest { .. })).await;
                assert_eq!(matches!(request, Event::PermissionRequest { guard: Some(_), .. }), warned);
                let asks = !yes || (smart && warned);
                if asks { assert!(answers.lock().unwrap().is_empty()); assert_eq!(actor.state().pending, ["p"]); actor.respond("p".into(), true).await.unwrap(); }
                let resolved = wait_event(&mut rx, |event| matches!(event, Event::PermissionResolved { .. })).await;
                assert!(matches!(resolved, Event::PermissionResolved { allow, blocked_by_guard, .. } if allow == expected && blocked_by_guard == (yes && !smart && warned)));
                let recorded = answers.lock().unwrap().clone();
                assert_eq!(recorded[0].0, expected);
                if !expected { assert!(recorded[0].1.as_deref().unwrap().contains("blocked by agentsflow guard (rm-recursive)")); }
                actor.shutdown().await;
                task.await.unwrap();
            }
        }).await;
    }
    impl Agent for Fake {
        async fn send(&mut self, _: &str) -> Result<()> {
            Ok(())
        }
        async fn next_events(&mut self) -> Result<Vec<Event>> {
            self.steps.pop_front().unwrap_or_else(|| {
                Ok(vec![Event::Done {
                    reason: "ok".into(),
                }])
            })
        }
        async fn respond(&mut self, _: &str, allow: bool) -> Result<()> {
            self.answers.lock().unwrap().push(allow);
            Ok(())
        }
        async fn shutdown(self) -> Result<()> {
            Ok(())
        }
    }
    fn permission() -> Event {
        Event::PermissionRequest {
            id: "p".into(),
            tool: "shell".into(),
            input: serde_json::json!({}),
            guard: None,
        }
    }
    async fn wait_event(
        rx: &mut broadcast::Receiver<Envelope>,
        predicate: impl Fn(&Event) -> bool,
    ) -> Event {
        loop {
            let envelope = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
            if predicate(&envelope.event) {
                return envelope.event;
            }
        }
    }
    #[test]
    fn snapshot_and_live_order() {
        let hub = Hub::new();
        hub.publish(
            AgentId::from("claude"),
            "a",
            Event::Done {
                reason: "first".into(),
            },
        );
        let (snapshot, mut rx) = hub.subscribe();
        hub.publish(
            AgentId::from("codex"),
            "b",
            Event::Done {
                reason: "second".into(),
            },
        );
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].seq, 1);
        assert_eq!(rx.try_recv().unwrap().seq, 2);
        for _ in 0..5000 {
            hub.publish(
                AgentId::from("claude"),
                "a",
                Event::Done { reason: "x".into() },
            );
        }
        let (history, _) = hub.subscribe();
        assert_eq!(history.len(), 5000);
        assert_eq!(history[0].seq, 3);
        assert_eq!(history.last().unwrap().seq, 5002);
    }
    struct Hangs(Arc<AtomicUsize>);
    impl Agent for Hangs {
        async fn send(&mut self, _: &str) -> Result<()> {
            Ok(())
        }
        async fn next_events(&mut self) -> Result<Vec<Event>> {
            std::future::pending().await
        }
        async fn respond(&mut self, _: &str, _: bool) -> Result<()> {
            Ok(())
        }
        async fn shutdown(self) -> Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    #[tokio::test]
    async fn shutdown_interrupts_a_silent_turn() {
        LocalSet::new()
            .run_until(async {
                let closed = Arc::new(AtomicUsize::new(0));
                let copy = closed.clone();
                let (actor, task) = spawn(Hub::new(), AgentId::from("claude"), false, move || {
                    let copy = copy.clone();
                    async move { Ok(Hangs(copy)) }
                });
                actor.prompt("hi".into()).await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                tokio::time::timeout(std::time::Duration::from_secs(2), actor.shutdown())
                    .await
                    .expect("shutdown must not hang");
                task.await.unwrap();
                assert_eq!(closed.load(Ordering::SeqCst), 1);
            })
            .await;
    }
    #[tokio::test]
    async fn permission_busy_and_auto() {
        LocalSet::new()
            .run_until(async {
                for (yes, allow) in [(false, true), (false, false), (true, true)] {
                    let hub = Hub::new();
                    let (_, mut rx) = hub.subscribe();
                    let answers = Arc::new(Mutex::new(Vec::new()));
                    let copy = answers.clone();
                    let (actor, task) = spawn(hub, AgentId::from("claude"), yes, move || {
                        let answers = copy.clone();
                        async move {
                            Ok(Fake {
                                steps: VecDeque::from([
                                    Ok(vec![permission()]),
                                    Ok(vec![Event::Done {
                                        reason: "ok".into(),
                                    }]),
                                ]),
                                answers,
                            })
                        }
                    });
                    actor.prompt("hello".into()).await.unwrap();
                    wait_event(&mut rx, |event| {
                        matches!(event, Event::PermissionRequest { .. })
                    })
                    .await;
                    if !yes {
                        assert_eq!(actor.state().pending, vec!["p"]);
                        assert_eq!(actor.prompt("busy".into()).await, Err(HubError::Busy));
                        assert_eq!(
                            actor.respond("wrong".into(), true).await,
                            Err(HubError::NotFound)
                        );
                        actor.respond("p".into(), allow).await.unwrap();
                    }
                    assert_eq!(
                        wait_event(&mut rx, |event| matches!(
                            event,
                            Event::PermissionResolved { .. }
                        ))
                        .await,
                        Event::PermissionResolved {
                            id: "p".into(),
                            allow,
                            blocked_by_guard: false,
                        }
                    );
                    wait_event(&mut rx, |event| matches!(event, Event::Done { .. })).await;
                    assert_eq!(*answers.lock().unwrap(), vec![allow]);
                    actor.shutdown().await;
                    task.await.unwrap();
                }
            })
            .await;
    }
    #[tokio::test]
    async fn error_resets_session() {
        LocalSet::new()
            .run_until(async {
                let hub = Hub::new();
                let (_, mut rx) = hub.subscribe();
                let starts = Arc::new(AtomicUsize::new(0));
                let count = starts.clone();
                let (actor, task) = spawn(hub, AgentId::from("codex"), false, move || {
                    let first = count.fetch_add(1, Ordering::SeqCst) == 0;
                    async move {
                        Ok(Fake {
                            steps: VecDeque::from([if first {
                                Err(anyhow::anyhow!("failed"))
                            } else {
                                Ok(vec![Event::Done {
                                    reason: "ok".into(),
                                }])
                            }]),
                            answers: Arc::new(Mutex::new(Vec::new())),
                        })
                    }
                });
                actor.prompt("one".into()).await.unwrap();
                wait_event(&mut rx, |e| matches!(e, Event::Error { .. })).await;
                wait_event(
                    &mut rx,
                    |e| matches!(e, Event::Done {reason} if reason == "error"),
                )
                .await;
                while actor.state().busy {
                    tokio::task::yield_now().await;
                }
                actor.prompt("two".into()).await.unwrap();
                wait_event(
                    &mut rx,
                    |e| matches!(e, Event::Done {reason} if reason == "ok"),
                )
                .await;
                assert_eq!(starts.load(Ordering::SeqCst), 2);
                actor.shutdown().await;
                task.await.unwrap();
            })
            .await;
    }

    async fn wait_idle(actor: &Actor) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while actor.state().busy {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("agent must become idle");
    }
    fn is_done(reason: &'static str) -> impl Fn(&Event) -> bool {
        move |event| matches!(event, Event::Done { reason: found } if found == reason)
    }
    #[tokio::test]
    async fn interrupt_ends_a_silent_turn_and_session_stays_usable() {
        use crate::testing::Silent;
        LocalSet::new()
            .run_until(async {
                let hub = Hub::new();
                let (_, mut rx) = hub.subscribe();
                let starts = Arc::new(AtomicUsize::new(0));
                let interrupts = Arc::new(AtomicUsize::new(0));
                let (count, seen) = (starts.clone(), interrupts.clone());
                let (actor, task) = spawn(hub, AgentId::from("claude"), false, move || {
                    count.fetch_add(1, Ordering::SeqCst);
                    let seen = seen.clone();
                    async move { Ok(Silent::new(1, false).probes(Default::default(), seen)) }
                });
                assert_eq!(actor.interrupt().await, Err(HubError::NotBusy));
                actor.prompt("one".into()).await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                assert!(actor.state().busy);
                actor.interrupt().await.unwrap();
                wait_event(&mut rx, is_done("interrupted")).await;
                wait_idle(&actor).await;
                assert_eq!(interrupts.load(Ordering::SeqCst), 1);
                assert_eq!(actor.interrupt().await, Err(HubError::NotBusy));
                actor.prompt("two".into()).await.unwrap();
                wait_event(&mut rx, is_done("ok")).await;
                wait_idle(&actor).await;
                assert_eq!(starts.load(Ordering::SeqCst), 1);
                actor.shutdown().await;
                task.await.unwrap();
            })
            .await;
    }
    #[tokio::test]
    async fn interrupt_denies_a_pending_permission() {
        use crate::testing::Silent;
        LocalSet::new()
            .run_until(async {
                let hub = Hub::new();
                let (_, mut rx) = hub.subscribe();
                let answers = Arc::new(Mutex::new(Vec::new()));
                let copy = answers.clone();
                let (actor, task) = spawn(hub, AgentId::from("claude"), false, move || {
                    let answers = copy.clone();
                    async move { Ok(Silent::new(1, true).probes(answers, Default::default())) }
                });
                actor.prompt("one".into()).await.unwrap();
                wait_event(&mut rx, |e| matches!(e, Event::PermissionRequest { .. })).await;
                assert_eq!(actor.state().pending, vec!["p"]);
                actor.interrupt().await.unwrap();
                assert_eq!(
                    wait_event(&mut rx, |e| matches!(e, Event::PermissionResolved { .. })).await,
                    Event::PermissionResolved {
                        id: "p".into(),
                        allow: false,
                        blocked_by_guard: false,
                    }
                );
                wait_event(&mut rx, is_done("interrupted")).await;
                wait_idle(&actor).await;
                assert_eq!(*answers.lock().unwrap(), vec![false]);
                assert!(actor.state().pending.is_empty());
                actor.shutdown().await;
                task.await.unwrap();
            })
            .await;
    }
    #[tokio::test]
    async fn interrupt_with_auto_approval_ends_the_turn() {
        use crate::testing::Silent;
        LocalSet::new()
            .run_until(async {
                let hub = Hub::new();
                let (_, mut rx) = hub.subscribe();
                let answers = Arc::new(Mutex::new(Vec::new()));
                let copy = answers.clone();
                // With --yes the request is allowed at once; the interrupt then
                // ends the silent remainder of the turn.
                let (actor, task) = spawn(hub, AgentId::from("claude"), true, move || {
                    let answers = copy.clone();
                    async move { Ok(Silent::new(1, true).probes(answers, Default::default())) }
                });
                actor.prompt("one".into()).await.unwrap();
                wait_event(&mut rx, |e| matches!(e, Event::PermissionResolved { .. })).await;
                actor.interrupt().await.unwrap();
                wait_event(&mut rx, is_done("interrupted")).await;
                wait_idle(&actor).await;
                assert_eq!(*answers.lock().unwrap(), vec![true]);
                actor.shutdown().await;
                task.await.unwrap();
            })
            .await;
    }
    struct Stuck(Arc<AtomicUsize>);
    impl Agent for Stuck {
        async fn send(&mut self, _: &str) -> Result<()> {
            Ok(())
        }
        async fn next_events(&mut self) -> Result<Vec<Event>> {
            std::future::pending().await
        }
        async fn respond(&mut self, _: &str, _: bool) -> Result<()> {
            Ok(())
        }
        fn interrupter(&self) -> agentsflow_adapters::Interrupter {
            agentsflow_adapters::Interrupter::new(|| Ok(()))
        }
        async fn shutdown(self) -> Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    #[tokio::test]
    async fn interrupt_that_never_ends_drops_the_session() {
        LocalSet::new()
            .run_until(async {
                let hub = Hub::new();
                let (_, mut rx) = hub.subscribe();
                let closed = Arc::new(AtomicUsize::new(0));
                let copy = closed.clone();
                let (actor, task) = spawn_with(
                    hub,
                    AgentId::from("claude"),
                    false,
                    move || {
                        let copy = copy.clone();
                        async move { Ok(Stuck(copy)) }
                    },
                    Duration::from_millis(100),
                );
                actor.prompt("one".into()).await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                actor.interrupt().await.unwrap();
                wait_event(&mut rx, is_done("interrupted")).await;
                wait_idle(&actor).await;
                assert_eq!(closed.load(Ordering::SeqCst), 1);
                actor.prompt("two".into()).await.unwrap();
                actor.shutdown().await;
                task.await.unwrap();
            })
            .await;
    }
    #[tokio::test]
    async fn interrupt_without_support_reports_closed() {
        LocalSet::new()
            .run_until(async {
                let closed = Arc::new(AtomicUsize::new(0));
                let copy = closed.clone();
                let (actor, task) = spawn(Hub::new(), AgentId::from("claude"), false, move || {
                    let copy = copy.clone();
                    async move { Ok(Hangs(copy)) }
                });
                actor.prompt("one".into()).await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                assert_eq!(actor.interrupt().await, Err(HubError::Closed));
                assert!(actor.state().busy);
                actor.shutdown().await;
                task.await.unwrap();
            })
            .await;
    }
    struct Dies {
        signal: mpsc::UnboundedSender<()>,
        interrupted: mpsc::UnboundedReceiver<()>,
    }
    impl Agent for Dies {
        async fn send(&mut self, _: &str) -> Result<()> {
            Ok(())
        }
        async fn next_events(&mut self) -> Result<Vec<Event>> {
            self.interrupted.recv().await;
            Err(anyhow::anyhow!("connection closed"))
        }
        async fn respond(&mut self, _: &str, _: bool) -> Result<()> {
            Ok(())
        }
        fn interrupter(&self) -> agentsflow_adapters::Interrupter {
            let signal = self.signal.clone();
            agentsflow_adapters::Interrupter::new(move || {
                signal.send(()).map_err(|_| anyhow::anyhow!("closed"))
            })
        }
        async fn shutdown(self) -> Result<()> {
            Ok(())
        }
    }
    #[tokio::test]
    async fn failure_after_interrupt_still_ends_interrupted() {
        LocalSet::new()
            .run_until(async {
                let hub = Hub::new();
                let (_, mut rx) = hub.subscribe();
                let (actor, task) = spawn(hub, AgentId::from("claude"), false, || async {
                    let (signal, interrupted) = mpsc::unbounded_channel();
                    Ok(Dies {
                        signal,
                        interrupted,
                    })
                });
                actor.prompt("one".into()).await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                actor.interrupt().await.unwrap();
                wait_event(&mut rx, is_done("interrupted")).await;
                wait_idle(&actor).await;
                actor.prompt("two".into()).await.unwrap();
                actor.shutdown().await;
                task.await.unwrap();
            })
            .await;
    }
}
