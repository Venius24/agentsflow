//! The orchestrator: a fresh one-turn session per owner message or internal
//! result. It answers with a JSON plan; the gateway validates every action
//! and applies it through [`Runs`], so the model never touches the board
//! directly.

use crate::{
    home::Home,
    ledger,
    runs::{NewWork, RunEnv, Runs},
    team::{AccessMode, Member},
    workflow::{self, Spec},
};
use agentsflow_events::{
    AgentId, DepartmentProposal, Event, Question, Risk, TaskSnapshot, WorkState,
};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{broadcast::error::RecvError, mpsc, watch};

/// Sender of the orchestrator's messages in the owner's mailbox.
pub const FROM: &str = "оркестратор";
const AGENT_PREFIX: &str = "orchestrator:plan:";
const TIMEOUT: Duration = Duration::from_secs(600);
const RESULT_LIMIT: usize = 6000;

#[derive(Clone)]
pub struct Orchestrator {
    tx: mpsc::UnboundedSender<Trigger>,
}

#[derive(Clone, Debug, PartialEq)]
enum Trigger {
    Owner(String),
    Review(String),
    /// A department run ordered work from another department.
    Request {
        parent: String,
        text: String,
    },
}

impl Orchestrator {
    pub fn start(runs: Runs, env: RunEnv) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let engine = Engine {
            runs,
            env,
            queue: VecDeque::new(),
            seen: HashSet::new(),
            calls: 0,
        };
        tokio::task::spawn_local(engine.run(rx));
        Self { tx }
    }

    /// A free-form message of the owner: tasks, answers, anything.
    /// An order of a department run, filed as a task of another department.
    pub fn request(&self, parent: &str, text: &str) -> Result<(), String> {
        self.tx
            .send(Trigger::Request {
                parent: parent.to_owned(),
                text: text.to_owned(),
            })
            .map_err(|_| "оркестратор остановлен".to_owned())
    }

    pub fn message(&self, text: &str) -> Result<(), String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("пустое сообщение".into());
        }
        self.tx
            .send(Trigger::Owner(text.to_owned()))
            .map_err(|_| "оркестратор остановлен".to_owned())
    }
}

struct Engine {
    runs: Runs,
    env: RunEnv,
    queue: VecDeque<Trigger>,
    /// Results already handed to acceptance: (task, run count).
    seen: HashSet<(String, usize)>,
    calls: u64,
}

impl Engine {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Trigger>) {
        let (_, mut events) = self.runs.hub().subscribe();
        for task in self.runs.hub().tasks(false) {
            self.observe(&task);
        }
        loop {
            while let Some(trigger) = self.next_trigger() {
                self.handle(trigger, &mut events).await;
            }
            tokio::select! {
                trigger = rx.recv() => match trigger {
                    Some(trigger) => self.queue.push_back(trigger),
                    None => break,
                },
                event = events.recv() => match event {
                    Ok(envelope) => self.on_event(envelope.agent, envelope.event),
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => break,
                },
            }
            while let Ok(trigger) = rx.try_recv() {
                self.queue.push_back(trigger);
            }
        }
    }

    /// Consecutive owner messages go to one session.
    fn next_trigger(&mut self) -> Option<Trigger> {
        let first = self.queue.pop_front()?;
        let Trigger::Owner(mut text) = first else {
            return Some(first);
        };
        while let Some(Trigger::Owner(more)) = self.queue.front() {
            text.push_str("\n\n");
            text.push_str(more);
            self.queue.pop_front();
        }
        Some(Trigger::Owner(text))
    }

    fn on_event(&mut self, agent: AgentId, event: Event) {
        match event {
            Event::Task { task } => self.observe(&task),
            Event::Usage {
                input_tokens,
                output_tokens,
                cost_usd,
            } if agent.as_str().starts_with(AGENT_PREFIX) => {
                self.account(&agent, input_tokens, output_tokens, cost_usd);
            }
            _ => {}
        }
    }

    /// Internal results go to acceptance; with the orchestrator off they go
    /// straight to the owner.
    fn observe(&mut self, task: &TaskSnapshot) {
        let Some(work) = &task.work else { return };
        if work.state != WorkState::Review || work.risk != Risk::Internal || work.owner_review {
            return;
        }
        if !self.seen.insert((task.id.clone(), work.runs.len())) {
            return;
        }
        if self.runs.home().config.orchestrator.enabled {
            self.queue.push_back(Trigger::Review(task.id.clone()));
        } else {
            let _ = self.runs.to_owner(AgentId::from(FROM), &task.id);
        }
    }

    fn account(&self, agent: &AgentId, input: u64, output: u64, reported: Option<f64>) {
        let home = self.runs.home();
        let config = &home.config.orchestrator;
        let (usd, source) = ledger::cost(
            config.kind,
            config.model.as_deref(),
            input,
            output,
            reported,
            &home.config.prices,
        );
        let ts_ms = now_ms();
        self.runs.ledger().record(ledger::Entry {
            ts_ms,
            month: ledger::month_of(ts_ms),
            department: "orchestrator".into(),
            task: String::new(),
            run: agent.as_str().trim_start_matches(AGENT_PREFIX).into(),
            agent: agent.to_string(),
            model: config.model.clone(),
            input_tokens: input,
            output_tokens: output,
            usd,
            source: source.into(),
        });
    }

    async fn handle(
        &mut self,
        trigger: Trigger,
        events: &mut tokio::sync::broadcast::Receiver<agentsflow_events::Envelope>,
    ) {
        let home = self.runs.home();
        if !home.config.orchestrator.enabled {
            if let Trigger::Owner(_) = trigger {
                self.tell(
                    "Оркестратор выключен",
                    "Сообщение не разобрано: в agentsflow.toml [orchestrator] enabled = false. Создайте задачу вручную.".into(),
                );
            }
            if let Trigger::Request { parent, .. } = &trigger {
                fail_request(
                    &self.runs,
                    parent,
                    "оркестратор выключен, заказ не оформлен",
                );
            }
            return;
        }
        if let Trigger::Review(id) = &trigger {
            // The owner may have acted while it waited in the queue.
            let still = self
                .runs
                .hub()
                .task(id)
                .and_then(|task| task.work)
                .is_some_and(|work| work.state == WorkState::Review && !work.owner_review);
            if !still {
                return;
            }
        }
        let prompt = prompt(&home, &self.runs.hub().tasks(false), &trigger);
        self.calls += 1;
        let reply = self.call(&home, prompt).await;
        // Usage events of the session arrive before it finishes; drain them.
        while let Ok(envelope) = events.try_recv() {
            self.on_event(envelope.agent, envelope.event);
        }
        let outcome = reply.and_then(|text| parse_plan(&text));
        match (outcome, &trigger) {
            (Ok(plan), _) => {
                let report = apply(&self.runs, &trigger, plan.actions);
                let mut body = plan.message.trim().to_owned();
                if !report.is_empty() {
                    if !body.is_empty() {
                        body.push_str("\n\n");
                    }
                    body.push_str(&report.join("\n"));
                }
                if !body.is_empty() {
                    let subject = match &trigger {
                        Trigger::Owner(_) => "Разобрал сообщение",
                        Trigger::Review(_) => "Приёмка",
                        Trigger::Request { .. } => "Заказ отдела",
                    };
                    self.tell(subject, body);
                }
            }
            (Err(error), Trigger::Owner(text)) => self.tell(
                "Не смог разобрать сообщение",
                format!(
                    "{error}\n\nСообщение: {}\nПовторите его или создайте задачу вручную.",
                    short(text, 300)
                ),
            ),
            (Err(error), Trigger::Request { parent, .. }) => {
                fail_request(&self.runs, parent, &format!("заказ не оформлен: {error}"));
            }
            (Err(error), Trigger::Review(id)) => {
                let _ = self.runs.to_owner(AgentId::from(FROM), id);
                self.tell(
                    "Приёмка не удалась",
                    format!("{error}\n\nРезультат задачи {id} ждёт вашей приёмки."),
                );
            }
        }
    }

    async fn call(&self, home: &Home, prompt: String) -> Result<String, String> {
        let config = &home.config.orchestrator;
        let id = AgentId::from(format!("{AGENT_PREFIX}{}", self.calls).as_str());
        let member = Member {
            id: id.clone(),
            kind: config.kind,
            model: config.model.clone(),
            effort: config.effort.clone(),
            mode: Some(AccessMode::Smart),
            sandbox: None,
            guard: true,
            title: "оркестратор".into(),
            roles: vec!["lead".into()],
            lead: true,
        };
        let mut spec = Spec::parse(
            "name = \"orchestrator\"\n[[step]]\nid = \"plan\"\nagent = \"lead\"\nprompt = \"{brief}\"\n",
        )
        .map_err(|error| format!("{error:#}"))?;
        spec.step[0].agent = id.to_string();
        spec.bind("brief", prompt);
        let dir = home.orchestrator_dir();
        let _ = std::fs::create_dir_all(&dir);
        let (actor, handle) = self.env.spawn(self.runs.hub(), &member, &dir);
        let mut agents = crate::runtime::Agents::new();
        agents.insert(id.clone(), actor);
        let (stop_tx, stop_rx) = watch::channel(false);
        let timer = tokio::task::spawn_local(async move {
            tokio::time::sleep(TIMEOUT).await;
            let _ = stop_tx.send(true);
        });
        let (result, history) = workflow::execute_traced(
            self.runs.hub(),
            &agents,
            &spec,
            "разбор",
            id.as_str(),
            stop_rx,
        )
        .await;
        timer.abort();
        for actor in agents.values() {
            actor.shutdown().await;
        }
        let _ = handle.await;
        result.map_err(|error| format!("сессия оркестратора: {error}"))?;
        history
            .into_iter()
            .last()
            .map(|record| record.output)
            .ok_or_else(|| "оркестратор ничего не ответил".into())
    }

    fn tell(&self, subject: &str, body: String) {
        if let Err(error) = self.runs.hub().send_mail(
            AgentId::from(FROM),
            AgentId::orchestrator(),
            subject.into(),
            body,
            None,
        ) {
            crate::server_diag!("orchestrator mail failed: {error}");
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

fn short(text: &str, limit: usize) -> String {
    let mut out: String = text.chars().take(limit).collect();
    if text.chars().count() > limit {
        out.push('…');
    }
    out
}

fn risk_name(risk: Risk) -> &'static str {
    match risk {
        Risk::Internal => "internal",
        Risk::External => "external",
    }
}

fn state_name(state: WorkState) -> &'static str {
    match state {
        WorkState::Queued => "в очереди",
        WorkState::Waiting => "ждёт хозяина",
        WorkState::Running => "в работе",
        WorkState::Stuck => "застряла",
        WorkState::Review => "на приёмке",
        WorkState::Done => "готово",
        WorkState::Cancelled => "отменена",
    }
}

const FORMAT: &str = r#"# Как ответить

Инструменты не нужны: ответь одним блоком ```json``` и больше ничем.

{"message": "коротко хозяину, по-русски", "actions": [ ... ]}

Действия (поле "type"):
- {"type":"create_task","key":"a","title":"...","text":"полная постановка: что сделать, откуда данные, какой результат, твои предположения","department":"id отдела","risk":"internal|external","depends_on":["ключ или id задачи"],"questions":["вопрос хозяину"],"budget_usd":2.0}
  key — метка для depends_on внутри этого ответа; questions — только если без ответа будет угадывание (задача подождёт ответа); risk, depends_on, questions, budget_usd необязательны.
- {"type":"propose_department","id":"латиница-через-дефис","title":"...","description":"за что отвечает","rules":"правила отдела в markdown","default_risk":"internal|external","tasks":[ ...create_task без department... ]}
  Новый отдел создаётся только после «да» хозяина; его задачи подождут.
- {"type":"ask_owner","task":"id","questions":["..."]} — вопросы по уже существующей задаче.
- {"type":"accept","task":"id","note":"..."} — принять internal-результат.
- {"type":"return_task","task":"id","reason":"что конкретно исправить"} — вернуть отделу (один раз).
- {"type":"send_to_owner","task":"id","summary":"..."} — отдать результат хозяину на приёмку.
- {"type":"propose_rule","department":"id отдела или orchestrator","text":"одно правило коротко"} — постоянное правило; добавится только после «да» хозяина. Предлагай, когда хозяин просит запомнить или повторяет одно и то же указание.

Если делать нечего, верни пустой "actions" и ответь хозяину в "message"."#;

/// The whole input of one orchestrator session.
fn prompt(home: &Home, tasks: &[TaskSnapshot], trigger: &Trigger) -> String {
    let mut out = String::new();
    let rules = home.orchestrator_rules();
    out.push_str(rules.trim());
    out.push_str("\n\n# Отделы\n\n");
    for dept in home.departments.values() {
        out.push_str(&format!(
            "- `{}` — {}: {} (риск по умолчанию {})\n",
            dept.id,
            dept.title,
            dept.description,
            risk_name(dept.default_risk)
        ));
    }
    let open: Vec<_> = tasks
        .iter()
        .filter_map(|task| task.work.as_ref().map(|work| (task, work)))
        .filter(|(_, work)| !matches!(work.state, WorkState::Done | WorkState::Cancelled))
        .collect();
    if !open.is_empty() {
        out.push_str("\n# Открытые задачи\n\n");
        for (task, work) in open {
            out.push_str(&format!(
                "- `{}` [{}, {}, {}] {}\n",
                task.id,
                work.department,
                state_name(work.state),
                risk_name(work.risk),
                task.title
            ));
            for question in work.questions.iter().filter(|q| q.answer.is_none()) {
                out.push_str(&format!("  - открытый вопрос: {}\n", question.text));
            }
        }
    }
    match trigger {
        Trigger::Owner(text) => {
            out.push_str("\n# Сообщение хозяина\n\n");
            out.push_str(text.trim());
            out.push_str(
                "\n\nРазложи его на задачи по отделам. Если это ответ на открытый вопрос — \
                 вопросы задаются карточками, ответ хозяин даёт кнопкой; просто скажи ему об этом.\n",
            );
        }
        Trigger::Review(id) => {
            out.push_str("\n# Результат на приёмку\n\n");
            if let Some(task) = tasks.iter().find(|task| &task.id == id)
                && let Some(work) = &task.work
            {
                out.push_str(&format!(
                    "Задача `{}` «{}», отдел `{}`, риск {}.\n\n## Постановка\n\n{}\n",
                    task.id,
                    task.title,
                    work.department,
                    risk_name(work.risk),
                    task.description.trim()
                ));
                if !work.review_notes.is_empty() {
                    out.push_str("\n## Прошлые замечания\n\n");
                    for note in &work.review_notes {
                        out.push_str(&format!("- {note}\n"));
                    }
                }
                let result = work
                    .result_path
                    .as_ref()
                    .and_then(|path| std::fs::read_to_string(home.root.join(path)).ok())
                    .unwrap_or_else(|| work.result_summary.clone());
                out.push_str("\n## Результат\n\n");
                out.push_str(&short(result.trim(), RESULT_LIMIT));
                out.push_str(&format!(
                    "\n\nПрими (accept), верни (return_task) или отдай хозяину (send_to_owner) задачу `{}`.\n",
                    task.id
                ));
            }
        }
        Trigger::Request { parent, text } => {
            out.push_str("\n# Заказ отдела\n\n");
            if let Some(task) = tasks.iter().find(|task| &task.id == parent)
                && let Some(work) = &task.work
            {
                out.push_str(&format!(
                    "Отдел `{}` по задаче `{}` «{}» просит работу другого отдела:\n\n{}\n\n",
                    work.department,
                    task.id,
                    task.title,
                    text.trim()
                ));
            }
            out.push_str(
                "Оформи заказ одной задачей create_task (можно с вопросами хозяину, если без них никак) \
                 в подходящий отдел, но не в отдел, который заказывает, и не выше по цепочке. \
                 Задача заказчика продолжится, когда заказ будет готов.\n",
            );
        }
    }
    out.push('\n');
    out.push_str(FORMAT);
    out
}

/// The order could not be filed: the ordering task stops and says why.
fn fail_request(runs: &Runs, parent: &str, reason: &str) {
    let _ = runs
        .hub()
        .update_work(AgentId::from(FROM), parent, |_, work| {
            if work.state == WorkState::Waiting {
                work.state = WorkState::Stuck;
                work.stuck_reason = Some(reason.to_owned());
            }
            Ok(())
        });
}

#[derive(Debug, Deserialize)]
struct Plan {
    #[serde(default)]
    message: String,
    #[serde(default)]
    actions: Vec<Action>,
}

#[derive(Debug, Deserialize)]
struct NewTask {
    #[serde(default)]
    key: Option<String>,
    title: String,
    text: String,
    #[serde(default)]
    department: String,
    #[serde(default)]
    risk: Option<Risk>,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    questions: Vec<String>,
    #[serde(default)]
    budget_usd: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Action {
    CreateTask(NewTask),
    ProposeDepartment {
        id: String,
        title: String,
        description: String,
        #[serde(default)]
        rules: String,
        #[serde(default)]
        default_risk: Risk,
        #[serde(default)]
        tasks: Vec<NewTask>,
    },
    AskOwner {
        task: String,
        questions: Vec<String>,
    },
    Accept {
        task: String,
        #[serde(default)]
        note: String,
    },
    ReturnTask {
        task: String,
        reason: String,
    },
    SendToOwner {
        task: String,
        #[serde(default)]
        summary: String,
    },
    ProposeRule {
        department: String,
        text: String,
    },
}

/// The JSON of the reply: the last fenced block, or the outermost braces.
fn parse_plan(text: &str) -> Result<Plan, String> {
    let mut candidates = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("```") {
        let after = &rest[start + 3..];
        let body_start = after.find('\n').map_or(0, |n| n + 1);
        let Some(end) = after[body_start..].find("```") else {
            break;
        };
        candidates.push(after[body_start..body_start + end].trim());
        rest = &after[body_start + end + 3..];
    }
    candidates.reverse();
    if let (Some(start), Some(end)) = (text.find('{'), text.rfind('}'))
        && start < end
    {
        candidates.push(&text[start..=end]);
    }
    let mut last_error = None;
    for candidate in candidates {
        match serde_json::from_str::<Plan>(candidate) {
            Ok(plan) => return Ok(plan),
            Err(error) => last_error = Some(error),
        }
    }
    Err(match last_error {
        Some(error) => format!("ответ оркестратора не разобран: {error}"),
        None => "в ответе оркестратора нет JSON".into(),
    })
}

/// Applies the plan: proposals and new tasks first (in order, so keys
/// resolve), then decisions. Returns a line per action for the owner.
fn apply(runs: &Runs, trigger: &Trigger, actions: Vec<Action>) -> Vec<String> {
    let by = AgentId::from(FROM);
    let source = match trigger {
        Trigger::Owner(_) => "owner",
        Trigger::Review(_) => "orchestrator",
        Trigger::Request { .. } => "request",
    };
    // An order: the new tasks belong to the ordering task, never go up the
    // chain and spend what is left of its budget.
    let order = match trigger {
        Trigger::Request { parent, .. } => runs.hub().task(parent).map(|task| {
            let mut forbidden: Vec<String> = crate::runs::ancestors(runs.hub(), parent)
                .into_iter()
                .chain(std::iter::once(task.clone()))
                .filter_map(|t| t.work.map(|w| w.department))
                .collect();
            forbidden.dedup();
            let work = task.work.unwrap_or_default();
            let budget = work.budget_usd.unwrap_or_else(|| {
                runs.home()
                    .departments
                    .get(&work.department)
                    .map_or(0.0, |dept| dept.limits.task_usd)
            });
            (parent.clone(), forbidden, (budget - work.cost_usd).max(0.0))
        }),
        _ => None,
    };
    let mut ordered: Vec<String> = Vec::new();
    let mut keys: HashMap<String, String> = HashMap::new();
    let mut report = Vec::new();
    let (creates, decisions): (Vec<_>, Vec<_>) = actions.into_iter().partition(|action| {
        matches!(
            action,
            Action::CreateTask(_) | Action::ProposeDepartment { .. }
        )
    });
    let mut create =
        |task: NewTask, proposal: Option<DepartmentProposal>, report: &mut Vec<String>| {
            let depends_on = task
                .depends_on
                .iter()
                .map(|dep| keys.get(dep).cloned().unwrap_or_else(|| dep.clone()))
                .collect();
            let mut budget_usd = task.budget_usd;
            let mut parent = None;
            if let Some((id, forbidden, left)) = &order {
                if forbidden.contains(&task.department) {
                    report.push(format!(
                        "⚠ «{}» не создана: заказ не может идти в отдел {} из своей цепочки",
                        task.title, task.department
                    ));
                    return;
                }
                if *left <= 0.0 {
                    report.push(format!(
                        "⚠ «{}» не создана: у заказчика кончился бюджет",
                        task.title
                    ));
                    return;
                }
                budget_usd = Some(budget_usd.map_or(*left, |b| b.min(*left)));
                parent = Some(id.clone());
            }
            let questions = task
                .questions
                .iter()
                .filter(|q| !q.trim().is_empty())
                .map(|text| Question {
                    text: text.trim().to_owned(),
                    answer: None,
                    from: FROM.into(),
                })
                .collect();
            let new = NewWork {
                title: task.title.trim().to_owned(),
                text: task.text,
                department: task.department,
                risk: task.risk,
                depends_on,
                budget_usd,
                parent,
                source: source.into(),
                questions,
                proposal,
            };
            if new.title.is_empty() {
                report.push("⚠ задача без названия пропущена".into());
                return;
            }
            match runs.create(by.clone(), new) {
                Ok(created) => {
                    let work = created.work.as_ref().expect("work task");
                    report.push(format!(
                        "• {} → {} ({}){}",
                        created.title,
                        work.department,
                        created.id,
                        if work.state == WorkState::Waiting {
                            ", ждёт вас"
                        } else {
                            ""
                        }
                    ));
                    if order.is_some() {
                        ordered.push(created.id.clone());
                    }
                    if let Some(key) = task.key {
                        keys.insert(key, created.id);
                    }
                }
                Err(error) => report.push(format!("⚠ «{}» не создана: {error}", task.title)),
            }
        };
    for action in creates {
        match action {
            Action::CreateTask(task) => create(task, None, &mut report),
            Action::ProposeDepartment {
                id,
                title,
                description,
                rules,
                default_risk,
                tasks,
            } => {
                if tasks.is_empty() {
                    report.push(format!("⚠ отдел {id} предложен без задач, пропущен"));
                    continue;
                }
                report.push(format!("🏢 предлагаю отдел {id} — {title}: {description}"));
                let proposal = DepartmentProposal {
                    id,
                    title,
                    description,
                    rules,
                    default_risk,
                };
                for task in tasks {
                    create(task, Some(proposal.clone()), &mut report);
                }
            }
            _ => unreachable!("partitioned"),
        }
    }
    if let Some((parent, _, _)) = &order {
        if ordered.is_empty() {
            fail_request(runs, parent, "оркестратор не оформил заказ");
            report.push(format!("⚠ заказ задачи {parent} не оформлен"));
        } else {
            let _ = runs.hub().update_work(by.clone(), parent, |_, work| {
                work.depends_on.extend(ordered.iter().cloned());
                if work.state == WorkState::Waiting {
                    work.state = WorkState::Queued;
                }
                Ok(())
            });
            runs.kick();
        }
    }
    for action in decisions {
        let line = match action {
            Action::AskOwner { task, questions } => runs
                .ask(by.clone(), &task, &questions)
                .map(|t| format!("❓ вопросы по «{}»", t.title)),
            Action::Accept { task, note } => runs.accept(by.clone(), &task).map(|t| {
                let mut line = format!("✅ принял «{}»", t.title);
                if !note.trim().is_empty() {
                    line.push_str(&format!(": {}", note.trim()));
                }
                line
            }),
            Action::ReturnTask { task, reason } => runs
                .return_task(by.clone(), &task, &reason)
                .map(|t| match &t.work {
                    Some(work) if work.owner_review => {
                        format!(
                            "👀 «{}» второй раз не прошло, нужна ваша приёмка: {reason}",
                            t.title
                        )
                    }
                    _ => format!("🔁 вернул «{}»: {reason}", t.title),
                }),
            Action::SendToOwner { task, summary } => runs.to_owner(by.clone(), &task).map(|t| {
                let mut line = format!("👀 «{}» ждёт вашей приёмки", t.title);
                if !summary.trim().is_empty() {
                    line.push_str(&format!(": {}", summary.trim()));
                }
                line
            }),
            Action::ProposeRule { department, text } => {
                runs.propose_rule(&department, &text).map(|p| {
                    format!(
                        "📜 предложил правило для {}: {}",
                        p.target,
                        short(text.trim(), 120)
                    )
                })
            }
            Action::CreateTask(_) | Action::ProposeDepartment { .. } => unreachable!("partitioned"),
        };
        report.push(line.unwrap_or_else(|error| format!("⚠ {error}")));
    }
    // A review the model did not decide on still needs someone.
    if let Trigger::Review(id) = trigger
        && runs
            .hub()
            .task(id)
            .and_then(|task| task.work)
            .is_some_and(|work| work.state == WorkState::Review && !work.owner_review)
        && runs.to_owner(by, id).is_ok()
    {
        report.push(format!("👀 решения по {id} нет, отдаю вам"));
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{hub::Hub, ledger::Ledger, testing::Scripted};
    use std::rc::Rc;

    #[test]
    fn plans_parse_from_fences_and_braces() {
        let fenced = "Вот план:\n```json\n{\"message\":\"ок\",\"actions\":[{\"type\":\"accept\",\"task\":\"t1\"}]}\n```";
        let plan = parse_plan(fenced).unwrap();
        assert_eq!(plan.message, "ок");
        assert!(matches!(&plan.actions[0], Action::Accept { task, .. } if task == "t1"));
        let bare = "{\"message\":\"привет\"}";
        assert!(parse_plan(bare).unwrap().actions.is_empty());
        assert!(parse_plan("ничего").is_err());
        let unknown = "```json\n{\"actions\":[{\"type\":\"delete_all\"}]}\n```";
        assert!(parse_plan(unknown).is_err());
    }

    fn with_orchestrator(
        reply: &'static str,
        check: impl AsyncFnOnce(Orchestrator, Runs, Hub, std::path::PathBuf),
    ) {
        let dir = std::env::temp_dir().join(format!("agentsflow-orch-{}", uuid::Uuid::new_v4()));
        crate::home::init(&dir).unwrap();
        let pr = dir.join("departments").join("pr");
        std::fs::create_dir_all(&pr).unwrap();
        std::fs::write(pr.join("department.toml"), crate::department::tests::SAMPLE).unwrap();
        let home = Home::load(&dir).unwrap();
        let hub = Hub::new();
        let replies: &'static [&'static str] = Box::leak(Box::new([reply]));
        let env = RunEnv {
            codex_bin: None,
            spawner: Some(Rc::new(move |hub, member, dir| {
                let dir = dir.to_path_buf();
                crate::hub::spawn(hub.clone(), member.id.clone(), true, move || {
                    let dir = dir.clone();
                    async move { Ok(Scripted::new(replies, Some(0.05), &dir)) }
                })
            })),
        };
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(tokio::task::LocalSet::new().run_until(async {
                // Department runs stay idle: tasks below wait for answers.
                let runs = Runs::start(hub.clone(), home, Ledger::memory(), RunEnv::default());
                let orchestrator = Orchestrator::start(runs.clone(), env);
                check(orchestrator, runs, hub, dir.clone()).await;
            }));
        std::fs::remove_dir_all(dir).unwrap();
    }

    async fn wait_mail(hub: &Hub, subject: &str) -> String {
        for _ in 0..500 {
            if let Some(mail) = hub
                .mail_all()
                .into_iter()
                .find(|mail| mail.subject == subject && mail.from.as_str() == FROM)
            {
                return mail.body;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no mail {subject:?}: {:?}", hub.mail_all());
    }

    #[test]
    fn owner_message_becomes_tasks_and_a_proposal() {
        let reply = r#"```json
{"message":"Разложил две задачи.","actions":[
 {"type":"create_task","key":"post","title":"Пост для Orca","text":"Анонс релиза","department":"pr","questions":["Какая дата релиза?"]},
 {"type":"create_task","title":"Отчёт","text":"по посту","department":"general","depends_on":["post"],"questions":["Формат?"]},
 {"type":"propose_department","id":"hr","title":"Кадры","description":"Найм","tasks":[{"title":"Вакансия","text":"Сеньор раст"}]},
 {"type":"create_task","title":"Мимо","text":"x","department":"nope"}
]}
```"#;
        with_orchestrator(reply, async |orchestrator, runs, hub, _| {
            orchestrator
                .message("сделай пост, отчёт и вакансию")
                .unwrap();
            let body = wait_mail(&hub, "Разобрал сообщение").await;
            assert!(body.starts_with("Разложил две задачи."));
            assert!(body.contains("предлагаю отдел hr"));
            assert!(body.contains("«Мимо» не создана"));
            let tasks = hub.tasks(false);
            let post = tasks.iter().find(|t| t.title == "Пост для Orca").unwrap();
            let report = tasks.iter().find(|t| t.title == "Отчёт").unwrap();
            assert_eq!(
                report.work.as_ref().unwrap().depends_on,
                vec![post.id.clone()]
            );
            assert_eq!(post.work.as_ref().unwrap().state, WorkState::Waiting);
            let vacancy = tasks.iter().find(|t| t.title == "Вакансия").unwrap();
            let work = vacancy.work.as_ref().unwrap();
            assert_eq!(work.department, "hr");
            assert_eq!(work.proposal.as_ref().unwrap().title, "Кадры");
            assert_eq!(work.source, "owner");
            // The session's cost lands in the ledger under "orchestrator".
            for _ in 0..100 {
                if runs.month_spend().contains_key("orchestrator") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(runs.month_spend()["orchestrator"] > 0.0);
        });
    }

    #[test]
    fn internal_results_are_accepted_by_the_orchestrator() {
        with_orchestrator(
            "{\"message\":\"\",\"actions\":[{\"type\":\"accept\",\"task\":\"__ID__\"}]}",
            async |_, runs, hub, _| {
                // The scripted reply cannot know the id; accept fails and the
                // task goes to the owner instead.
                let mut new = NewWork {
                    title: "Отчёт".into(),
                    text: "посчитать".into(),
                    department: "pr".into(),
                    risk: Some(Risk::Internal),
                    ..Default::default()
                };
                new.questions = vec![Question {
                    text: "?".into(),
                    answer: None,
                    from: FROM.into(),
                }];
                let task = runs.create(AgentId::from("owner"), new).unwrap();
                hub.update_work(AgentId::orchestrator(), &task.id, |_, work| {
                    work.state = WorkState::Review;
                    work.result_summary = "42".into();
                    work.runs.push(agentsflow_events::RunRef {
                        id: "r1".into(),
                        started_ms: 0,
                        finished_ms: Some(1),
                        outcome: "finished".into(),
                        cost_usd: 0.0,
                    });
                    Ok(())
                })
                .unwrap();
                let body = wait_mail(&hub, "Приёмка").await;
                assert!(body.contains("отдаю вам"), "{body}");
                let work = hub.task(&task.id).unwrap().work.unwrap();
                assert!(work.owner_review);
                assert_eq!(work.state, WorkState::Review);
            },
        );
    }

    #[test]
    fn broken_reply_is_reported_to_the_owner() {
        with_orchestrator(
            "не знаю что сказать",
            async |orchestrator, _, hub, _| {
                orchestrator.message("сделай что-нибудь").unwrap();
                let body = wait_mail(&hub, "Не смог разобрать сообщение").await;
                assert!(body.contains("нет JSON"));
                assert!(hub.tasks(false).is_empty());
            },
        );
    }

    async fn wait_for(hub: &Hub, id: &str, state: WorkState) -> TaskSnapshot {
        for _ in 0..1000 {
            if let Some(task) = hub.task(id)
                && task.work.as_ref().is_some_and(|w| w.state == state)
            {
                return task;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{id} did not reach {state:?}: {:?}", hub.task(id));
    }

    #[test]
    fn departments_order_work_through_the_orchestrator() {
        let dir = std::env::temp_dir().join(format!("agentsflow-order-{}", uuid::Uuid::new_v4()));
        crate::home::init(&dir).unwrap();
        let pr = dir.join("departments").join("pr");
        std::fs::create_dir_all(&pr).unwrap();
        std::fs::write(pr.join("department.toml"), crate::department::tests::SAMPLE).unwrap();
        let home = Home::load(&dir).unwrap();
        let hub = Hub::new();
        // The first doer of `pr` orders figures; everyone else just works.
        let ordered = Rc::new(std::cell::Cell::new(false));
        let runs_env = RunEnv {
            codex_bin: None,
            spawner: Some(Rc::new(move |hub, member, dir| {
                let dir = dir.to_path_buf();
                let doer = member.roles.iter().any(|r| r == "doer");
                if doer && member.id.as_str().starts_with("pr:") && !ordered.get() {
                    ordered.set(true);
                    std::fs::write(
                        dir.join(crate::runs::REQUEST_FILE),
                        "Нужны продажи Байтака за октябрь",
                    )
                    .unwrap();
                }
                let replies: &'static [&'static str] = if doer {
                    &["write:Готово"]
                } else {
                    &["VERDICT: APPROVED"]
                };
                crate::hub::spawn(hub.clone(), member.id.clone(), true, move || {
                    let dir = dir.clone();
                    async move { Ok(Scripted::new(replies, Some(0.1), &dir)) }
                })
            })),
        };
        let calls = Rc::new(std::cell::Cell::new(0));
        let orchestrator_env = RunEnv {
            codex_bin: None,
            spawner: Some(Rc::new(move |hub, member, dir| {
                let dir = dir.to_path_buf();
                calls.set(calls.get() + 1);
                // First the order, then a review it leaves to the owner.
                let replies: &'static [&'static str] = if calls.get() == 1 {
                    &[
                        r#"{"message":"","actions":[{"type":"create_task","title":"Продажи Байтака","text":"за октябрь","department":"general"},{"type":"create_task","title":"Мимо","text":"x","department":"pr"}]}"#,
                    ]
                } else {
                    &[r#"{"message":"","actions":[]}"#]
                };
                crate::hub::spawn(hub.clone(), member.id.clone(), true, move || {
                    let dir = dir.clone();
                    async move { Ok(Scripted::new(replies, None, &dir)) }
                })
            })),
        };
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(tokio::task::LocalSet::new().run_until(async {
                let runs = Runs::start(hub.clone(), home, Ledger::memory(), runs_env);
                runs.set_orchestrator(Orchestrator::start(runs.clone(), orchestrator_env));
                let parent = runs
                    .create(
                        AgentId::from("owner"),
                        NewWork {
                            title: "Пост по итогам".into(),
                            text: "Пост о продажах".into(),
                            department: "pr".into(),
                            ..Default::default()
                        },
                    )
                    .unwrap();
                let body = wait_mail(&hub, "Заказ отдела").await;
                assert!(body.contains("из своей цепочки"), "{body}");
                let child = hub
                    .tasks(false)
                    .into_iter()
                    .find(|t| t.title == "Продажи Байтака")
                    .unwrap();
                let child_work = child.work.clone().unwrap();
                assert_eq!(child_work.parent.as_deref(), Some(parent.id.as_str()));
                assert_eq!(child_work.source, "request");
                // What is left of the parent's $2.50 after its first run.
                assert!(child_work.budget_usd.unwrap() < 2.5);
                let waiting = hub.task(&parent.id).unwrap().work.unwrap();
                assert_eq!(waiting.depends_on, vec![child.id.clone()]);
                assert_eq!(waiting.runs[0].outcome, "request");
                // The child is internal; the orchestrator leaves it to the owner.
                let reviewed = wait_for(&hub, &child.id, WorkState::Review).await;
                for _ in 0..200 {
                    if hub.task(&child.id).unwrap().work.unwrap().owner_review {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                let spent_child = reviewed.work.unwrap().cost_usd;
                hub.update_task(
                    AgentId::orchestrator(),
                    &child.id,
                    crate::hub::TaskPatch {
                        status: Some(agentsflow_events::TaskStatus::Done),
                        ..Default::default()
                    },
                )
                .unwrap();
                let done = wait_for(&hub, &parent.id, WorkState::Review).await;
                let work = done.work.unwrap();
                assert_eq!(work.runs.len(), 2);
                // The order's spending counts against the parent.
                assert!(work.cost_usd >= spent_child + 0.2 - 1e-9);
                let brief = std::fs::read_to_string(
                    dir.join("departments/pr/runs")
                        .join(&parent.id)
                        .join("brief.md"),
                )
                .unwrap();
                assert!(brief.contains("Продажи Байтака"), "inputs list the order");
            }));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
