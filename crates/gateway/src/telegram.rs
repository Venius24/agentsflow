use crate::{
    hub::{Hub, HubError, NewTask, ResolutionOrigin},
    runtime::Agents,
    team::Team,
};
use agentsflow_events::{AgentId, Envelope, Event, Role, WorkflowStatus};
use anyhow::{Result, bail};
use frankenstein::{AsyncTelegramApi, client_reqwest::Bot};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::Duration,
};
use tokio::sync::{mpsc, watch};

mod screens;

#[derive(Clone)]
pub struct Control {
    pub specs: Vec<crate::workflow::Spec>,
    pub manager: crate::workflow::Manager,
}

#[derive(Clone)]
pub struct Config {
    token: String,
    users: Vec<i64>,
    api: String,
    prompts: bool,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let token = std::env::var("AGENTSFLOW_TELEGRAM_TOKEN").unwrap_or_default();
        let users = std::env::var("AGENTSFLOW_TELEGRAM_USERS").unwrap_or_default();
        let api = std::env::var("AGENTSFLOW_TELEGRAM_API")
            .unwrap_or_else(|_| "https://api.telegram.org".into());
        let mut config = Self::parse(token, &users, api)?;
        config.prompts = std::env::var("AGENTSFLOW_TELEGRAM_PROMPTS").as_deref() != Ok("0");
        Ok(config)
    }

    fn parse(token: String, users: &str, api: String) -> Result<Self> {
        if token.trim().is_empty() {
            bail!("set AGENTSFLOW_TELEGRAM_TOKEN");
        }
        let mut seen = HashSet::new();
        let mut parsed = Vec::new();
        for part in users.split(',') {
            let id = part.trim().parse::<i64>().ok().filter(|id| *id > 0)
                .ok_or_else(|| anyhow::anyhow!("set AGENTSFLOW_TELEGRAM_USERS to numeric user IDs; send a message to @userinfobot to find your ID"))?;
            if seen.insert(id) {
                parsed.push(id);
            }
        }
        let users = parsed;
        if users.is_empty() {
            bail!(
                "set AGENTSFLOW_TELEGRAM_USERS to numeric user IDs; send a message to @userinfobot to find your ID"
            );
        }
        Ok(Self {
            token,
            users,
            api,
            prompts: true,
        })
    }

    fn bot(&self) -> Bot {
        Bot::new_url(format!(
            "{}/bot{}",
            self.api.trim_end_matches('/'),
            self.token
        ))
    }
}

fn message_text(text: &str, token: &str, limit: usize) -> String {
    let source = redact(text, token);
    let mut result = String::new();
    for c in source.chars() {
        let encoded = match c {
            '&' => "&amp;",
            '<' => "&lt;",
            '>' => "&gt;",
            '"' => "&quot;",
            _ => "",
        };
        let size = if encoded.is_empty() {
            1
        } else {
            encoded.chars().count()
        };
        if result.chars().count() + size > limit {
            break;
        }
        if encoded.is_empty() {
            result.push(c);
        } else {
            result.push_str(encoded);
        }
    }
    result
}

fn redact(text: &str, token: &str) -> String {
    let mut source = text.replace(token, "[redacted]");
    while let Some(start) = source.to_ascii_lowercase().find("token=") {
        let end = source[start..]
            .find(|c: char| c.is_whitespace() || c == '&')
            .map_or(source.len(), |i| start + i);
        source.replace_range(start..end, "[redacted]");
    }
    source
}

struct TemporaryCommand(std::path::PathBuf);
impl Drop for TemporaryCommand {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn menu_keyboard() -> Value {
    json!({"keyboard":[[{"text":"👥 Команда"},{"text":"🔐 Запросы"}],[{"text":"📋 Доска"},{"text":"✉ Почта"}],[{"text":"⚙ Ещё"}]],"resize_keyboard":true,"is_persistent":true})
}

fn bot_commands() -> Value {
    json!({"commands":[
        {"command":"team","description":"Команда"},
        {"command":"requests","description":"Запросы на разрешение"},
        {"command":"board","description":"Доска задач"},
        {"command":"task","description":"Новая задача"},
        {"command":"mail","description":"Почта"},
        {"command":"ask","description":"Написать агенту"},
        {"command":"agent","description":"Карточка агента"},
        {"command":"stop","description":"Остановить агента"},
        {"command":"run","description":"Запустить workflow"},
        {"command":"stats","description":"Статистика"},
        {"command":"log","description":"Что делал агент"},
        {"command":"export","description":"Экспорт переписки"},
        {"command":"mute","description":"Тишина"},
        {"command":"unmute","description":"Включить звук"},
        {"command":"status","description":"Состояние команды"},
        {"command":"more","description":"Другие действия"},
        {"command":"help","description":"Помощь"}
    ]})
}

fn format_time(timestamp_ms: u64, now_ms: u64) -> String {
    let seconds = timestamp_ms / 1000;
    let days = seconds / 86_400;
    let time = format!("{:02}:{:02}", seconds % 86_400 / 3600, seconds % 3600 / 60);
    let today = now_ms / 86_400_000;
    if days == today {
        return time;
    }
    if days.checked_add(1) == Some(today) {
        return format!("вчера {time}");
    }
    let z = days as i64 + 719_468;
    let era = z / 146_097;
    let day = z - era * 146_097;
    let year = (day - day / 1460 + day / 36_524 - day / 146_096) / 365;
    let day_of_year = day - (365 * year + year / 4 - year / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day_of_month = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = month_index + if month_index < 10 { 3 } else { -9 };
    format!("{day_of_month:02}.{month:02} {time}")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn resolution_label(origin: Option<ResolutionOrigin>, allow: bool, rule: &str) -> String {
    match origin {
        Some(ResolutionOrigin::Interrupt) => "⏹ Ход остановлен".into(),
        Some(ResolutionOrigin::Guard) => format!("🛡 Отклонено guard: {rule}"),
        Some(ResolutionOrigin::Response) if allow => "✅ Разрешено на странице".into(),
        Some(ResolutionOrigin::Response) => "⛔ Отклонено на странице".into(),
        _ => "Запрос закрыт".into(),
    }
}

fn own_decision(allow: bool) -> &'static str {
    if allow {
        "✅ Разрешено вами"
    } else {
        "⛔ Отклонено вами"
    }
}

#[cfg(test)]
async fn send(bot: &Bot, chat: i64, text: &str) -> std::result::Result<Value, frankenstein::Error> {
    bot.request(
        "sendMessage",
        Some(json!({"chat_id":chat,"text":text,"parse_mode":"HTML"})),
    )
    .await
}

#[derive(Clone)]
struct Outgoing {
    text: String,
    key: Option<u64>,
    critical: bool,
    buttons: Option<Value>,
}
struct Pending {
    agent: AgentId,
    id: String,
    messages: HashMap<i64, Vec<i64>>,
    detail: String,
    guarded: bool,
    confirming: HashSet<i64>,
    text: String,
}
struct Screen {
    key: u64,
    message: i64,
    agent: Option<AgentId>,
    rendered: String,
    refresh: bool,
    requests: bool,
    updated: tokio::time::Instant,
}
struct Draft {
    agent: AgentId,
    text: Option<String>,
    expires: tokio::time::Instant,
    key: u64,
    message: Option<i64>,
}
struct Bridge {
    hub: Hub,
    config: Config,
    bot: Bot,
    agents: Agents,
    team: Team,
    pending: HashMap<u64, Pending>,
    guard_notes: HashMap<(AgentId, String), (String, String)>,
    queues: HashMap<i64, VecDeque<Outgoing>>,
    dropped: HashMap<i64, usize>,
    last_sent: HashMap<i64, tokio::time::Instant>,
    retry_at: HashMap<i64, tokio::time::Instant>,
    failures: HashMap<i64, u32>,
    started: HashMap<AgentId, tokio::time::Instant>,
    next_key: u64,
    rejected: u64,
    screens: HashMap<i64, Screen>,
    recent: HashMap<AgentId, VecDeque<String>>,
    errors: HashSet<AgentId>,
    drafts: HashMap<i64, Draft>,
    bypass_confirmed: HashSet<AgentId>,
    muted: HashSet<i64>,
    inputs: HashMap<i64, screens::Input>,
    wf: Option<screens::WorkflowView>,
    wf_live: HashMap<i64, (i64, String)>,
    control: Option<Control>,
    mail_seen: HashSet<String>,
    exported: bool,
    task_seen: HashMap<String, agentsflow_events::TaskStatus>,
}

impl Bridge {
    #[cfg(test)]
    fn new(config: Config, agents: Agents, team: Team) -> Self {
        Self::with_hub(config, agents, team, Hub::new())
    }

    fn with_hub(config: Config, agents: Agents, team: Team, hub: Hub) -> Self {
        let task_seen = hub
            .tasks(true)
            .into_iter()
            .map(|task| (task.id, task.status))
            .collect();
        Self {
            hub,
            bot: config.bot(),
            config,
            agents,
            team,
            pending: HashMap::new(),
            guard_notes: HashMap::new(),
            queues: HashMap::new(),
            dropped: HashMap::new(),
            last_sent: HashMap::new(),
            retry_at: HashMap::new(),
            failures: HashMap::new(),
            started: HashMap::new(),
            next_key: 0,
            rejected: 0,
            screens: HashMap::new(),
            recent: HashMap::new(),
            errors: HashSet::new(),
            drafts: HashMap::new(),
            bypass_confirmed: HashSet::new(),
            muted: HashSet::new(),
            inputs: HashMap::new(),
            wf: None,
            wf_live: HashMap::new(),
            control: None,
            mail_seen: HashSet::new(),
            exported: false,
            task_seen,
        }
    }
    fn queue(&mut self, chat: i64, text: &str, key: Option<u64>) {
        let text = message_text(text, &self.config.token, 3500);
        self.queue_html(chat, text, key);
    }
    fn queue_html(&mut self, chat: i64, text: String, key: Option<u64>) {
        self.queue_item(chat, text, key, key.is_some());
    }
    fn queue_item(&mut self, chat: i64, text: String, key: Option<u64>, critical: bool) {
        if !self.config.users.contains(&chat) {
            return;
        }
        let queue = self.queues.entry(chat).or_default();
        if queue.len() >= 20 {
            if let Some(index) = queue.iter().position(|item| !item.critical) {
                queue.remove(index);
                *self.dropped.entry(chat).or_default() += 1;
            } else if !critical {
                *self.dropped.entry(chat).or_default() += 1;
                return;
            }
        }
        queue.push_back(Outgoing {
            text,
            key,
            critical,
            buttons: None,
        });
    }
    fn broadcast(&mut self, text: &str, key: Option<u64>) {
        for chat in self.config.users.clone() {
            if key.is_some() || !self.muted.contains(&chat) {
                self.queue(chat, text, key);
            }
        }
    }
    fn broadcast_buttons(&mut self, text: &str, buttons: Value) {
        for chat in self.config.users.clone() {
            if !self.muted.contains(&chat) {
                self.queue_html(chat, text.to_owned(), None);
                if let Some(item) = self.queues.get_mut(&chat).and_then(VecDeque::back_mut) {
                    item.buttons = Some(buttons.clone());
                }
            }
        }
    }
    fn broadcast_html(&mut self, text: &str) {
        for chat in self.config.users.clone() {
            if !self.muted.contains(&chat) {
                self.queue_html(chat, text.to_owned(), None);
            }
        }
    }
    fn handle_event(&mut self, envelope: Envelope) -> Vec<(i64, i64, String)> {
        let agent = envelope.agent;
        let mut edits = Vec::new();
        self.track_workflow(&envelope.event);
        match envelope.event {
            Event::Message {
                role: Role::Assistant,
                text,
                ..
            } => {
                let recent = self.recent.entry(agent).or_default();
                recent.push_back(text);
                while recent.len() > 3 {
                    recent.pop_front();
                }
            }
            Event::Message {
                role: Role::User, ..
            } => {
                self.errors.remove(&agent);
                self.started.insert(agent, tokio::time::Instant::now());
            }
            Event::PermissionRequest {
                id,
                tool,
                input,
                guard,
            } => {
                let command = input.get("command").and_then(Value::as_str).unwrap_or("");
                if let Some(note) = &guard {
                    self.guard_notes.insert(
                        (agent.clone(), id.clone()),
                        (format!("{} - {}", note.rule, note.reason), command.into()),
                    );
                }
                if !self
                    .agents
                    .get(&agent)
                    .is_some_and(|a| a.state().pending.contains(&id))
                {
                    return edits;
                }
                if self
                    .pending
                    .values()
                    .any(|p| p.agent == agent && p.id == id)
                {
                    return edits;
                }
                self.next_key = self.next_key.wrapping_add(1);
                let key = self.next_key;
                let detail: String = if command.is_empty() {
                    input.to_string()
                } else {
                    command.to_owned()
                };
                let short = message_text(
                    &redact(&detail, &self.config.token)
                        .chars()
                        .take(600)
                        .collect::<String>(),
                    &self.config.token,
                    usize::MAX,
                );
                let safe_agent = message_text(agent.as_str(), &self.config.token, 100);
                let text = if let Some(note) = &guard {
                    format!(
                        "⚠️ <b>Опасная команда</b> · {safe_agent}\nПравило: <code>{}</code> - {}\n<pre>{short}</pre>",
                        message_text(&note.rule, &self.config.token, 100),
                        message_text(&note.reason, &self.config.token, 500)
                    )
                } else if !command.is_empty() {
                    format!(
                        "🔐 <b>{safe_agent}</b> просит разрешение\nКоманда в терминале:\n<pre>{short}</pre>"
                    )
                } else {
                    format!(
                        "🔐 <b>{safe_agent}</b> просит разрешение\n{}: <code>{short}</code>",
                        message_text(&tool, &self.config.token, 100)
                    )
                };
                self.pending.insert(
                    key,
                    Pending {
                        agent: agent.clone(),
                        id,
                        messages: HashMap::new(),
                        detail,
                        guarded: guard.is_some(),
                        confirming: HashSet::new(),
                        text: text.clone(),
                    },
                );
                for chat in self.config.users.clone() {
                    self.queue_html(chat, text.clone(), Some(key));
                }
            }
            Event::PermissionResolved {
                id,
                allow,
                blocked_by_guard,
            } => {
                let note = self.guard_notes.remove(&(agent.clone(), id.clone()));
                if let (true, Some((reason, command))) = (blocked_by_guard, note.as_ref()) {
                    let agent = message_text(agent.as_str(), &self.config.token, 100);
                    let reason = message_text(reason, &self.config.token, 500);
                    let command = message_text(command, &self.config.token, 600);
                    for chat in self.config.users.clone() {
                        self.queue_item(chat, format!("⚠️ guard остановил <b>{agent}</b>: <code>{command}</code> ({reason})"), None, true);
                    }
                }
                let keys: Vec<_> = self
                    .pending
                    .iter()
                    .filter(|(_, p)| p.agent == agent && p.id == id)
                    .map(|(key, _)| *key)
                    .collect();
                for key in keys {
                    if let Some(pending) = self.pending.remove(&key) {
                        let label = resolution_label(
                            self.hub.resolution_origin(envelope.seq),
                            allow,
                            note.as_ref()
                                .map_or("", |(rule, _)| rule.split(" - ").next().unwrap_or("")),
                        );
                        for (chat, messages) in pending.messages {
                            for message in messages {
                                edits.push((
                                    chat,
                                    message,
                                    format!(
                                        "{} · {} · {}: {}",
                                        label,
                                        format_time(now_ms(), now_ms()),
                                        agent,
                                        pending.detail.chars().take(100).collect::<String>()
                                    ),
                                ));
                            }
                        }
                    }
                }
            }
            Event::Mail {
                from,
                to,
                subject,
                body,
                thread,
                ..
            } if to == AgentId::orchestrator() => {
                let from = message_text(from.as_str(), &self.config.token, 100);
                let subject = message_text(&subject, &self.config.token, 200);
                let body = message_text(&body, &self.config.token, 400);
                self.broadcast_buttons(
                    &format!("✉ <b>{from}</b> → вам: <i>{subject}</i>\n{body}"),
                    json!({"inline_keyboard":[[{"text":"Открыть письмо","callback_data":format!("s:mt:{thread}:0")}]]}),
                );
            }
            Event::Task { task } => {
                let previous = self.task_seen.insert(task.id.clone(), task.status.clone());
                if previous.as_ref() != Some(&task.status) {
                    let who = message_text(
                        task.assignee
                            .as_ref()
                            .map_or("исполнитель", AgentId::as_str),
                        &self.config.token,
                        100,
                    );
                    let title = message_text(&task.title, &self.config.token, 200);
                    let open = json!({"text":"Открыть","callback_data":format!("s:t:{}", task.id)});
                    match task.status {
                        agentsflow_events::TaskStatus::Review => self.broadcast_buttons(
                            &format!("👀 <b>{who}</b> сдал задачу «{title}» на проверку"),
                            json!({"inline_keyboard":[[open,{"text":"✅ Принять","callback_data":format!("s:ta:{}:acc", task.id)}]]}),
                        ),
                        agentsflow_events::TaskStatus::Blocked => self.broadcast_buttons(
                            &format!("🧱 <b>{who}</b>: задача «{title}» застряла"),
                            json!({"inline_keyboard":[[open]]}),
                        ),
                        _ => {}
                    }
                }
            }
            Event::Workflow {
                name,
                status,
                detail,
                ..
            } if matches!(status, WorkflowStatus::Finished | WorkflowStatus::Failed) => {
                let text = match status {
                    WorkflowStatus::Finished => format!("🔁 Workflow «{name}»: завершён"),
                    _ if detail == "stopped" => format!("⏹ Workflow «{name}»: остановлен"),
                    _ => format!("❌ Workflow «{name}»: с ошибкой: {detail}"),
                };
                self.broadcast(&text, None)
            }
            Event::Done { reason } => {
                let keys: Vec<_> = self
                    .pending
                    .iter()
                    .filter(|(_, pending)| pending.agent == agent)
                    .map(|(key, _)| *key)
                    .collect();
                for key in keys {
                    if let Some(pending) = self.pending.remove(&key) {
                        for (chat, messages) in pending.messages {
                            for message in messages {
                                edits.push((
                                    chat,
                                    message,
                                    if reason == "interrupted" {
                                        "⏹ Ход остановлен".into()
                                    } else {
                                        "Запрос закрыт".into()
                                    },
                                ));
                            }
                        }
                    }
                }
                if reason == "error" {
                    self.errors.insert(agent.clone());
                }
                let elapsed = self.started.remove(&agent).map(|t| t.elapsed().as_secs());
                if matches!(reason.as_str(), "error" | "interrupted") {
                    let agent = message_text(agent.as_str(), &self.config.token, 100);
                    self.broadcast_html(&format!("❌ <b>{agent}</b>: ход завершился ошибкой"));
                } else if let Some(secs) = elapsed.filter(|secs| *secs >= 60) {
                    let agent = message_text(agent.as_str(), &self.config.token, 100);
                    self.broadcast_html(&format!(
                        "✔ <b>{agent}</b> закончил за {} мин {} с",
                        secs / 60,
                        secs % 60
                    ));
                }
            }
            _ => {}
        }
        edits
    }
    async fn flush(&mut self) {
        for chat in self.config.users.clone() {
            let now = tokio::time::Instant::now();
            if self
                .last_sent
                .get(&chat)
                .is_some_and(|last| now.duration_since(*last) < Duration::from_secs(1))
                || self.retry_at.get(&chat).is_some_and(|next| *next > now)
            {
                continue;
            }
            let Some(item) = self.queues.get_mut(&chat).and_then(VecDeque::pop_front) else {
                continue;
            };
            if item.key.is_some_and(|key| !self.pending.contains_key(&key)) {
                continue;
            }
            let mut request = json!({"chat_id":chat,"text":item.text,"parse_mode":"HTML"});
            if let Some(key) = item.key {
                if let Some(pending) = self.pending.get(&key) {
                    let mut rows = if pending.guarded {
                        vec![vec![
                            json!({"text":"⛔ Отклонить","callback_data":format!("perm:d:{key}")}),
                            json!({"text":"Разрешить всё равно…","callback_data":format!("perm:c:{key}")}),
                        ]]
                    } else {
                        vec![vec![
                            json!({"text":"✅ Разрешить","callback_data":format!("perm:a:{key}")}),
                            json!({"text":"⛔ Отклонить","callback_data":format!("perm:d:{key}")}),
                        ]]
                    };
                    if pending.detail.chars().count() > 600 {
                        rows.push(vec![
                            json!({"text":"Показать полностью","callback_data":format!("perm:f:{key}")}),
                        ]);
                    }
                    request["reply_markup"] = json!({"inline_keyboard":rows});
                }
            } else if let Some(buttons) = &item.buttons {
                request["reply_markup"] = buttons.clone();
            } else {
                request["reply_markup"] = menu_keyboard();
            }
            let result: Result<Value, _> = self.bot.request("sendMessage", Some(request)).await;
            match result {
                Ok(value) if value.get("ok").and_then(Value::as_bool) == Some(true) => {
                    self.last_sent.insert(chat, now);
                    self.failures.remove(&chat);
                    self.retry_at.remove(&chat);
                    if let (Some(key), Some(message)) = (
                        item.key,
                        value.pointer("/result/message_id").and_then(Value::as_i64),
                    ) {
                        if let Some(pending) = self.pending.get_mut(&key) {
                            pending.messages.entry(chat).or_default().push(message);
                        }
                    }
                    if let Some(count) = self.dropped.remove(&chat).filter(|count| *count > 0) {
                        self.queue(chat, &format!("… ещё {count} событий"), None);
                    }
                }
                error => {
                    if !retryable(&error) {
                        continue;
                    }
                    let attempt = self.failures.entry(chat).or_default();
                    *attempt = (*attempt + 1).min(7);
                    let delay =
                        retry_after(&error).unwrap_or_else(|| (1_u64 << (*attempt - 1)).min(60));
                    self.retry_at.insert(chat, now + Duration::from_secs(delay));
                    self.queues.entry(chat).or_default().push_front(item);
                }
            }
        }
    }
    async fn edit(&self, edits: Vec<(i64, i64, String)>) {
        for (chat, message, text) in edits {
            self.retry_request("editMessageText", json!({"chat_id":chat,"message_id":message,"text":message_text(&text, &self.config.token, 3500),"reply_markup":{"inline_keyboard":[]}})).await;
        }
    }
    async fn answer(&self, id: &str, text: &str) {
        self.retry_request(
            "answerCallbackQuery",
            json!({"callback_query_id":id,"text":text}),
        )
        .await;
    }
    async fn retry_request(&self, method: &str, params: Value) {
        let mut delay = 1;
        loop {
            let result: Result<Value, _> = self.bot.request(method, Some(params.clone())).await;
            if !retryable(&result) {
                return;
            }
            tokio::time::sleep(Duration::from_secs(retry_after(&result).unwrap_or(delay))).await;
            delay = (delay * 2).min(60);
        }
    }
    fn screen_content(&self, agent: Option<&AgentId>, key: u64) -> (String, Value) {
        let safe = |s: &str| message_text(s, &self.config.token, 300);
        let status = |id: &AgentId| {
            let state = self.agents.get(id).map(|a| a.state()).unwrap_or_default();
            if !state.pending.is_empty() {
                "🟡 ждёт вас"
            } else if state.busy {
                "🟢 работает"
            } else if self.errors.contains(id) {
                "🔴 ошибка"
            } else {
                "⚪ свободен"
            }
        };
        let mode = |m: &crate::team::Member| match m.mode {
            Some(crate::team::AccessMode::Auto) => "⚡ авто",
            Some(crate::team::AccessMode::Smart) => "🧠 умный",
            Some(crate::team::AccessMode::Bypass) => "<b>⚠ BYPASS</b> (без запросов и песочницы)",
            _ => "✋ спрашивает",
        };
        let task_line = |id: &AgentId| {
            self.hub
                .tasks(false)
                .into_iter()
                .find(|t| {
                    t.assignee.as_ref() == Some(id)
                        && t.status == agentsflow_events::TaskStatus::InProgress
                })
                .map(|t| {
                    let filled = usize::from(t.progress) * 5 / 100;
                    format!(
                        "▸ {} {}{} {}%",
                        safe(&t.title),
                        "▓".repeat(filled),
                        "░".repeat(5 - filled),
                        t.progress
                    )
                })
        };
        if let Some(member) = agent.and_then(|id| self.team.get(id)) {
            let state = self
                .agents
                .get(&member.id)
                .map(|a| a.state())
                .unwrap_or_default();
            let mut text = format!(
                "{} <b>{}</b> - {}\n{:?} · {} · {}\n{} · 🛡 {}",
                status(&member.id),
                safe(member.id.as_str()),
                safe(&member.title),
                member.kind,
                safe(member.model.as_deref().unwrap_or("по умолчанию")),
                safe(member.effort.as_deref().unwrap_or("по умолчанию")),
                mode(member),
                if member.guard {
                    "включена"
                } else {
                    "выключена"
                }
            );
            if let Some(task) = task_line(&member.id) {
                text.push_str(&format!("\n{task}"));
            }
            if let Some(recent) = self.recent.get(&member.id) {
                text.push_str("\n\nПоследнее:");
                for item in recent {
                    text.push_str(&format!("\n{}", safe(item)));
                }
            }
            let mut rows = vec![vec![
                json!({"text":"← Назад","callback_data":format!("ui:team:{key}")}),
            ]];
            if self.config.prompts {
                rows.push(vec![
                    json!({"text":"💬 Написать","callback_data":format!("ui:write:{key}")}),
                ]);
            }
            rows.push(vec![
                json!({"text":"📋 Его задачи","callback_data":format!("ui:tasks:{key}")}),
            ]);
            if state.busy {
                rows.push(vec![
                    json!({"text":"⏹ Стоп","callback_data":format!("ui:stop:{key}")}),
                ]);
            }
            (text, json!({"inline_keyboard":rows}))
        } else {
            let mut text = "<b>👥 Команда</b>".to_owned();
            let mut buttons = Vec::new();
            for (index, member) in self.team.members.iter().enumerate() {
                let state = self
                    .agents
                    .get(&member.id)
                    .map(|a| a.state())
                    .unwrap_or_default();
                text.push_str(&format!(
                    "\n{} <b>{}</b> · {} · {}",
                    status(&member.id),
                    safe(member.id.as_str()),
                    safe(&member.title),
                    mode(member)
                ));
                if state.busy {
                    text.push_str(&format!(
                        "\n{}",
                        task_line(&member.id).unwrap_or_else(|| "▸ работает над запросом".into())
                    ));
                }
                if !state.pending.is_empty() {
                    text.push_str(&format!("\n🟡 ждёт вас: {} запросов", state.pending.len()));
                }
                buttons.push(json!({"text":member.id.as_str(),"callback_data":format!("ui:agent:{key}:{index}")}));
            }
            let mut rows: Vec<Vec<Value>> = buttons.chunks(2).map(|row| row.to_vec()).collect();
            rows.push(vec![
                json!({"text":"🔄 Обновить","callback_data":format!("ui:team:{key}")}),
            ]);
            (text, json!({"inline_keyboard":rows}))
        }
    }
    async fn open_team(&mut self, chat: i64, agent: Option<AgentId>, existing: Option<i64>) {
        self.next_key = self.next_key.wrapping_add(1);
        let key = self.next_key;
        let (text, keyboard) = self.screen_content(agent.as_ref(), key);
        let method = if existing.is_some() {
            "editMessageText"
        } else {
            "sendMessage"
        };
        let mut payload =
            json!({"chat_id":chat,"text":text,"parse_mode":"HTML","reply_markup":keyboard});
        if let Some(message) = existing {
            payload["message_id"] = json!(message);
        }
        let response: Result<Value, _> = self.bot.request(method, Some(payload)).await;
        let message = existing.or_else(|| {
            response
                .ok()
                .and_then(|r| r.pointer("/result/message_id").and_then(Value::as_i64))
        });
        if let Some(message) = message {
            self.screens.insert(
                chat,
                Screen {
                    key,
                    message,
                    agent,
                    rendered: text,
                    refresh: true,
                    requests: false,
                    updated: tokio::time::Instant::now(),
                },
            );
        }
    }
    async fn refresh_screens(&mut self) {
        let screens: Vec<_> = self
            .screens
            .iter()
            .filter(|(_, screen)| {
                screen.refresh && screen.updated.elapsed() >= Duration::from_secs(5)
            })
            .map(|(chat, screen)| {
                (
                    *chat,
                    screen.message,
                    screen.key,
                    screen.agent.clone(),
                    screen.rendered.clone(),
                )
            })
            .collect();
        for (chat, message, key, agent, old) in screens {
            let (text, keyboard) = self.screen_content(agent.as_ref(), key);
            if text != old {
                self.retry_request("editMessageText", json!({"chat_id":chat,"message_id":message,"text":text,"parse_mode":"HTML","reply_markup":keyboard})).await;
                if let Some(screen) = self.screens.get_mut(&chat) {
                    screen.rendered = text;
                    screen.updated = tokio::time::Instant::now();
                }
            }
        }
    }
    async fn open_more(&mut self, chat: i64, existing: Option<i64>) {
        self.next_key = self.next_key.wrapping_add(1);
        let key = self.next_key;
        let text = "<b>⚙ Ещё</b>";
        let mute = if self.muted.contains(&chat) {
            "🔔 Звук"
        } else {
            "🔕 Тишина"
        };
        let rows = vec![
            vec![
                json!({"text":"📊 Статистика","callback_data":"s:st"}),
                json!({"text":"⚙ Запуск workflow","callback_data":"s:w"}),
            ],
            vec![
                json!({"text":"📜 Что делал агент","callback_data":"s:lg"}),
                json!({"text":"⬇ Экспорт","callback_data":"s:ex"}),
            ],
            vec![
                json!({"text":mute,"callback_data":format!("ui:mute:{key}")}),
                json!({"text":"ℹ Помощь","callback_data":"s:help"}),
            ],
            vec![json!({"text":"← Назад","callback_data":format!("ui:team:{key}")})],
        ];
        let message = self.show(chat, existing, text.into(), rows).await;
        if let Some(message) = message {
            self.screens.insert(
                chat,
                Screen {
                    key,
                    message,
                    agent: None,
                    rendered: text.into(),
                    refresh: false,
                    requests: false,
                    updated: tokio::time::Instant::now(),
                },
            );
        }
    }
    async fn open_requests(&mut self, chat: i64, existing: Option<i64>) {
        self.next_key = self.next_key.wrapping_add(1);
        let key = self.next_key;
        let mut pending: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, request)| {
                self.agents
                    .get(&request.agent)
                    .is_some_and(|actor| actor.state().pending.contains(&request.id))
            })
            .collect();
        pending.sort_by_key(|(id, _)| **id);
        let mut text = "<b>🔐 Запросы</b>".to_owned();
        let mut rows = Vec::new();
        for (request_key, request) in pending {
            text.push_str(&format!(
                "\n{}: {}",
                message_text(request.agent.as_str(), &self.config.token, 40),
                message_text(&request.detail, &self.config.token, 120)
            ));
            rows.push(vec![json!({"text":format!("Показать: {}", request.agent),"callback_data":format!("ui:request:{key}:{request_key}")})]);
        }
        if rows.is_empty() {
            text = "Ничего не ждёт вашего решения ✔".into();
        }
        rows.insert(
            0,
            vec![json!({"text":"← Назад","callback_data":format!("ui:team:{key}")})],
        );
        rows.push(vec![
            json!({"text":"🔄 Обновить","callback_data":format!("ui:requests:{key}")}),
        ]);
        let mut payload = json!({"chat_id":chat,"text":text,"parse_mode":"HTML","reply_markup":{"inline_keyboard":rows}});
        let method = if let Some(message) = existing {
            payload["message_id"] = json!(message);
            "editMessageText"
        } else {
            "sendMessage"
        };
        let result: Result<Value, _> = self.bot.request(method, Some(payload)).await;
        let message = existing.or_else(|| {
            result
                .ok()
                .and_then(|r| r.pointer("/result/message_id").and_then(Value::as_i64))
        });
        if let Some(message) = message {
            self.screens.insert(
                chat,
                Screen {
                    key,
                    message,
                    agent: None,
                    rendered: text,
                    refresh: false,
                    requests: true,
                    updated: tokio::time::Instant::now(),
                },
            );
        }
    }
    async fn refresh_requests(&mut self) {
        let screens: Vec<_> = self
            .screens
            .iter()
            .filter(|(_, screen)| screen.requests)
            .map(|(chat, screen)| (*chat, screen.message))
            .collect();
        for (chat, message) in screens {
            self.open_requests(chat, Some(message)).await;
        }
    }
    async fn prompt_offer(
        &self,
        chat: i64,
        text: &str,
        buttons: Value,
        message: Option<i64>,
    ) -> Option<i64> {
        let mut body = json!({"chat_id":chat,"text":message_text(text, &self.config.token, 3500),"parse_mode":"HTML","reply_markup":{"inline_keyboard":buttons}});
        let method = if let Some(message) = message {
            body["message_id"] = json!(message);
            "editMessageText"
        } else {
            "sendMessage"
        };
        let result: Result<Value, _> = self.bot.request(method, Some(body)).await;
        message.or_else(|| {
            result
                .ok()
                .and_then(|v| v.pointer("/result/message_id").and_then(Value::as_i64))
        })
    }
    async fn show_full_command(&mut self, chat: i64, command: &str) {
        let clean = redact(command, &self.config.token);
        let escaped = message_text(&clean, &self.config.token, usize::MAX);
        if clean.chars().count() <= 3500 {
            self.queue_html(chat, format!("<pre>{escaped}</pre>"), None);
            return;
        }
        let path =
            std::env::temp_dir().join(format!("agentsflow-command-{}.txt", uuid::Uuid::new_v4()));
        let file = TemporaryCommand(path.clone());
        if std::fs::write(&path, &clean).is_ok() {
            let result: Result<Value, _> = self
                .bot
                .request_with_form_data(
                    "sendDocument",
                    json!({"chat_id":chat}),
                    vec![("document", path)],
                )
                .await;
            if result.is_ok_and(|value| value.get("ok").and_then(Value::as_bool) == Some(true)) {
                return;
            }
        }
        drop(file);
        let mut chunk = String::new();
        for ch in clean.chars() {
            chunk.push(ch);
            if chunk.chars().count() == 500 {
                let text = message_text(&chunk, &self.config.token, 3489);
                self.queue_item(chat, format!("<pre>{text}</pre>"), None, true);
                chunk.clear();
            }
        }
        if !chunk.is_empty() {
            let text = message_text(&chunk, &self.config.token, 3489);
            self.queue_item(chat, format!("<pre>{text}</pre>"), None, true);
        }
    }
    async fn submit_draft(&mut self, chat: i64, stopped: bool, confirmed: bool) {
        if !self.config.prompts {
            let message = self.drafts.remove(&chat).and_then(|draft| draft.message);
            self.draft_feedback(
                chat,
                message,
                "Отправка заданий из Telegram выключена владельцем",
            )
            .await;
            return;
        }
        let Some(draft) = self.drafts.get(&chat) else {
            self.queue(chat, "Задание устарело", None);
            return;
        };
        if draft.expires <= tokio::time::Instant::now() {
            let message = self.drafts.remove(&chat).and_then(|draft| draft.message);
            self.draft_feedback(chat, message, "Время ввода истекло")
                .await;
            return;
        }
        let Some(text) = draft.text.clone() else {
            return;
        };
        let agent = draft.agent.clone();
        let key = draft.key;
        let message = draft.message;
        let Some(actor) = self.agents.get(&agent).cloned() else {
            self.drafts.remove(&chat);
            self.draft_feedback(chat, message, "Нет такого агента")
                .await;
            return;
        };
        let bypass = self
            .team
            .get(&agent)
            .is_some_and(|member| member.mode == Some(crate::team::AccessMode::Bypass));
        if bypass && !self.bypass_confirmed.contains(&agent) && !confirmed {
            let message = self.prompt_offer(chat, &format!("⚠ У {agent} режим BYPASS: его команды выполняются без запросов и без песочницы. Отправить?"), json!([[{"text":"Да, отправить","callback_data":format!("draft:confirm:{key}")},{"text":"Отмена","callback_data":format!("draft:cancel:{key}")} ]]), message).await;
            if let Some(draft) = self.drafts.get_mut(&chat) {
                draft.message = message;
            }
            return;
        }
        if confirmed && bypass {
            self.bypass_confirmed.insert(agent.clone());
        }
        if actor.state().busy && !stopped {
            let message = self.prompt_offer(chat, &format!("{agent} сейчас занят."), json!([[{"text":"⏹ Остановить и отправить","callback_data":format!("draft:stop:{key}")},{"text":"Отмена","callback_data":format!("draft:cancel:{key}")} ]]), message).await;
            if let Some(draft) = self.drafts.get_mut(&chat) {
                draft.message = message;
            }
            return;
        }
        if stopped && actor.state().busy {
            if actor.interrupt().await.is_err() {
                self.draft_feedback(chat, message, "Не удалось остановить агента")
                    .await;
                return;
            }
            let idle = tokio::time::timeout(Duration::from_secs(15), async {
                while actor.state().busy {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
            .await;
            if idle.is_err() {
                self.draft_feedback(chat, message, "Агент не остановился")
                    .await;
                return;
            }
        }
        match actor.prompt(text).await {
            Ok(()) => {
                self.drafts.remove(&chat);
                if let Some(message) = message {
                    self.edit(vec![(chat, message, format!("✔ Передано {agent}"))])
                        .await;
                    self.screens.remove(&chat);
                } else {
                    self.queue(chat, &format!("✔ Передано {agent}"), None);
                }
            }
            Err(_) => {
                self.draft_feedback(chat, message, "Не удалось передать задание")
                    .await
            }
        }
    }
    async fn draft_feedback(&mut self, chat: i64, message: Option<i64>, text: &str) {
        if let Some(message) = message {
            self.edit(vec![(chat, message, text.into())]).await;
            self.screens.remove(&chat);
        } else {
            self.queue(chat, text, None);
        }
    }
    async fn update(&mut self, update: &Value) {
        if let Some(callback) = update.get("callback_query") {
            let id = callback.get("id").and_then(Value::as_str).unwrap_or("");
            let user = callback.pointer("/from/id").and_then(Value::as_i64);
            if !user.is_some_and(|id| self.config.users.contains(&id)) {
                self.rejected += 1;
                return;
            }
            if callback
                .pointer("/message/chat/type")
                .and_then(Value::as_str)
                != Some("private")
                || callback.pointer("/message/chat/id").and_then(Value::as_i64) != user
            {
                self.answer(id, "нет доступа").await;
                return;
            }
            let data = callback.get("data").and_then(Value::as_str).unwrap_or("");
            if data.starts_with("draft:") {
                let chat = user.unwrap_or_default();
                let parts: Vec<_> = data.split(':').collect();
                let key = parts.get(2).and_then(|s| s.parse::<u64>().ok());
                if !self
                    .drafts
                    .get(&chat)
                    .is_some_and(|draft| Some(draft.key) == key)
                {
                    self.answer(id, "Задание устарело").await;
                    return;
                }
                match parts.get(1).copied() {
                    Some("cancel") => {
                        let message = self.drafts.remove(&chat).and_then(|draft| draft.message);
                        if let Some(message) = message {
                            self.open_team(chat, None, Some(message)).await;
                        } else {
                            self.queue(chat, "Отменено", None);
                        }
                    }
                    Some("confirm") => self.submit_draft(chat, false, true).await,
                    Some("stop") => self.submit_draft(chat, true, false).await,
                    _ => {}
                }
                self.answer(id, "").await;
                return;
            }
            if data.starts_with("s:") {
                let chat = user.unwrap_or_default();
                let message = callback
                    .pointer("/message/message_id")
                    .and_then(Value::as_i64);
                self.screens.remove(&chat);
                let flash = self.route(chat, message, data).await;
                self.answer(id, &flash).await;
                return;
            }
            if data.starts_with("ui:") {
                let chat = user.unwrap_or_default();
                self.drafts.remove(&chat);
                self.inputs.remove(&chat);
                let parts: Vec<_> = data.split(':').collect();
                let key = parts.get(2).and_then(|v| v.parse::<u64>().ok());
                let screen = self.screens.get(&chat);
                if !screen.is_some_and(|s| Some(s.key) == key) {
                    self.answer(id, "Экран устарел - открыл заново").await;
                    let message = callback
                        .pointer("/message/message_id")
                        .and_then(Value::as_i64);
                    if let Some(message) = message {
                        self.open_team(chat, None, Some(message)).await;
                    }
                    return;
                }
                let message = screen.map(|s| s.message);
                let active_agent = screen.and_then(|s| s.agent.clone());
                match parts.get(1).copied() {
                    Some("team") => self.open_team(chat, None, message).await,
                    Some("requests") => self.open_requests(chat, message).await,
                    Some("request") => {
                        let request = parts.get(3).and_then(|v| v.parse::<u64>().ok());
                        let card = request.and_then(|request| {
                            self.pending
                                .get(&request)
                                .filter(|pending| {
                                    self.agents.get(&pending.agent).is_some_and(|actor| {
                                        actor.state().pending.contains(&pending.id)
                                    })
                                })
                                .map(|pending| (request, pending.text.clone()))
                        });
                        if let Some((request, text)) = card {
                            self.queue_html(chat, text, Some(request));
                        } else {
                            self.open_requests(chat, message).await;
                        }
                    }
                    Some("mute") => {
                        if !self.muted.remove(&chat) {
                            self.muted.insert(chat);
                        }
                        self.open_more(chat, message).await;
                    }
                    Some("agent") => {
                        let agent = parts
                            .get(3)
                            .and_then(|s| s.parse::<usize>().ok())
                            .and_then(|i| self.team.members.get(i))
                            .map(|m| m.id.clone());
                        self.open_team(chat, agent, message).await;
                    }
                    Some("stop") => {
                        if let Some(actor) = active_agent
                            .as_ref()
                            .and_then(|agent| self.agents.get(agent))
                        {
                            let _ = actor.interrupt().await;
                        }
                        self.open_team(chat, active_agent, message).await;
                    }
                    Some("write") if self.config.prompts => {
                        if let Some(agent) = active_agent {
                            self.inputs.remove(&chat);
                            self.next_key = self.next_key.wrapping_add(1);
                            let key = self.next_key;
                            self.drafts.insert(
                                chat,
                                Draft {
                                    agent: agent.clone(),
                                    text: None,
                                    expires: tokio::time::Instant::now() + Duration::from_secs(600),
                                    key,
                                    message,
                                },
                            );
                            self.prompt_offer(chat, &format!("Что передать {agent}? Следующее сообщение уйдёт ему как задание."), json!([[{"text":"Отмена","callback_data":format!("draft:cancel:{key}")} ]]), message).await;
                            if let Some(screen) = self.screens.get_mut(&chat) {
                                screen.refresh = false;
                            }
                        }
                    }
                    Some("tasks") => {
                        if let (Some(agent), Some(message)) = (active_agent, message) {
                            if let Some(screen) = self.screens.get_mut(&chat) {
                                screen.refresh = false;
                            }
                            let tasks = self
                                .hub
                                .tasks(false)
                                .into_iter()
                                .filter(|task| task.assignee.as_ref() == Some(&agent))
                                .map(|task| {
                                    format!(
                                        "{} · {:?} · {}%",
                                        task.title, task.status, task.progress
                                    )
                                })
                                .collect::<Vec<_>>();
                            let text = if tasks.is_empty() {
                                "Нет задач".to_owned()
                            } else {
                                tasks.join("\n")
                            };
                            let text = message_text(&text, &self.config.token, 3500);
                            let member_index = self
                                .team
                                .members
                                .iter()
                                .position(|member| member.id == agent)
                                .unwrap_or(0);
                            self.retry_request("editMessageText", json!({"chat_id":chat,"message_id":message,"text":text,"reply_markup":{"inline_keyboard":[[{"text":"← Назад","callback_data":format!("ui:agent:{}:{member_index}", key.unwrap_or_default())}]]}})).await;
                        }
                    }
                    _ => {}
                }
                self.answer(id, "").await;
                return;
            }
            let parsed = data
                .strip_prefix("perm:")
                .and_then(|value| value.split_once(':'))
                .and_then(|(action, key)| key.parse::<u64>().ok().map(|key| (action, key)))
                .or_else(|| {
                    data.get(..1)
                        .zip(data.get(1..).and_then(|s| s.parse::<u64>().ok()))
                });
            let Some((action, key)) = parsed else {
                self.answer(id, "запрос недоступен").await;
                return;
            };
            if !matches!(action, "a" | "d" | "c" | "n" | "f") {
                self.answer(id, "запрос недоступен").await;
                return;
            }
            let Some(pending) = self.pending.get(&key) else {
                self.answer(id, "уже решено").await;
                return;
            };
            if action == "f" {
                let detail = pending.detail.clone();
                self.show_full_command(user.unwrap_or_default(), &detail)
                    .await;
                self.answer(id, "").await;
                return;
            }
            if action == "c" && pending.guarded {
                let chat = user.unwrap_or_default();
                let message = callback
                    .pointer("/message/message_id")
                    .and_then(Value::as_i64)
                    .or_else(|| {
                        pending
                            .messages
                            .get(&chat)
                            .and_then(|messages| messages.last().copied())
                    });
                if let Some(pending) = self.pending.get_mut(&key) {
                    pending.confirming.insert(chat);
                }
                if let Some(message) = message {
                    self.retry_request("editMessageText", json!({"chat_id":chat,"message_id":message,"text":"Точно разрешить опасную команду?","reply_markup":{"inline_keyboard":[[{"text":"✅ Да, разрешить","callback_data":format!("perm:a:{key}")},{"text":"← Нет","callback_data":format!("perm:n:{key}")}]]}})).await;
                }
                self.answer(id, "").await;
                return;
            }
            if action == "n" {
                let chat = user.unwrap_or_default();
                let clicked = callback
                    .pointer("/message/message_id")
                    .and_then(Value::as_i64);
                let data = if let Some(pending) = self.pending.get_mut(&key) {
                    pending.confirming.remove(&chat);
                    clicked
                        .or_else(|| {
                            pending
                                .messages
                                .get(&chat)
                                .and_then(|messages| messages.last().copied())
                        })
                        .map(|message| {
                            (
                                message,
                                pending.text.clone(),
                                pending.detail.chars().count() > 600,
                            )
                        })
                } else {
                    None
                };
                if let Some((message, text, long)) = data {
                    let mut rows = vec![vec![
                        json!({"text":"⛔ Отклонить","callback_data":format!("perm:d:{key}")}),
                        json!({"text":"Разрешить всё равно…","callback_data":format!("perm:c:{key}")}),
                    ]];
                    if long {
                        rows.push(vec![json!({"text":"Показать полностью","callback_data":format!("perm:f:{key}")})]);
                    }
                    self.retry_request("editMessageText", json!({"chat_id":chat,"message_id":message,"text":text,"parse_mode":"HTML","reply_markup":{"inline_keyboard":rows}})).await;
                }
                self.answer(id, "Запрос остаётся открытым").await;
                return;
            }
            if action == "a"
                && pending.guarded
                && !pending.confirming.contains(&user.unwrap_or_default())
            {
                self.answer(id, "Подтвердите опасную команду").await;
                return;
            }
            let allow = action == "a";
            let result = match self.agents.get(&pending.agent) {
                Some(actor) => actor.respond(pending.id.clone(), allow).await,
                None => Err(HubError::NotFound),
            };
            if result.is_ok() {
                self.answer(
                    id,
                    if allow {
                        "разрешено"
                    } else {
                        "отклонено"
                    },
                )
                .await;
                if let Some(pending) = self.pending.remove(&key) {
                    let label = format!(
                        "{} · {} · {}: {}",
                        own_decision(allow),
                        format_time(now_ms(), now_ms()),
                        pending.agent,
                        pending.detail.chars().take(100).collect::<String>()
                    );
                    self.edit(
                        pending
                            .messages
                            .into_iter()
                            .flat_map(|(chat, messages)| {
                                let label = label.clone();
                                messages
                                    .into_iter()
                                    .map(move |message| (chat, message, label.clone()))
                            })
                            .collect(),
                    )
                    .await;
                }
            } else {
                self.answer(id, "уже решено").await;
            }
        } else if let Some(message) = update.get("message") {
            let user = message.pointer("/from/id").and_then(Value::as_i64);
            if !user.is_some_and(|id| self.config.users.contains(&id)) {
                self.rejected += 1;
                return;
            }
            if message.pointer("/chat/type").and_then(Value::as_str) != Some("private")
                || message.pointer("/chat/id").and_then(Value::as_i64) != user
            {
                return;
            }
            let chat = user.unwrap_or_default();
            let raw = message.get("text").and_then(Value::as_str).unwrap_or("");
            let is_menu = raw.starts_with('/')
                || matches!(
                    raw,
                    "👥 Команда" | "🔐 Запросы" | "📋 Доска" | "✉ Почта" | "⚙ Ещё"
                );
            if !is_menu && self.inputs.contains_key(&chat) {
                self.process_input(chat, raw).await;
                return;
            }
            if !is_menu && self.drafts.contains_key(&chat) {
                let text = raw.trim();
                if text.is_empty() || text.chars().count() > 8000 {
                    self.queue(chat, "Введите текст задания длиной до 8000 символов", None);
                    return;
                }
                if let Some(draft) = self.drafts.get_mut(&chat) {
                    draft.text = Some(text.to_owned());
                }
                self.submit_draft(chat, false, false).await;
                return;
            }
            if is_menu {
                self.drafts.remove(&chat);
                self.inputs.remove(&chat);
            }
            if self.handle_command(chat, raw).await {
                return;
            }
            if let Some(args) = raw.strip_prefix("/ask ") {
                if !self.config.prompts {
                    self.queue(
                        chat,
                        "Отправка заданий из Telegram выключена владельцем",
                        None,
                    );
                    return;
                }
                let Some((agent, text)) = args.trim().split_once(char::is_whitespace) else {
                    self.queue(chat, "Укажите имя агента и текст задания", None);
                    return;
                };
                let text = text.trim();
                if text.is_empty() || text.chars().count() > 8000 {
                    self.queue(chat, "Введите текст задания длиной до 8000 символов", None);
                    return;
                }
                self.next_key = self.next_key.wrapping_add(1);
                self.drafts.insert(
                    chat,
                    Draft {
                        agent: AgentId::from(agent),
                        text: Some(text.to_owned()),
                        expires: tokio::time::Instant::now() + Duration::from_secs(600),
                        key: self.next_key,
                        message: None,
                    },
                );
                self.submit_draft(chat, false, false).await;
                return;
            }
            if matches!(raw, "/team" | "/status" | "👥 Команда") {
                self.open_team(chat, None, None).await;
                return;
            }
            if matches!(raw, "/more" | "⚙ Ещё") {
                self.open_more(chat, None).await;
                return;
            }
            if matches!(raw, "/requests" | "🔐 Запросы") {
                self.open_requests(chat, None).await;
                return;
            }
            if matches!(raw, "/agent") {
                self.open_team(chat, None, None).await;
                return;
            }
            if let Some(id) = raw.strip_prefix("/agent ") {
                let agent = AgentId::from(id.trim());
                if self.team.get(&agent).is_some() {
                    self.open_team(chat, Some(agent), None).await;
                } else {
                    self.queue(chat, "Нет такого агента", None);
                }
                return;
            }
            let mut words = message
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .split_whitespace();
            let command = match raw {
                "📋 Доска" => "/board",
                "✉ Почта" => "/mail",
                "🔐 Запросы" => "/requests",
                "⚙ Ещё" => "/more",
                _ => words.next().unwrap_or("").split('@').next().unwrap_or(""),
            };
            let answer = match command {
                "/start" | "/help" => screens::help_text(),
                "/ask" => "Укажите агента и задание: /ask <имя> <текст>".to_owned(),
                "/mute" => {
                    self.muted.insert(chat);
                    "Тишина включена".to_owned()
                }
                "/unmute" => {
                    self.muted.remove(&chat);
                    "Звук включён".to_owned()
                }
                "/requests" => {
                    if self.pending.is_empty() {
                        "Ничего не ждёт вашего решения ✔".to_owned()
                    } else {
                        let list = self
                            .pending
                            .values()
                            .map(|pending| {
                                format!(
                                    "{}: {}",
                                    pending.agent,
                                    pending.detail.chars().take(120).collect::<String>()
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        format!("Ожидают вашего решения (карточки присланы выше):\n{list}")
                    }
                }
                "/status" => self
                    .team
                    .members
                    .iter()
                    .map(|m| {
                        let state = self
                            .agents
                            .get(&m.id)
                            .map(|a| a.state())
                            .unwrap_or_default();
                        let mode = match m.mode {
                            Some(crate::team::AccessMode::Ask) => "ask",
                            Some(crate::team::AccessMode::Auto) => "auto",
                            Some(crate::team::AccessMode::Smart) => "smart",
                            Some(crate::team::AccessMode::Bypass) => "bypass",
                            None => "ask",
                        };
                        format!(
                            "{} {} {} pending: {}",
                            m.id,
                            mode,
                            if state.busy { "busy" } else { "idle" },
                            if state.pending.is_empty() {
                                "-".into()
                            } else {
                                state.pending.join(", ")
                            }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                "/stop" => match words
                    .next()
                    .and_then(|id| self.agents.get(&AgentId::from(id)))
                {
                    Some(actor) => match actor.interrupt().await {
                        Ok(()) => "остановлено",
                        Err(HubError::NotBusy) => "не занят",
                        Err(_) => "не удалось остановить",
                    }
                    .into(),
                    None => "нет такого агента".into(),
                },
                _ => "Используйте /help".into(),
            };
            self.queue(chat, &answer, None);
        }
    }
}

fn retry_after(result: &Result<Value, frankenstein::Error>) -> Option<u64> {
    match result {
        Err(frankenstein::Error::Api(error)) => error
            .parameters
            .as_ref()
            .and_then(|p| p.retry_after)
            .map(u64::from),
        Ok(value) if value.get("ok").and_then(Value::as_bool) == Some(false) => value
            .pointer("/parameters/retry_after")
            .and_then(Value::as_u64),
        _ => None,
    }
}

fn retryable(result: &Result<Value, frankenstein::Error>) -> bool {
    match result {
        Ok(value) => {
            value.get("ok").and_then(Value::as_bool) == Some(false)
                && value
                    .get("error_code")
                    .and_then(Value::as_u64)
                    .is_some_and(|code| code == 429 || code >= 500)
        }
        Err(frankenstein::Error::Api(error)) => error.error_code == 429 || error.error_code >= 500,
        Err(_) => true,
    }
}

async fn poll(config: Config, tx: mpsc::UnboundedSender<Value>, mut stop: watch::Receiver<bool>) {
    let bot = config.bot();
    let mut offset = 0_i64;
    let mut delay = 1_u64;
    let mut connected = false;
    loop {
        let result: Result<Value, _> = tokio::select! {
            biased;
            _ = stop.wait_for(|v| *v) => break,
            result = async {
                if connected { bot.request("getUpdates", Some(json!({"offset":offset,"timeout":30,"allowed_updates":["message","callback_query"]}))).await }
                else { bot.request::<Value, Value>("getMe", None).await }
            } => result,
        };
        if let Ok(value) = &result {
            if value.get("ok").and_then(Value::as_bool) == Some(true) {
                delay = 1;
                if !connected {
                    let username: String = value
                        .pointer("/result/username")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .chars()
                        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                        .take(32)
                        .collect();
                    let username = if username.contains(&config.token) {
                        "redacted"
                    } else {
                        username.as_str()
                    };
                    eprintln!(
                        "telegram: connected as @{}, {} user(s)",
                        username,
                        config.users.len()
                    );
                    connected = true;
                    let _: Result<Value, _> =
                        bot.request("setMyCommands", Some(bot_commands())).await;
                } else if let Some(updates) = value.get("result").and_then(Value::as_array) {
                    for update in updates {
                        if let Some(id) = update.get("update_id").and_then(Value::as_i64) {
                            offset = offset.max(id.saturating_add(1));
                        }
                        if tx.send(update.clone()).is_err() {
                            return;
                        }
                    }
                    if updates.is_empty() {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
                continue;
            }
        }
        if !connected {
            eprintln!("telegram: connection failed; retrying");
        }
        let wait = retry_after(&result).unwrap_or(delay);
        tokio::select! { biased; _ = stop.wait_for(|v| *v) => break, _ = tokio::time::sleep(Duration::from_secs(wait)) => {} }
        delay = (delay * 2).min(60);
    }
}

pub async fn run(
    config: Config,
    hub: Hub,
    agents: Agents,
    team: Team,
    control: Option<Control>,
    mut stop: watch::Receiver<bool>,
) {
    let (history, mut events) = hub.subscribe();
    let mut bridge = Bridge::with_hub(config.clone(), agents, team, hub.clone());
    bridge.control = control;
    for event in history {
        if matches!(
            event.event,
            Event::PermissionRequest { .. }
                | Event::Message {
                    role: Role::Assistant,
                    ..
                }
        ) {
            bridge.handle_event(event);
        }
    }
    let (tx, mut updates) = mpsc::unbounded_channel();
    let polling = tokio::task::spawn_local(poll(config, tx, stop.clone()));
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut stats = tokio::time::interval(Duration::from_secs(60));
    let mut screens = tokio::time::interval(Duration::from_secs(5));
    let mut operation_stop = stop.clone();
    loop {
        tokio::select! {
                   biased;
                   _ = stop.wait_for(|v| *v) => break,
                   event = events.recv() => match event {
                       Ok(event) => { let refresh = matches!(event.event, Event::PermissionRequest { .. } | Event::PermissionResolved { .. } | Event::Done { .. }); let workflow = matches!(event.event, Event::Workflow { .. }); let edits = bridge.handle_event(event); tokio::select! { biased; _ = operation_stop.wait_for(|v| *v) => break, _ = async { bridge.edit(edits).await; if refresh { bridge.refresh_requests().await; }
        if workflow { bridge.refresh_workflow().await; } } => {} } }
                       Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                           let (history, receiver) = hub.subscribe(); events = receiver;
                           for event in &history {
                               if matches!(event.event, Event::PermissionResolved { .. }) {
                                   let edits = bridge.handle_event(event.clone());
                                   tokio::select! { biased; _ = operation_stop.wait_for(|v| *v) => break, _ = bridge.edit(edits) => {} }
                               }
                           }
                           for event in history { if matches!(event.event, Event::PermissionRequest { .. }) { bridge.handle_event(event); } }
                       }
                       Err(_) => break,
                   },
                   Some(update) = updates.recv() => tokio::select! { biased; _ = operation_stop.wait_for(|v| *v) => break, _ = bridge.update(&update) => {} },
                   _ = tick.tick() => tokio::select! { biased; _ = operation_stop.wait_for(|v| *v) => break, _ = bridge.flush() => {} },
                   _ = stats.tick() => if bridge.rejected > 0 { eprintln!("telegram: ignored {} unauthorized updates", bridge.rejected); bridge.rejected = 0; },
                   _ = screens.tick() => tokio::select! { biased; _ = operation_stop.wait_for(|v| *v) => break, _ = async { bridge.refresh_screens().await; bridge.refresh_workflow().await; } => {} },
               }
    }
    polling.abort();
    let _ = polling.await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentsflow_adapters::Agent;
    use axum::{
        Json, Router,
        extract::{Path, State},
        routing::post,
    };
    use std::sync::{Arc, Mutex};
    type RequestLog = Arc<Mutex<Vec<(String, Value)>>>;
    type RawRequestLog = Arc<Mutex<Vec<(String, Vec<u8>)>>>;
    type AttemptLog = (Arc<std::sync::atomic::AtomicUsize>, Arc<Mutex<Vec<Value>>>);

    struct FakeAgent {
        request: bool,
        answers: Arc<Mutex<Vec<bool>>>,
    }
    impl Agent for FakeAgent {
        async fn send(&mut self, _: &str) -> Result<()> {
            Ok(())
        }
        async fn next_events(&mut self) -> Result<Vec<Event>> {
            if self.request {
                self.request = false;
                Ok(vec![Event::PermissionRequest {
                    id: "p".into(),
                    tool: "Bash".into(),
                    input: json!({"command":"echo hi"}),
                    guard: None,
                }])
            } else {
                Ok(vec![Event::Done {
                    reason: "success".into(),
                }])
            }
        }
        async fn respond(&mut self, _: &str, allow: bool) -> Result<()> {
            self.answers.lock().unwrap().push(allow);
            Ok(())
        }
        async fn shutdown(self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn whitelist_and_text() {
        assert_eq!(own_decision(true), "✅ Разрешено вами");
        assert_eq!(own_decision(false), "⛔ Отклонено вами");
        assert_eq!(
            resolution_label(Some(ResolutionOrigin::Response), true, ""),
            "✅ Разрешено на странице"
        );
        assert_eq!(
            resolution_label(Some(ResolutionOrigin::Response), false, ""),
            "⛔ Отклонено на странице"
        );
        assert_eq!(
            resolution_label(Some(ResolutionOrigin::Guard), false, "rm"),
            "🛡 Отклонено guard: rm"
        );
        assert_eq!(
            resolution_label(Some(ResolutionOrigin::Interrupt), false, ""),
            "⏹ Ход остановлен"
        );
        assert_eq!(resolution_label(None, false, ""), "Запрос закрыт");
        assert_eq!(format_time(1_700_000_000_000, 1_700_000_000_000), "22:13");
        assert_eq!(
            format_time(1_700_000_000_000, 1_700_086_400_000),
            "вчера 22:13"
        );
        assert_eq!(format_time(0, 3 * 86_400_000), "01.01 00:00");
        assert_eq!(menu_keyboard()["is_persistent"], true);
        assert!(
            bot_commands()["commands"]
                .as_array()
                .unwrap()
                .iter()
                .any(|command| command["command"] == "ask")
        );
        assert!(Config::parse("secret".into(), "", "http://localhost".into()).is_err());
        assert!(Config::parse("".into(), "1", "http://localhost".into()).is_err());
        assert!(Config::parse("secret".into(), "1,invalid", "http://localhost".into()).is_err());
        assert_eq!(
            retry_after(&Ok(
                json!({"ok":false,"error_code":429,"parameters":{"retry_after":120}})
            )),
            Some(120)
        );
        assert_eq!(
            Config::parse("secret".into(), "1, 2,1", "http://localhost".into())
                .unwrap()
                .users,
            [1, 2]
        );
        assert!(!message_text("url?TOKEN=private", "secret", 3500).contains("private"));
        assert_eq!(
            message_text("<a>& secret ?token=abc", "secret", 3500),
            "&lt;a&gt;&amp; [redacted] ?[redacted]"
        );
        assert!(
            message_text(&"<".repeat(5000), "secret", 3500)
                .chars()
                .count()
                <= 3500
        );
    }

    #[test]
    fn queues_compact_only_non_permission_notifications() {
        let config = Config::parse("secret".into(), "11", "http://localhost".into()).unwrap();
        let mut bridge = Bridge::new(config, Agents::new(), Team::preset("pair").unwrap());
        for _ in 0..25 {
            bridge.queue(11, "event", None);
        }
        assert_eq!(bridge.queues[&11].len(), 20);
        assert_eq!(bridge.dropped[&11], 5);
        bridge.queue(11, "permission", Some(1));
        assert_eq!(bridge.queues[&11].len(), 20);
        assert_eq!(bridge.dropped[&11], 6);
        assert!(bridge.queues[&11].iter().any(|item| item.key == Some(1)));
        bridge.queue(99, "unauthorized", None);
        assert!(!bridge.queues.contains_key(&99));
    }

    #[test]
    fn mute_suppresses_ordinary_events_but_not_guard() {
        let config = Config::parse("secret".into(), "11", "http://localhost".into()).unwrap();
        let mut bridge = Bridge::new(config, Agents::new(), Team::preset("pair").unwrap());
        bridge.muted.insert(11);
        bridge.broadcast("ordinary", None);
        assert!(!bridge.queues.contains_key(&11));
        let make = |event| Envelope {
            session: "s".into(),
            agent: AgentId::from("claude"),
            seq: 1,
            ts_ms: 1,
            event,
        };
        bridge.handle_event(make(Event::PermissionRequest {
            id: "p".into(),
            tool: "shell".into(),
            input: json!({"command":"rm -rf /"}),
            guard: Some(agentsflow_events::GuardNote {
                rule: "rm-recursive".into(),
                reason: "danger".into(),
            }),
        }));
        bridge.handle_event(make(Event::PermissionResolved {
            id: "p".into(),
            allow: false,
            blocked_by_guard: true,
        }));
        assert!(bridge.queues[&11][0].text.contains("guard"));
        assert!(bridge.queues[&11][0].critical);
    }

    #[tokio::test]
    async fn navigation_requests_and_full_command() {
        use axum::body::Bytes;
        let calls = Arc::new(Mutex::new(Vec::<(String, Vec<u8>)>::new()));
        let app = Router::new()
            .route(
                "/{path}/{method}",
                post(
                    |Path((path, method)): Path<(String, String)>,
                     State(calls): State<RawRequestLog>,
                     body: Bytes| async move {
                        calls
                            .lock()
                            .unwrap()
                            .push((format!("{path}/{method}"), body.to_vec()));
                        Json(json!({"ok":true,"result":{"message_id":42}}))
                    },
                ),
            )
            .with_state(calls.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config = Config::parse("secret".into(), "11", format!("http://{address}")).unwrap();
        let mut bridge = Bridge::new(config, Agents::new(), Team::preset("pair").unwrap());
        let msg = |text| json!({"message":{"from":{"id":11},"chat":{"id":11,"type":"private"},"text":text}});
        let cb = |data: String, user| json!({"callback_query":{"id":"cb","from":{"id":user},"data":data,"message":{"message_id":42,"chat":{"id":11,"type":"private"}}}});
        bridge.update(&msg("/requests")).await;
        assert!(
            String::from_utf8_lossy(&calls.lock().unwrap().last().unwrap().1)
                .contains("Ничего не ждёт")
        );
        bridge.update(&msg("/team")).await;
        let key = bridge.screens[&11].key;
        let before = calls.lock().unwrap().len();
        bridge.update(&cb(format!("ui:agent:{key}:0"), 11)).await;
        assert!(
            calls.lock().unwrap()[before..]
                .iter()
                .any(|(path, _)| path.ends_with("/editMessageText"))
        );
        let key = bridge.screens[&11].key;
        bridge.update(&cb(format!("ui:team:{key}"), 11)).await;
        let before = calls.lock().unwrap().len();
        bridge.update(&cb(format!("ui:agent:{key}:0"), 99)).await;
        assert_eq!(calls.lock().unwrap().len(), before);
        bridge.update(&cb(format!("ui:agent:{key}:0"), 11)).await;
        assert!(
            calls.lock().unwrap()[before..]
                .iter()
                .any(|(path, body)| path.ends_with("/answerCallbackQuery")
                    && String::from_utf8_lossy(body).contains("устарел"))
        );
        let short = format!("{} <end>", "a".repeat(650));
        bridge.show_full_command(11, &short).await;
        assert!(
            bridge.queues[&11]
                .back()
                .unwrap()
                .text
                .contains("&lt;end&gt;</pre>")
        );
        bridge.show_full_command(11, &"x".repeat(4000)).await;
        assert!(calls.lock().unwrap().iter().any(|(path, body)| {
            path == "botsecret/sendDocument"
                && body
                    .windows(4000)
                    .any(|chunk| chunk == "x".repeat(4000).as_bytes())
        }));
        server.abort();
    }

    #[tokio::test]
    async fn requests_busy_prompt_and_interrupt_resolution() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        tokio::task::LocalSet::new().run_until(async {
            let calls = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
            let app = Router::new().route("/{path}/{method}", post(|Path((path, method)): Path<(String,String)>, State(calls): State<RequestLog>, Json(body): Json<Value>| async move {
                calls.lock().unwrap().push((format!("{path}/{method}"), body));
                Json(json!({"ok":true,"result":{"message_id":42}}))
            })).with_state(calls.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let hub = Hub::new();
            let (_, mut rx) = hub.subscribe();
            let interrupts = Arc::new(AtomicUsize::new(0));
            let (actor, task) = crate::hub::spawn(hub.clone(), AgentId::from("claude"), false, {
                let interrupts = interrupts.clone(); move || { let interrupts = interrupts.clone(); async move { Ok(crate::testing::Silent::new(1, true).probes(Arc::default(), interrupts)) } }
            });
            let config = Config::parse("secret".into(), "11", format!("http://{address}")).unwrap();
            let mut bridge = Bridge::with_hub(config, Agents::from([(AgentId::from("claude"), actor.clone())]), Team::preset("pair").unwrap(), hub);
            let msg = |text| json!({"message":{"from":{"id":11},"chat":{"id":11,"type":"private"},"text":text}});
            let cb = |data: String| json!({"callback_query":{"id":"cb","from":{"id":11},"data":data,"message":{"message_id":42,"chat":{"id":11,"type":"private"}}}});
            actor.prompt("first".into()).await.unwrap();
            loop { let e = rx.recv().await.unwrap(); if matches!(e.event, Event::PermissionRequest { .. }) { bridge.handle_event(e); break; } }
            bridge.flush().await;
            bridge.update(&msg("/requests")).await;
            let key = bridge.screens[&11].key;
            assert!(calls.lock().unwrap().iter().any(|(_, value)| value["reply_markup"]["inline_keyboard"].to_string().contains("Показать")));
            bridge.update(&cb(format!("ui:request:{key}:1"))).await;
            bridge.last_sent.clear();
            bridge.flush().await;
            assert_eq!(bridge.pending[&1].messages[&11].len(), 2);
            bridge.update(&msg("/ask claude second")).await;
            let draft = bridge.drafts[&11].key;
            bridge.update(&cb(format!("draft:cancel:{draft}"))).await;
            assert!(actor.state().busy);
            assert_eq!(interrupts.load(Ordering::SeqCst), 0);
            bridge.update(&msg("/ask claude second")).await;
            let draft = bridge.drafts[&11].key;
            bridge.update(&cb(format!("draft:stop:{draft}"))).await;
            assert_eq!(interrupts.load(Ordering::SeqCst), 1);
            let resolved = loop { let e = rx.recv().await.unwrap(); if matches!(e.event, Event::PermissionResolved { .. }) { break e; } };
            let edits = bridge.handle_event(resolved);
            assert_eq!(edits.len(), 2);
            assert!(edits.iter().all(|(_, _, text)| text.contains("Ход остановлен")));
            actor.shutdown().await;
            task.await.unwrap();
            server.abort();
        }).await;
    }

    #[tokio::test]
    async fn dangerous_permission_needs_confirmation() {
        tokio::task::LocalSet::new().run_until(async {
            let app = Router::new().route("/{path}/{method}", post(|| async { Json(json!({"ok":true,"result":{"message_id":42}})) }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let hub = Hub::new();
            let (_, mut rx) = hub.subscribe();
            let answers = Arc::new(Mutex::new(Vec::new()));
            let (actor, task) = crate::hub::spawn(hub.clone(), AgentId::from("claude"), false, {
                let answers = answers.clone(); move || { let answers = answers.clone(); async move { Ok(crate::testing::Silent::new(1, true).probes(answers, Arc::default())) } }
            });
            let config = Config::parse("secret".into(), "11", format!("http://{address}")).unwrap();
            let mut bridge = Bridge::new(config, Agents::from([(AgentId::from("claude"), actor.clone())]), Team::preset("pair").unwrap());
            actor.prompt("work".into()).await.unwrap();
            let mut event = loop { let e = rx.recv().await.unwrap(); if matches!(e.event, Event::PermissionRequest { .. }) { break e; } };
            if let Event::PermissionRequest { guard, input, .. } = &mut event.event { *guard = Some(agentsflow_events::GuardNote { rule: "test".into(), reason: "danger".into() }); *input = json!({"command":"rm -rf /"}); }
            bridge.handle_event(event);
            bridge.flush().await;
            let callback = |data| json!({"callback_query":{"id":"cb","from":{"id":11},"data":data,"message":{"chat":{"id":11,"type":"private"}}}});
            bridge.update(&callback("a1")).await;
            bridge.update(&callback("c1")).await;
            assert!(answers.lock().unwrap().is_empty());
            bridge.update(&callback("a1")).await;
            assert_eq!(*answers.lock().unwrap(), [true]);
            actor.interrupt().await.unwrap();
            actor.shutdown().await;
            task.await.unwrap();
            server.abort();
        }).await;
    }

    #[tokio::test]
    async fn prompt_requires_bypass_confirmation_and_honors_switch() {
        tokio::task::LocalSet::new().run_until(async {
            let app = Router::new().route("/{path}/{method}", post(|| async { Json(json!({"ok":true,"result":{"message_id":42}})) }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let hub = Hub::new();
            let (actor, task) = crate::hub::spawn(hub.clone(), AgentId::from("claude"), false, || async { Ok(crate::testing::Silent::new(1, false)) });
            let config = Config::parse("secret".into(), "11", format!("http://{address}")).unwrap();
            let mut team = Team::preset("pair").unwrap();
            team.members[0].mode = Some(crate::team::AccessMode::Bypass);
            let mut bridge = Bridge::with_hub(config, Agents::from([(AgentId::from("claude"), actor.clone())]), team, hub);
            let ask = json!({"message":{"from":{"id":11},"chat":{"id":11,"type":"private"},"text":"/ask claude hello"}});
            bridge.config.prompts = false;
            bridge.update(&ask).await;
            assert!(!actor.state().busy);
            bridge.config.prompts = true;
            bridge.update(&ask).await;
            assert!(!actor.state().busy);
            let key = bridge.drafts[&11].key;
            bridge.update(&json!({"callback_query":{"id":"cb","from":{"id":11},"data":format!("draft:confirm:{key}"),"message":{"chat":{"id":11,"type":"private"}}}})).await;
            assert!(actor.state().busy);
            assert!(bridge.bypass_confirmed.contains(&AgentId::from("claude")));
            actor.interrupt().await.unwrap();
            actor.shutdown().await;
            task.await.unwrap();
            server.abort();
        }).await;
    }

    #[tokio::test]
    async fn draft_expires_and_menu_cancels_it() {
        let config = Config::parse("secret".into(), "11", "http://localhost".into()).unwrap();
        let mut bridge = Bridge::new(config, Agents::new(), Team::preset("pair").unwrap());
        bridge.drafts.insert(
            11,
            Draft {
                agent: AgentId::from("claude"),
                text: None,
                expires: tokio::time::Instant::now() - Duration::from_secs(1),
                key: 1,
                message: None,
            },
        );
        bridge.update(&json!({"message":{"from":{"id":11},"chat":{"id":11,"type":"private"},"text":"hello"}})).await;
        assert!(!bridge.drafts.contains_key(&11));
        assert!(bridge.queues[&11].back().unwrap().text.contains("истекло"));
        bridge.drafts.insert(
            11,
            Draft {
                agent: AgentId::from("claude"),
                text: None,
                expires: tokio::time::Instant::now() + Duration::from_secs(60),
                key: 2,
                message: None,
            },
        );
        bridge.update(&json!({"message":{"from":{"id":11},"chat":{"id":11,"type":"private"},"text":"/board"}})).await;
        assert!(!bridge.drafts.contains_key(&11));
    }

    #[tokio::test]
    async fn sends_only_to_requested_user_on_fake_api() {
        let received = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
        let app = Router::new().route("/{path}/{method}", post(|Path((path, method)): Path<(String, String)>, State(received): State<RequestLog>, Json(body): Json<Value>| async move {
            received.lock().unwrap().push((format!("{path}/{method}"), body));
            Json(json!({"ok":true,"result":{"message_id":1,"date":1,"chat":{"id":1,"type":"private"}}}))
        })).with_state(received.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config =
            Config::parse("test-secret".into(), "1,2", format!("http://{address}")).unwrap();
        let text = message_text("hello <world>", &config.token, 3500);
        send(&config.bot(), config.users[0], &text).await.unwrap();
        let requests = received.lock().unwrap();
        assert_eq!(requests[0].0, "bottest-secret/sendMessage");
        assert_eq!(requests[0].1["chat_id"], 1);
        assert_eq!(requests[0].1["text"], "hello &lt;world&gt;");
        assert!(!requests[0].1.to_string().contains("test-secret"));
        drop(requests);
        server.abort();
    }

    #[tokio::test]
    async fn permission_callbacks_and_unauthorized_updates() {
        tokio::task::LocalSet::new().run_until(async {
            let received = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
            let app = Router::new().route("/{path}/{method}", post(|Path((path, method)): Path<(String,String)>, State(received): State<RequestLog>, Json(body): Json<Value>| async move {
                received.lock().unwrap().push((format!("{path}/{method}"), body));
                Json(json!({"ok":true,"result":{"message_id":42}}))
            })).with_state(received.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let hub = Hub::new();
            let (_, mut rx) = hub.subscribe();
            let answers = Arc::new(Mutex::new(Vec::new()));
            let (actor, task) = crate::hub::spawn(hub.clone(), AgentId::from("claude"), false, {
                let answers = answers.clone(); move || { let answers = answers.clone(); async move { Ok(FakeAgent { request: true, answers }) } }
            });
            let config = Config::parse("test-secret".into(), "11,22", format!("http://{address}")).unwrap();
            let agents = Agents::from([(AgentId::from("claude"), actor.clone())]);
            let mut bridge = Bridge::with_hub(config, agents, Team::preset("pair").unwrap(), hub.clone());
            actor.prompt("task".into()).await.unwrap();
            loop { let event = rx.recv().await.unwrap(); if matches!(event.event, Event::PermissionRequest { .. }) { bridge.handle_event(event); break; } }
            bridge.flush().await;
            {
                let requests = received.lock().unwrap();
                let sent: Vec<_> = requests.iter().filter(|(path, _)| path.ends_with("/sendMessage")).collect();
                assert_eq!(sent.len(), 2);
                assert!(sent.iter().all(|(path, body)| path.starts_with("bottest-secret/") && body["reply_markup"]["inline_keyboard"][0][0]["callback_data"] == "perm:a:1" && !body.to_string().contains("test-secret")));
            }
            bridge.update(&json!({"message":{"from":{"id":99},"chat":{"id":99,"type":"private"},"text":"/status"}})).await;
            bridge.update(&json!({"callback_query":{"id":"bad","from":{"id":99},"data":"a1","message":{"chat":{"id":99,"type":"private"}}}})).await;
            assert!(answers.lock().unwrap().is_empty());
            bridge.update(&json!({"callback_query":{"id":"ok","from":{"id":11},"data":"a1","message":{"chat":{"id":11,"type":"private"}}}})).await;
            assert_eq!(*answers.lock().unwrap(), [true]);
            assert_eq!(received.lock().unwrap().iter().filter(|(path, _)| path.ends_with("/editMessageText")).count(), 2);
            assert_eq!(received.lock().unwrap().iter().filter(|(path, _)| path.ends_with("/sendMessage")).count(), 2);
            bridge.update(&json!({"message":{"from":{"id":11},"chat":{"id":11,"type":"private"},"text":"/status"}})).await;
            assert!(received.lock().unwrap().iter().any(|(_, body)| body["text"].as_str().is_some_and(|text| text.contains("claude"))));
            bridge.update(&json!({"message":{"from":{"id":11},"chat":{"id":11,"type":"private"},"text":"/stop unknown"}})).await;
            assert_eq!(bridge.queues[&11].back().unwrap().text, "нет такого агента");
            bridge.queues.clear();
            bridge.last_sent.clear();
            let (other, other_task) = crate::hub::spawn(hub.clone(), AgentId::from("codex"), false, || async { Ok(FakeAgent { request: true, answers: Arc::default() }) });
            bridge.agents.insert(AgentId::from("codex"), other.clone());
            other.prompt("task".into()).await.unwrap();
            loop { let event = rx.recv().await.unwrap(); if event.agent == AgentId::from("codex") && matches!(event.event, Event::PermissionRequest { .. }) { bridge.handle_event(event); break; } }
            bridge.flush().await;
            other.respond("p".into(), false).await.unwrap();
            let edits = loop { let event = rx.recv().await.unwrap(); if event.agent == AgentId::from("codex") && matches!(event.event, Event::PermissionResolved { .. }) { break bridge.handle_event(event); } };
            assert!(!bridge.pending.contains_key(&2));
            assert!(edits[0].2.contains("Отклонено на странице"));
            bridge.edit(edits).await;
            other.shutdown().await;
            other_task.await.unwrap();
            actor.shutdown().await;
            task.await.unwrap();
            server.abort();
        }).await;
    }

    #[tokio::test]
    async fn retries_messages_and_formats_guard_mail_and_commands() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let attempts = Arc::new(AtomicUsize::new(0));
        let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
        let app = Router::new()
            .route(
                "/{path}/{method}",
                post(
                    |Path((path, _method)): Path<(String, String)>,
                     State((attempts, captured)): State<AttemptLog>,
                     Json(body): Json<Value>| async move {
                        assert_eq!(path, "bottest-secret");
                        captured.lock().unwrap().push(body);
                        if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                            (
                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                Json(json!({"ok":false,"error_code":500,"description":"failed"})),
                            )
                        } else {
                            (
                                axum::http::StatusCode::OK,
                                Json(json!({"ok":true,"result":{"message_id":42}})),
                            )
                        }
                    },
                ),
            )
            .with_state((attempts.clone(), captured.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config =
            Config::parse("test-secret".into(), "11", format!("http://{address}")).unwrap();
        let mut bridge = Bridge::new(config, Agents::new(), Team::preset("pair").unwrap());
        let make = |event| Envelope {
            session: "s".into(),
            agent: AgentId::from("claude"),
            seq: 1,
            ts_ms: 1,
            event,
        };
        bridge.handle_event(make(Event::PermissionRequest {
            id: "p".into(),
            tool: "shell".into(),
            input: json!({"command":"rm -rf /"}),
            guard: Some(agentsflow_events::GuardNote {
                rule: "rm-recursive".into(),
                reason: "bad".into(),
            }),
        }));
        bridge.handle_event(make(Event::PermissionResolved {
            id: "p".into(),
            allow: false,
            blocked_by_guard: true,
        }));
        bridge.flush().await;
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(bridge.queues[&11].len(), 1);
        bridge.retry_at.clear();
        bridge.flush().await;
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(
            captured.lock().unwrap()[1]["text"]
                .as_str()
                .unwrap()
                .contains("guard")
        );
        bridge.handle_event(make(Event::Mail {
            id: "i".into(),
            thread: "t".into(),
            from: AgentId::from("claude"),
            to: AgentId::orchestrator(),
            subject: "hello".into(),
            body: "secret test-secret".into(),
        }));
        bridge.last_sent.clear();
        bridge.flush().await;
        assert!(
            captured.lock().unwrap()[2]["text"]
                .as_str()
                .unwrap()
                .contains("[redacted]")
        );
        bridge.update(&json!({"message":{"from":{"id":11},"chat":{"id":11,"type":"private"},"text":"/status"}})).await;
        assert!(captured.lock().unwrap().iter().any(|body| {
            body["text"]
                .as_str()
                .is_some_and(|text| text.contains("claude"))
        }));
        bridge.update(&json!({"message":{"from":{"id":11},"chat":{"id":11,"type":"private"},"text":"/stop missing"}})).await;
        assert!(
            bridge.queues[&11]
                .back()
                .unwrap()
                .text
                .contains("нет такого агента")
        );
        assert!(
            captured
                .lock()
                .unwrap()
                .iter()
                .all(|body| !body.to_string().contains("test-secret"))
        );
        server.abort();
    }

    #[tokio::test]
    async fn stop_command_interrupts_fake_actor() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        tokio::task::LocalSet::new().run_until(async {
            let hub = Hub::new();
            let count = Arc::new(AtomicUsize::new(0));
            let (actor, task) = crate::hub::spawn(hub, AgentId::from("claude"), false, {
                let count = count.clone();
                move || { let count = count.clone(); async move { Ok(crate::testing::Silent::new(1, false).probes(Arc::default(), count)) } }
            });
            let config = Config::parse("test-secret".into(), "11", "http://127.0.0.1:1".into()).unwrap();
            let mut bridge = Bridge::new(config, Agents::from([(AgentId::from("claude"), actor.clone())]), Team::preset("pair").unwrap());
            actor.prompt("start".into()).await.unwrap();
            bridge.update(&json!({"message":{"from":{"id":11},"chat":{"id":11,"type":"private"},"text":"/stop claude"}})).await;
            assert_eq!(bridge.queues[&11].back().unwrap().text, "остановлено");
            assert_eq!(count.load(Ordering::SeqCst), 1);
            tokio::time::timeout(Duration::from_secs(2), async { while actor.state().busy { tokio::task::yield_now().await; } }).await.unwrap();
            bridge.update(&json!({"message":{"from":{"id":11},"chat":{"id":11,"type":"private"},"text":"/stop claude"}})).await;
            assert_eq!(bridge.queues[&11].back().unwrap().text, "не занят");
            actor.shutdown().await;
            task.await.unwrap();
        }).await;
    }

    #[tokio::test]
    async fn poll_retries_get_me_and_receives_updates() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let attempts = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route("/{path}/{method}", post(|Path((path, method)): Path<(String, String)>, State(attempts): State<Arc<AtomicUsize>>| async move {
            assert_eq!(path, "bottest-secret");
            if method == "getMe" && attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                (axum::http::StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"ok":false,"error_code":500,"description":"failed"})))
            } else if method == "getMe" {
                (axum::http::StatusCode::OK, Json(json!({"ok":true,"result":{"username":"test_bot"}})))
            } else {
                (axum::http::StatusCode::OK, Json(json!({"ok":true,"result":[{"update_id":1,"message":{"from":{"id":11},"chat":{"id":11,"type":"private"},"text":"/help"}}]})))
            }
        })).with_state(attempts.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config =
            Config::parse("test-secret".into(), "11", format!("http://{address}")).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (stop, shutdown) = watch::channel(false);
        let task = tokio::spawn(poll(config, tx, shutdown));
        let update = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(update["message"]["from"]["id"], 11);
        assert!(attempts.load(Ordering::SeqCst) >= 2);
        stop.send(true).unwrap();
        task.await.unwrap();
        server.abort();
    }
}
