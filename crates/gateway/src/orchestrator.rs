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
/// Subject of a mail that carries a task result: the chat adds its file buttons.
pub const RESULT_SUBJECT: &str = "Итог задачи ";
/// How many closed tasks the orchestrator sees, newest first.
const RECENT_CLOSED: usize = 8;
const DIALOG_FILE: &str = "dialog.jsonl";
/// Exchanges of the conversation each session gets; every session starts fresh.
const DIALOG_TURNS: usize = 6;
const DIALOG_TEXT: usize = 600;

#[derive(Clone)]
pub struct Orchestrator {
    tx: mpsc::UnboundedSender<Trigger>,
}

#[derive(Clone, Debug, PartialEq)]
enum Trigger {
    /// Text of the owner and the files sent with it.
    Owner(String, Vec<String>),
    Review(String),
    /// A department run ordered work from another department.
    Request {
        parent: String,
        text: String,
        /// Files the ordering run handed over, already in the inbox.
        files: Vec<String>,
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
            session: String::new(),
        };
        tokio::task::spawn_local(engine.run(rx));
        Self { tx }
    }

    /// A free-form message of the owner: tasks, answers, anything.
    /// An order of a department run, filed as a task of another department.
    pub fn request(&self, parent: &str, text: &str, files: Vec<String>) -> Result<(), String> {
        self.tx
            .send(Trigger::Request {
                parent: parent.to_owned(),
                text: text.to_owned(),
                files,
            })
            .map_err(|_| "оркестратор остановлен".to_owned())
    }

    #[cfg(test)]
    pub fn message(&self, text: &str) -> Result<(), String> {
        self.message_with_files(text, Vec::new())
    }

    /// The files go to every task made from this message.
    pub fn message_with_files(&self, text: &str, files: Vec<String>) -> Result<(), String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("пустое сообщение".into());
        }
        self.tx
            .send(Trigger::Owner(text.to_owned(), files))
            .map_err(|_| "оркестратор остановлен".to_owned())
    }
}

struct Engine {
    runs: Runs,
    env: RunEnv,
    queue: VecDeque<Trigger>,
    /// Results already handed to acceptance: (task, run count).
    seen: HashSet<(String, usize)>,
    /// Id of the current session: unique across restarts, so the ledger and
    /// the page never mix two sessions.
    session: String,
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
        let Trigger::Owner(mut text, mut files) = first else {
            return Some(first);
        };
        while let Some(Trigger::Owner(more, more_files)) = self.queue.front() {
            text.push_str("\n\n");
            text.push_str(more);
            files.extend(more_files.iter().cloned());
            self.queue.pop_front();
        }
        Some(Trigger::Owner(text, files))
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
            if let Trigger::Owner(..) = trigger {
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
        self.session = uuid::Uuid::new_v4().simple().to_string()[..8].to_owned();
        let reply = self.call(&home, prompt).await;
        // Usage events of the session arrive before it finishes; drain them.
        while let Ok(envelope) = events.try_recv() {
            self.on_event(envelope.agent, envelope.event);
        }
        let outcome = reply.and_then(|text| parse_plan(&text));
        let kind = match &trigger {
            Trigger::Owner(..) => "сообщение".to_owned(),
            Trigger::Review(id) => format!("приёмка {id}"),
            Trigger::Request { parent, .. } => format!("заказ от {parent}"),
        };
        if let Err(error) = &outcome {
            crate::diag::diag(
                "WARN",
                &format!("оркестратор {}, {kind}: {error}", self.session),
            );
        }
        match (outcome, &trigger) {
            (Ok(plan), _) => {
                let report = apply(&self.runs, &trigger, plan.actions);
                crate::diag::diag(
                    "INFO",
                    &format!(
                        "оркестратор {}, {kind}: {}",
                        self.session,
                        report.join("; ")
                    ),
                );
                let mut body = plan.message.trim().to_owned();
                if !report.is_empty() {
                    if !body.is_empty() {
                        body.push_str("\n\n");
                    }
                    body.push_str(&report.join("\n"));
                }
                if let Trigger::Owner(text, _) = &trigger {
                    remember(&home, text, &body);
                }
                if !body.is_empty() {
                    let subject = match &trigger {
                        Trigger::Owner(..) => "Разобрал сообщение",
                        Trigger::Review(_) => "Приёмка",
                        Trigger::Request { .. } => "Заказ отдела",
                    };
                    self.tell(subject, body);
                }
            }
            (Err(error), Trigger::Owner(text, _)) => self.tell(
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
        let id = AgentId::from(format!("{AGENT_PREFIX}{}", self.session).as_str());
        let member = Member {
            id: id.clone(),
            kind: config.kind,
            command: config.command.clone(),
            model: config.model.clone(),
            effort: config.effort.clone(),
            mode: Some(AccessMode::Smart),
            sandbox: None,
            guard: true,
            title: "оркестратор".into(),
            roles: vec!["lead".into()],
            lead: true,
            sites: Vec::new(),
            repos: Vec::new(),
            workdir: None,
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
  key — метка для depends_on внутри этого ответа; questions — только если без ответа будет угадывание (задача подождёт ответа); risk, depends_on, questions, budget_usd, size необязательны.
  "size":"small" — мелкая задача: короткая справка, правка пары строк, одно простое действие. Её сделает один быстрый агент без ревью. Сомневаешься — не ставь; external мелким не бывает.
  "repo":"путь" — репозиторий из списка отдела, в котором делать задачу. Не указан — основной (первый). У отдела один репозиторий — не указывай.
  Как выбрать отдел: работа в магазине по списку или файлу заданий (доноры, группы, сверки) — в отдел этого магазина, все пункты списка одной задачей: их делает один исполнитель и сам себя проверяет. Разобраться, почему что-то сломалось, починить, добавить функцию в коде — в отдел разработки с нужным repo: там ведущий разбирается и пишет план, исполнитель делает, ведущий проверяет.
- {"type":"propose_department","id":"латиница-через-дефис","title":"...","description":"за что отвечает","rules":"правила отдела в markdown","default_risk":"internal|external","tasks":[ ...create_task без department... ]}
  Новый отдел создаётся только после «да» хозяина; его задачи подождут.
- {"type":"ask_owner","task":"id","questions":["..."]} — вопросы по уже существующей задаче. Не повторяй вопросы, на которые хозяин уже ответил, даже другими словами. Ответ «на свой вкус» значит, что решает отдел: верни ему задачу (return_task) с указанием выбрать самому, а не спрашивай снова.
- {"type":"accept","task":"id","note":"..."} — принять internal-результат.
- {"type":"return_task","task":"id","reason":"что конкретно исправить"} — вернуть отделу (один раз).
- {"type":"send_to_owner","task":"id","summary":"..."} — отдать результат хозяину на приёмку.
- {"type":"propose_rule","department":"id отдела или orchestrator","text":"одно правило коротко"} — постоянное правило; добавится только после «да» хозяина. Предлагай, когда хозяин просит запомнить или повторяет одно и то же указание.
- {"type":"show_result","task":"id"} — прислать хозяину итог задачи целиком (шлёт шлюз, сам не пересказывай). Только по его сообщению.
- {"type":"answer","task":"id","text":"слова хозяина"} — передать его ответ на открытые вопросы задачи. Только по его сообщению.
- {"type":"close_task","task":"id"} — закрыть задачу, если хозяин просит. Только по его сообщению.

Если делать нечего, верни пустой "actions" и ответь хозяину в "message"."#;

/// Keeps one exchange: sessions are fresh, so "да" or "последний" would
/// mean nothing without what was said before.
fn remember(home: &Home, owner: &str, reply: &str) {
    use std::io::Write;
    let line = serde_json::json!({
        "ts_ms": now_ms(),
        "owner": short(owner.trim(), DIALOG_TEXT),
        "reply": short(reply.trim(), DIALOG_TEXT),
    });
    let dir = home.state_dir();
    let _ = std::fs::create_dir_all(&dir);
    let written = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(DIALOG_FILE))
        .and_then(|mut file| writeln!(file, "{line}"));
    if let Err(error) = written {
        crate::server_diag!("orchestrator dialog not saved: {error}");
    }
}

fn recent_dialog(home: &Home) -> Vec<(String, String)> {
    let text = std::fs::read_to_string(home.state_dir().join(DIALOG_FILE)).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(DIALOG_TURNS)..]
        .iter()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .map(|value| {
            let field = |name: &str| value[name].as_str().unwrap_or_default().to_owned();
            (field("owner"), field("reply"))
        })
        .collect()
}

/// The text of a task result: the file, or the summary without one.
fn result_text(home: &Home, work: &agentsflow_events::WorkInfo) -> String {
    work.result_path
        .as_ref()
        .and_then(|path| std::fs::read_to_string(home.root.join(path)).ok())
        .unwrap_or_else(|| work.result_summary.clone())
        .trim()
        .to_owned()
}

/// The whole input of one orchestrator session.
fn prompt(home: &Home, tasks: &[TaskSnapshot], trigger: &Trigger) -> String {
    let mut out = String::new();
    let rules = home.orchestrator_rules();
    out.push_str(rules.trim());
    out.push_str("\n\n# Отделы\n\n");
    for dept in home.departments.values() {
        out.push_str(&format!(
            "- `{}` — {}: {} (риск по умолчанию {}; {})\n",
            dept.id,
            dept.title,
            dept.description,
            risk_name(dept.default_risk),
            dept.shape()
        ));
        for repo in &dept.repos {
            out.push_str(&format!(
                "  - repo `{}` — {}\n",
                repo.display(),
                crate::projectmem::describe(repo)
            ));
        }
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
            if work.state == WorkState::Queued && !work.depends_on.is_empty() {
                out.push_str(&format!(
                    "  - ждёт задачи: {}\n",
                    work.depends_on.join(", ")
                ));
            }
            for question in work.questions.iter().filter(|q| q.answer.is_none()) {
                out.push_str(&format!("  - открытый вопрос: {}\n", question.text));
            }
        }
    }
    let mut closed: Vec<_> = tasks
        .iter()
        .filter_map(|task| task.work.as_ref().map(|work| (task, work)))
        .filter(|(_, work)| matches!(work.state, WorkState::Done | WorkState::Cancelled))
        .collect();
    closed.sort_by_key(|(task, _)| std::cmp::Reverse(task.updated_ms));
    if !closed.is_empty() {
        out.push_str("\n# Недавно закрытые задачи (сначала новые)\n\n");
        for (task, work) in closed.into_iter().take(RECENT_CLOSED) {
            out.push_str(&format!(
                "- `{}` [{}, {}] {}: {}\n",
                task.id,
                work.department,
                state_name(work.state),
                task.title,
                short(work.result_summary.trim(), 200).replace('\n', " ")
            ));
        }
    }
    if matches!(trigger, Trigger::Owner(..)) {
        let dialog = recent_dialog(home);
        if !dialog.is_empty() {
            out.push_str("\n# Последние сообщения (для понимания, что имел в виду хозяин)\n\n");
            for (owner, reply) in dialog {
                out.push_str(&format!("- Хозяин: {owner}\n  Ты: {reply}\n"));
            }
        }
    }
    match trigger {
        Trigger::Owner(text, files) => {
            out.push_str("\n# Сообщение хозяина\n\n");
            out.push_str(text.trim());
            if !files.is_empty() {
                out.push_str("\n\nК сообщению приложены файлы:\n");
                for file in files {
                    out.push_str(&format!("- {}\n", file.rsplit('/').next().unwrap_or(file)));
                }
                out.push_str(
                    "Шлюз сам положит их в папку каждой задачи из этого сообщения и назовёт в брифе. Ты их не открываешь: упомяни в постановке, что с ними сделать.",
                );
            }
            out.push_str(
                "\n\nСначала пойми, чего он хочет:\n\
                 - Просит показать итог, результат, отчёт или файл: show_result. «Последний» значит \
                 самая свежая задача с готовым результатом. Не пересказывай итог сам, его пришлёт шлюз.\n\
                 - Отвечает на открытый вопрос задачи: answer. Его ответ важнее формулировки вопроса.\n\
                 - Просит закрыть, убрать или отменить задачи: close_task на каждую.\n\
                 - Спрашивает, что происходит: ответь в message по спискам выше, без новых задач.\n\
                 - Просит сделать новую работу: разложи на задачи по отделам.\n\
                 Если неясно, о какой задаче речь, но вариант один, бери его; вариантов несколько — \
                 коротко назови их и спроси, какой.\n",
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
                let answered: Vec<_> = work
                    .questions
                    .iter()
                    .filter_map(|q| q.answer.as_ref().map(|answer| (&q.text, answer)))
                    .collect();
                if !answered.is_empty() {
                    out.push_str("\n## Ответы хозяина\n\n");
                    for (question, answer) in answered {
                        out.push_str(&format!("- {question}\n  Ответ: {}\n", answer.trim()));
                    }
                }
                if !work.review_notes.is_empty() {
                    out.push_str("\n## Прошлые замечания\n\n");
                    for note in &work.review_notes {
                        out.push_str(&format!("- {note}\n"));
                    }
                }
                out.push_str("\n## Результат\n\n");
                out.push_str(&short(&result_text(home, work), RESULT_LIMIT));
                let files = handed_files(home, id, work);
                if !files.is_empty() {
                    out.push_str(
                        "\n\n## Файлы задачи\n\nВсе задачи, которые ты создашь на этой приёмке, \
                         получат эти файлы во входных данных; в постановке называй файл по имени:\n",
                    );
                    for (path, size) in files {
                        let name = path.rsplit('/').next().unwrap_or(&path);
                        out.push_str(&format!("- {name} ({} КБ)\n", size.div_ceil(1024)));
                    }
                }
                out.push_str(&format!(
                    "\n\nПрими (accept), верни (return_task) или отдай хозяину (send_to_owner) задачу `{}`.\n",
                    task.id
                ));
            }
        }
        Trigger::Request {
            parent,
            text,
            files,
        } => {
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
            if !files.is_empty() {
                out.push_str(
                    "К заказу приложены файлы, исполнитель получит их во входных данных:\n",
                );
                for file in files {
                    let name = file.rsplit('/').next().unwrap_or(file);
                    out.push_str(&format!("- {name}\n"));
                }
                out.push('\n');
            }
            out.push_str(
                "Оформи заказ одной задачей create_task (можно с вопросами хозяину, если без них никак) \
                 в подходящий отдел, но не в отдел, который заказывает, и не выше по цепочке. \
                 Задача заказчика продолжится, когда заказ будет готов. Не ставь условий \
                 «не начинать без ответов»: если хозяин оставит выбор отделу, отдел решает сам.\n",
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
    #[serde(default)]
    size: Option<String>,
    /// One of the department's repositories, as listed in the prompt.
    #[serde(default)]
    repo: Option<String>,
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
    ShowResult {
        task: String,
    },
    Answer {
        task: String,
        text: String,
    },
    CloseTask {
        task: String,
    },
}

/// The gateway sends the result itself, so the model cannot retell it wrong.
fn show_result(runs: &Runs, by: &AgentId, id: &str) -> Result<(), String> {
    let task = runs
        .hub()
        .task(id)
        .ok_or_else(|| format!("задачи {id} нет"))?;
    let work = task
        .work
        .as_ref()
        .ok_or_else(|| format!("у {id} нет отдела"))?;
    let text = result_text(&runs.home(), work);
    if text.is_empty() {
        return Err(format!("у «{}» пока нет итога", task.title));
    }
    let subject: String = format!("{RESULT_SUBJECT}{id}: {}", task.title)
        .chars()
        .take(200)
        .collect();
    runs.hub()
        .send_mail(
            by.clone(),
            AgentId::orchestrator(),
            subject,
            short(&text, 30_000),
            None,
        )
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn close_task(runs: &Runs, by: &AgentId, id: &str) -> Result<TaskSnapshot, String> {
    let work = runs
        .hub()
        .task(id)
        .and_then(|task| task.work)
        .ok_or_else(|| format!("задачи {id} нет"))?;
    if work.state == WorkState::Running {
        return Err(format!(
            "задача {id} сейчас в работе, её можно остановить в карточке"
        ));
    }
    runs.hub()
        .update_task(
            by.clone(),
            id,
            crate::hub::TaskPatch {
                status: Some(agentsflow_events::TaskStatus::Archived),
                ..Default::default()
            },
        )
        .map_err(|error| error.to_string())
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

/// Files a reviewed task hands to the tasks made on its review: the owner's
/// attachments, the result and what the run left in `artifacts/`, without
/// the browser's logs. Paths are relative to the home.
fn handed_files(home: &Home, id: &str, work: &agentsflow_events::WorkInfo) -> Vec<(String, u64)> {
    let mut files: Vec<(String, u64)> = work
        .attachments
        .iter()
        .filter_map(|file| {
            let meta = std::fs::metadata(home.root.join(file)).ok()?;
            meta.is_file().then(|| (file.clone(), meta.len()))
        })
        .collect();
    if let Some(dept) = home.departments.get(&work.department) {
        for entry in crate::telegram::files::TaskFiles::read(&dept.run_dir(id)).entries {
            if entry.name.starts_with("browser/") {
                continue;
            }
            if let Ok(relative) = entry.path.strip_prefix(&home.root) {
                files.push((relative.to_string_lossy().replace('\\', "/"), entry.size));
            }
        }
    }
    files
}

/// Applies the plan: proposals and new tasks first (in order, so keys
/// resolve), then decisions. Returns a line per action for the owner.
fn apply(runs: &Runs, trigger: &Trigger, actions: Vec<Action>) -> Vec<String> {
    let by = AgentId::from(FROM);
    let source = match trigger {
        Trigger::Owner(..) => "owner",
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
    let attachments = match trigger {
        Trigger::Owner(_, files) | Trigger::Request { files, .. } => files.clone(),
        Trigger::Review(id) => runs
            .hub()
            .task(id)
            .and_then(|task| task.work)
            .map(|work| {
                handed_files(&runs.home(), id, &work)
                    .into_iter()
                    .map(|(path, _)| path)
                    .collect()
            })
            .unwrap_or_default(),
    };
    let mut attached = false;
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
            let repo = match task
                .repo
                .as_deref()
                .map(str::trim)
                .filter(|r| !r.is_empty())
            {
                None => None,
                Some(repo) => {
                    let home = runs.home();
                    let known = home
                        .departments
                        .get(&task.department)
                        .and_then(|dept| crate::projectmem::find_repo(&dept.repos, repo));
                    if known.is_none() {
                        report.push(format!(
                            "⚠ «{}»: репозитория {repo} нет у отдела {}, взят основной",
                            task.title.trim(),
                            task.department
                        ));
                    }
                    known
                }
            };
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
                small: task.size.as_deref() == Some("small"),
                attachments: attachments.clone(),
            };
            if new.title.is_empty() {
                report.push("⚠ задача без названия пропущена".into());
                return;
            }
            match runs.create(by.clone(), new) {
                Ok(created) => {
                    attached = true;
                    if let Some(repo) = repo {
                        let _ = runs.hub().update_work(by.clone(), &created.id, |_, work| {
                            work.repo = Some(repo.to_string_lossy().into_owned());
                            Ok(())
                        });
                    }
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
    if !attachments.is_empty() && !attached && !matches!(trigger, Trigger::Review(_)) {
        report.push(
            "📎 Задач из сообщения не вышло, файлы никуда не ушли: пришлите их с задачей".into(),
        );
    }
    let from_owner = matches!(trigger, Trigger::Owner(..));
    for action in decisions {
        // Only the owner's own words may answer questions or close work.
        if !from_owner
            && matches!(
                action,
                Action::ShowResult { .. } | Action::Answer { .. } | Action::CloseTask { .. }
            )
        {
            report.push("⚠ это действие доступно только по сообщению хозяина".into());
            continue;
        }
        let line = match action {
            Action::ShowResult { task } => match show_result(runs, &by, &task) {
                Ok(()) => continue,
                Err(error) => Err(error),
            },
            Action::Answer { task, text } => runs
                .answer(&task, &text)
                .map(|t| format!("✍ передал ответ в «{}»", t.title)),
            Action::CloseTask { task } => {
                close_task(runs, &by, &task).map(|t| format!("🗄 закрыл «{}»", t.title))
            }
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
            gh: None,
            stall_clock: None,
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
    fn files_go_to_every_task_of_the_message_and_are_named_to_the_model() {
        let reply = r#"{"message":"","actions":[
 {"type":"create_task","title":"Первая","text":"x","department":"general","questions":["?"]},
 {"type":"create_task","title":"Вторая","text":"y","department":"general","questions":["?"]}
]}"#;
        with_orchestrator(reply, async |orchestrator, _, hub, _| {
            let file = ".agentsflow/inbox/ab12/цены.xlsx".to_owned();
            orchestrator
                .message_with_files("сравни цены", vec![file.clone()])
                .unwrap();
            wait_mail(&hub, "Разобрал сообщение").await;
            for task in hub.tasks(false) {
                assert_eq!(task.work.unwrap().attachments, vec![file.clone()]);
            }
        });
        let dir = std::env::temp_dir().join(format!("agentsflow-prompt-{}", uuid::Uuid::new_v4()));
        crate::home::init(&dir).unwrap();
        let home = Home::load(&dir).unwrap();
        let text = prompt(
            &home,
            &[],
            &Trigger::Owner(
                "сравни".into(),
                vec![".agentsflow/inbox/ab12/цены.xlsx".into()],
            ),
        );
        assert!(text.contains("- цены.xlsx"));
        assert!(!text.contains(".agentsflow/inbox"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tasks_made_on_a_review_get_the_reviewed_task_files() {
        with_orchestrator("{}", async |_, runs, hub, dir| {
            let done = runs
                .create(
                    AgentId::from(FROM),
                    NewWork {
                        title: "Проверка Kaspi".into(),
                        text: "x".into(),
                        department: "general".into(),
                        risk: None,
                        depends_on: vec![],
                        budget_usd: None,
                        parent: None,
                        source: "owner".into(),
                        questions: vec![],
                        proposal: None,
                        small: false,
                        attachments: vec![".agentsflow/inbox/ab12/пул.xlsx".into()],
                    },
                )
                .unwrap();
            std::fs::create_dir_all(dir.join(".agentsflow/inbox/ab12")).unwrap();
            std::fs::write(dir.join(".agentsflow/inbox/ab12/пул.xlsx"), b"pool").unwrap();
            let run = runs.home().departments["general"].run_dir(&done.id);
            std::fs::create_dir_all(run.join("artifacts/browser")).unwrap();
            std::fs::write(run.join("result.md"), b"# done").unwrap();
            std::fs::write(run.join("artifacts/проверка.xlsx"), b"checked").unwrap();
            std::fs::write(run.join("artifacts/browser/console.log"), b"noise").unwrap();
            let action: Action = serde_json::from_str(
                r#"{"type":"create_task","title":"BuyShark","text":"сверь с проверка.xlsx","department":"general"}"#,
            )
            .unwrap();
            let report = apply(&runs, &Trigger::Review(done.id.clone()), vec![action]);
            let made = hub
                .tasks(false)
                .into_iter()
                .find(|task| task.title == "BuyShark")
                .unwrap_or_else(|| panic!("{report:?}"));
            let base = format!("departments/general/runs/{}", done.id);
            assert_eq!(
                made.work.unwrap().attachments,
                vec![
                    ".agentsflow/inbox/ab12/пул.xlsx".to_owned(),
                    format!("{base}/result.md"),
                    format!("{base}/artifacts/проверка.xlsx"),
                ]
            );
            let text = prompt(
                &runs.home(),
                &hub.tasks(false),
                &Trigger::Review(done.id.clone()),
            );
            assert!(text.contains("## Файлы задачи"), "{text}");
            assert!(text.contains("- проверка.xlsx (1 КБ)"), "{text}");
            assert!(!text.contains("console.log"), "{text}");
        });
    }

    #[test]
    fn owner_gets_a_result_answers_and_closes_by_words() {
        with_orchestrator("{}", async |_, runs, hub, _| {
            let make = |title: &str| {
                runs.create(
                    AgentId::from(FROM),
                    NewWork {
                        title: title.into(),
                        text: "x".into(),
                        department: "general".into(),
                        risk: None,
                        depends_on: vec![],
                        budget_usd: None,
                        parent: None,
                        source: "owner".into(),
                        questions: vec![Question {
                            text: "Какой?".into(),
                            answer: None,
                            from: FROM.into(),
                        }],
                        proposal: None,
                        small: false,
                        attachments: vec![],
                    },
                )
                .unwrap()
            };
            let result = make("Отчёт");
            hub.update_work(AgentId::from(FROM), &result.id, |_, work| {
                work.result_summary = "полный итог отчёта".into();
                Ok(())
            })
            .unwrap();
            let ask = make("Цвет");
            let old = make("Старая");
            let actions = vec![
                Action::ShowResult {
                    task: result.id.clone(),
                },
                Action::Answer {
                    task: ask.id.clone(),
                    text: "синий".into(),
                },
                Action::CloseTask {
                    task: old.id.clone(),
                },
                Action::ShowResult {
                    task: "нет-такой".into(),
                },
            ];
            let trigger = Trigger::Owner("покажи".into(), vec![]);
            let report = apply(&runs, &trigger, actions);
            let mail = hub
                .mail_all()
                .into_iter()
                .find(|mail| mail.subject.starts_with(RESULT_SUBJECT))
                .expect("result mail");
            assert_eq!(mail.body, "полный итог отчёта");
            assert!(mail.subject.contains(&result.id));
            let answered = hub.task(&ask.id).unwrap().work.unwrap();
            assert_eq!(answered.questions[0].answer.as_deref(), Some("синий"));
            assert_eq!(
                hub.task(&old.id).unwrap().work.unwrap().state,
                WorkState::Cancelled
            );
            assert!(
                report.iter().any(|line| line.contains("нет-такой")),
                "{report:?}"
            );

            // Words of a department or a review never act for the owner.
            let denied = apply(
                &runs,
                &Trigger::Review(result.id.clone()),
                vec![Action::Answer {
                    task: result.id.clone(),
                    text: "да".into(),
                }],
            );
            assert!(
                denied
                    .iter()
                    .any(|line| line.contains("только по сообщению")),
                "{denied:?}"
            );
            assert!(
                hub.task(&result.id).unwrap().work.unwrap().questions[0]
                    .answer
                    .is_none()
            );
        });
    }

    #[test]
    fn prompt_shows_closed_tasks_and_the_conversation() {
        let dir = std::env::temp_dir().join(format!("agentsflow-prompt-{}", uuid::Uuid::new_v4()));
        crate::home::init(&dir).unwrap();
        let home = Home::load(&dir).unwrap();
        remember(&home, "сделай отчёт", "разложил");
        let task = TaskSnapshot {
            id: "t1".into(),
            title: "Отчёт".into(),
            description: String::new(),
            status: agentsflow_events::TaskStatus::Done,
            assignee: None,
            progress: 0,
            labels: vec![],
            note: String::new(),
            thread: None,
            created_ms: 0,
            updated_ms: 1,
            work: Some(agentsflow_events::WorkInfo {
                department: "general".into(),
                state: WorkState::Done,
                result_summary: "готово".into(),
                ..Default::default()
            }),
        };
        let text = prompt(
            &home,
            &[task],
            &Trigger::Owner("покажи итог".into(), vec![]),
        );
        assert!(
            text.contains("Недавно закрытые") && text.contains("`t1`"),
            "{text}"
        );
        assert!(text.contains("Хозяин: сделай отчёт"), "{text}");
        assert!(text.contains("show_result"), "{text}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn files_without_a_task_are_reported() {
        let reply = r#"{"message":"Просто отвечаю.","actions":[]}"#;
        with_orchestrator(reply, async |orchestrator, _, hub, _| {
            orchestrator
                .message_with_files("что это?", vec![".agentsflow/inbox/a/b.txt".into()])
                .unwrap();
            let body = wait_mail(&hub, "Разобрал сообщение").await;
            assert!(body.contains("файлы никуда не ушли"), "{body}");
        });
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
    fn review_prompt_carries_the_owner_answers() {
        with_orchestrator("{}", async |_, runs, hub, dir| {
            let mut new = NewWork {
                title: "Цены".into(),
                text: "сравнить".into(),
                department: "pr".into(),
                ..Default::default()
            };
            new.questions = vec![Question {
                text: "Какие услуги?".into(),
                answer: Some("На свой вкус".into()),
                from: FROM.into(),
            }];
            let task = runs.create(AgentId::from("owner"), new).unwrap();
            let home = Home::load(&dir).unwrap();
            let text = prompt(&home, &hub.tasks(false), &Trigger::Review(task.id));
            assert!(text.contains("## Ответы хозяина\n\n- Какие услуги?\n  Ответ: На свой вкус"));
            assert!(text.contains("Не повторяй вопросы"));
        });
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
            gh: None,
            stall_clock: None,
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
            gh: None,
            stall_clock: None,
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
