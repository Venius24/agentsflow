use crate::{
    hub::{Hub, HubError},
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

#[derive(Clone)]
pub struct Config {
    token: String,
    users: Vec<i64>,
    api: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let token = std::env::var("AGENTSFLOW_TELEGRAM_TOKEN").unwrap_or_default();
        let users = std::env::var("AGENTSFLOW_TELEGRAM_USERS").unwrap_or_default();
        let api = std::env::var("AGENTSFLOW_TELEGRAM_API")
            .unwrap_or_else(|_| "https://api.telegram.org".into());
        Self::parse(token, &users, api)
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
        Ok(Self { token, users, api })
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
    let mut source = text.replace(token, "[redacted]");
    while let Some(start) = source.to_ascii_lowercase().find("token=") {
        let end = source[start..]
            .find(|c: char| c.is_whitespace() || c == '&')
            .map_or(source.len(), |i| start + i);
        source.replace_range(start..end, "[redacted]");
    }
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
}
struct Pending {
    agent: AgentId,
    id: String,
    messages: HashMap<i64, i64>,
}
struct Bridge {
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
}

impl Bridge {
    fn new(config: Config, agents: Agents, team: Team) -> Self {
        Self {
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
        }
    }
    fn queue(&mut self, chat: i64, text: &str, key: Option<u64>) {
        if !self.config.users.contains(&chat) {
            return;
        }
        let queue = self.queues.entry(chat).or_default();
        if queue.len() >= 20 {
            if let Some(index) = queue.iter().position(|item| item.key.is_none()) {
                queue.remove(index);
                *self.dropped.entry(chat).or_default() += 1;
            } else if key.is_none() {
                *self.dropped.entry(chat).or_default() += 1;
                return;
            }
        }
        queue.push_back(Outgoing {
            text: message_text(text, &self.config.token, 3500),
            key,
        });
    }
    fn broadcast(&mut self, text: &str, key: Option<u64>) {
        for chat in self.config.users.clone() {
            self.queue(chat, text, key);
        }
    }
    fn handle_event(&mut self, envelope: Envelope) -> Vec<(i64, i64, String)> {
        let agent = envelope.agent;
        let mut edits = Vec::new();
        match envelope.event {
            Event::Message {
                role: Role::User, ..
            } => {
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
                }
                .chars()
                .take(600)
                .collect();
                let warning = guard
                    .map(|n| format!("\nопасно ({}): {}", n.rule, n.reason))
                    .unwrap_or_default();
                self.pending.insert(
                    key,
                    Pending {
                        agent: agent.clone(),
                        id,
                        messages: HashMap::new(),
                    },
                );
                self.broadcast(
                    &format!("🔐 {agent} просит разрешение: {tool} {detail}{warning}"),
                    Some(key),
                );
            }
            Event::PermissionResolved {
                id,
                allow,
                blocked_by_guard,
            } => {
                let note = self.guard_notes.remove(&(agent.clone(), id.clone()));
                if let (true, Some((reason, command))) = (blocked_by_guard, note) {
                    self.broadcast(
                        &format!("🛡 guard отклонил: {agent}: {reason}: {command}"),
                        None,
                    );
                }
                let keys: Vec<_> = self
                    .pending
                    .iter()
                    .filter(|(_, p)| p.agent == agent && p.id == id)
                    .map(|(key, _)| *key)
                    .collect();
                for key in keys {
                    if let Some(pending) = self.pending.remove(&key) {
                        for (chat, message) in pending.messages {
                            edits.push((
                                chat,
                                message,
                                if allow {
                                    "решено на странице: разрешено"
                                } else {
                                    "решено на странице: отклонено"
                                }
                                .into(),
                            ));
                        }
                    }
                }
            }
            Event::Mail {
                from,
                to,
                subject,
                body,
                ..
            } if to == AgentId::orchestrator() => self.broadcast(
                &format!(
                    "✉ {from} -> вы: {subject}\n{}",
                    body.chars().take(600).collect::<String>()
                ),
                None,
            ),
            Event::Workflow {
                name,
                status,
                detail,
                ..
            } if matches!(status, WorkflowStatus::Finished | WorkflowStatus::Failed) => {
                self.broadcast(&format!("workflow {name}: {status:?} {detail}"), None)
            }
            Event::Done { reason } => {
                let elapsed = self.started.remove(&agent).map(|t| t.elapsed().as_secs());
                if matches!(reason.as_str(), "error" | "interrupted") {
                    self.broadcast(&format!("{agent}: {reason}"), None);
                } else if let Some(secs) = elapsed.filter(|secs| *secs >= 60) {
                    self.broadcast(&format!("✔ {agent} закончил ({secs} с)"), None);
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
                request["reply_markup"] = json!({"inline_keyboard":[[{"text":"✅ Разрешить","callback_data":format!("a{key}")},{"text":"⛔ Отклонить","callback_data":format!("d{key}")}]]});
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
                            pending.messages.insert(chat, message);
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
    async fn update(&mut self, update: &Value) {
        if let Some(callback) = update.get("callback_query") {
            let id = callback.get("id").and_then(Value::as_str).unwrap_or("");
            let user = callback.pointer("/from/id").and_then(Value::as_i64);
            if !user.is_some_and(|id| self.config.users.contains(&id)) {
                self.rejected += 1;
                self.answer(id, "нет доступа").await;
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
            let Some((action, key)) = data
                .get(..1)
                .zip(data.get(1..).and_then(|s| s.parse::<u64>().ok()))
            else {
                self.answer(id, "запрос недоступен").await;
                return;
            };
            if !matches!(action, "a" | "d") {
                self.answer(id, "запрос недоступен").await;
                return;
            }
            let Some(pending) = self.pending.get(&key) else {
                self.answer(id, "уже решено").await;
                return;
            };
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
                    self.edit(
                        pending
                            .messages
                            .into_iter()
                            .map(|(chat, message)| {
                                (
                                    chat,
                                    message,
                                    if allow {
                                        "✅ разрешено вами"
                                    } else {
                                        "⛔ отклонено вами"
                                    }
                                    .into(),
                                )
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
            let mut words = message
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .split_whitespace();
            let command = words.next().unwrap_or("").split('@').next().unwrap_or("");
            let answer = match command {
                "/start" | "/help" => {
                    "/status - состояние команды\n/stop <agent> - остановить ход\n/help - помощь"
                        .to_owned()
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
    mut stop: watch::Receiver<bool>,
) {
    let (history, mut events) = hub.subscribe();
    let mut bridge = Bridge::new(config.clone(), agents, team);
    for event in history {
        if matches!(event.event, Event::PermissionRequest { .. }) {
            bridge.handle_event(event);
        }
    }
    let (tx, mut updates) = mpsc::unbounded_channel();
    let polling = tokio::task::spawn_local(poll(config, tx, stop.clone()));
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut stats = tokio::time::interval(Duration::from_secs(60));
    let mut operation_stop = stop.clone();
    loop {
        tokio::select! {
            biased;
            _ = stop.wait_for(|v| *v) => break,
            event = events.recv() => match event {
                Ok(event) => { let edits = bridge.handle_event(event); tokio::select! { biased; _ = operation_stop.wait_for(|v| *v) => break, _ = bridge.edit(edits) => {} } }
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
            let mut bridge = Bridge::new(config, agents, Team::preset("pair").unwrap());
            actor.prompt("task".into()).await.unwrap();
            loop { let event = rx.recv().await.unwrap(); if matches!(event.event, Event::PermissionRequest { .. }) { bridge.handle_event(event); break; } }
            bridge.flush().await;
            {
                let requests = received.lock().unwrap();
                let sent: Vec<_> = requests.iter().filter(|(path, _)| path.ends_with("/sendMessage")).collect();
                assert_eq!(sent.len(), 2);
                assert!(sent.iter().all(|(path, body)| path.starts_with("bottest-secret/") && body["reply_markup"]["inline_keyboard"][0][0]["callback_data"] == "a1" && !body.to_string().contains("test-secret")));
            }
            bridge.update(&json!({"message":{"from":{"id":99},"chat":{"id":99,"type":"private"},"text":"/status"}})).await;
            bridge.update(&json!({"callback_query":{"id":"bad","from":{"id":99},"data":"a1","message":{"chat":{"id":99,"type":"private"}}}})).await;
            assert!(answers.lock().unwrap().is_empty());
            bridge.update(&json!({"callback_query":{"id":"ok","from":{"id":11},"data":"a1","message":{"chat":{"id":11,"type":"private"}}}})).await;
            assert_eq!(*answers.lock().unwrap(), [true]);
            assert_eq!(received.lock().unwrap().iter().filter(|(path, _)| path.ends_with("/editMessageText")).count(), 2);
            assert_eq!(received.lock().unwrap().iter().filter(|(path, _)| path.ends_with("/sendMessage")).count(), 2);
            bridge.update(&json!({"message":{"from":{"id":11},"chat":{"id":11,"type":"private"},"text":"/status"}})).await;
            assert!(bridge.queues[&11].back().unwrap().text.contains("claude"));
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
            assert_eq!(edits[0].2, "решено на странице: отклонено");
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
        assert!(bridge.queues[&11].back().unwrap().text.contains("claude"));
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
