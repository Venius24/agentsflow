//! Экраны Telegram-пульта: доска, почта, workflow, статистика, история, экспорт.
//! Кнопки здесь без состояния: все данные в `callback_data` вида `s:<экран>:<аргументы>`.

use super::*;
use crate::hub::{MailError, MailMsg, TaskError, TaskPatch};
use agentsflow_events::{TaskSnapshot, TaskStatus};

pub(super) const HELP: &str = "Здесь можно следить за командой и управлять ею, как на странице.\n👥 Команда /team - агенты, /agent <имя> - один агент, /ask <имя> <текст> - задание, /stop <имя>.\n🔐 Запросы /requests - решения о разрешениях и опасных командах.\n📋 Доска /board, /task <название> | <описание> - задачи.\n✉ Почта /mail, /mail <агент> <тема> | <текст>.\n⚙ Запуск workflow /run, 📊 /stats, 📜 /log <имя> [n], ⬇ /export md|jsonl.\n/mute и /unmute - тишина, /more - все действия.\nЗапросы на разрешение придут сюда сами. Время в сообщениях: UTC.";

pub(super) fn help_text() -> String {
    format!("{HELP}\nВерсия: {}", env!("CARGO_PKG_VERSION"))
}

const STATUSES: [TaskStatus; 6] = [
    TaskStatus::Backlog,
    TaskStatus::Todo,
    TaskStatus::InProgress,
    TaskStatus::Review,
    TaskStatus::Done,
    TaskStatus::Blocked,
];
const PAGE: usize = 8;
const MAIL_PAGE: usize = 6;

pub(super) fn status_label(status: &TaskStatus) -> &'static str {
    match status {
        TaskStatus::Backlog => "Очередь",
        TaskStatus::Todo => "К выполнению",
        TaskStatus::InProgress => "В работе",
        TaskStatus::Review => "На проверке",
        TaskStatus::Done => "Готово",
        TaskStatus::Blocked => "Застряла",
        TaskStatus::Archived => "Архив",
    }
}
fn short_label(status: &TaskStatus) -> &'static str {
    match status {
        TaskStatus::Review => "Проверка",
        other => status_label(other),
    }
}
pub(super) fn status_icon(status: &TaskStatus) -> &'static str {
    match status {
        TaskStatus::Backlog => "📥",
        TaskStatus::Todo => "📌",
        TaskStatus::InProgress => "⚙",
        TaskStatus::Review => "👀",
        TaskStatus::Done => "✅",
        TaskStatus::Blocked => "🧱",
        TaskStatus::Archived => "🗄",
    }
}
fn bar(progress: u8) -> String {
    let filled = usize::from(progress.min(100)) * 5 / 100;
    format!(
        "{}{} {}%",
        "▓".repeat(filled),
        "░".repeat(5 - filled),
        progress
    )
}
fn task_error_text(error: &TaskError) -> String {
    match error {
        TaskError::NotFound => "Задача не найдена".into(),
        TaskError::Limit => "Слишком много задач на доске".into(),
        TaskError::Forbidden => "Эту задачу нельзя менять".into(),
        TaskError::MailDisabled => "Почта выключена".into(),
        TaskError::NoAssignee => "Нужен исполнитель".into(),
        TaskError::Mail(error) => mail_error_text(error).into(),
        TaskError::Invalid(text) => match *text {
            "title must contain 1 to 200 characters" => "Название: от 1 до 200 символов".into(),
            "description exceeds 8000 characters" => "Описание длиннее 8000 символов".into(),
            _ => format!("Не принято: {text}"),
        },
    }
}
fn mail_error_text(error: &MailError) -> &'static str {
    match error {
        MailError::SameAgent => "Отправитель и получатель совпадают",
        MailError::Subject => "Тема: от 1 до 200 символов",
        MailError::Body => "Текст письма: от 1 символа, не более 64 КБ",
        MailError::UnknownThread => "Такой переписки нет",
        MailError::ThreadLimit => "В этой переписке исчерпан лимит писем",
    }
}
fn button(text: &str, data: impl Into<String>) -> Value {
    json!({"text":text,"callback_data":data.into()})
}
fn event_line(event: &Event) -> Option<String> {
    let clip = |text: &str, n: usize| text.chars().take(n).collect::<String>();
    Some(match event {
        Event::Message {
            role: Role::User,
            text,
            ..
        } => format!("💬 вы: {}", clip(text, 140)),
        Event::Message {
            role: Role::Assistant,
            text,
            delta: false,
        } => format!("🤖 {}", clip(text, 140)),
        Event::ToolCall { name, input, .. } => {
            let what = ["command", "file_path", "path", "pattern"]
                .iter()
                .find_map(|key| input.get(*key).and_then(Value::as_str))
                .unwrap_or("");
            format!("▶ {name}: {}", clip(what, 100))
        }
        Event::ToolResult {
            is_error: true,
            output,
            ..
        } => format!("⚠ ошибка команды: {}", clip(output, 100)),
        Event::Diff { path, .. } => format!("✏ правка файла: {}", clip(path, 100)),
        Event::PermissionRequest { tool, .. } => format!("🔐 запрос разрешения: {tool}"),
        Event::PermissionResolved { allow: true, .. } => "✅ разрешено".into(),
        Event::PermissionResolved { allow: false, .. } => "⛔ отклонено".into(),
        Event::Done { reason } if reason.starts_with("interrupted") => "⏹ ход остановлен".into(),
        Event::Done { reason } if reason == "error" => "❌ ход закончился ошибкой".into(),
        Event::Done { .. } => "✔ ход закончен".into(),
        Event::Error { message } => format!("❌ {}", clip(message, 140)),
        _ => return None,
    })
}

pub(super) enum InputKind {
    NewTask,
    EditTitle(String),
    EditDescription(String),
    MailReply(String),
    MailNew(AgentId),
    WorkflowTask(usize),
    WorkflowConfirm(usize, String),
}
pub(super) struct Input {
    pub kind: InputKind,
    pub expires: tokio::time::Instant,
    pub message: Option<i64>,
}

#[derive(Default)]
pub(super) struct WorkflowView {
    pub name: String,
    pub steps_started: usize,
    pub step: Option<String>,
    pub result: Option<String>,
}

struct Thread {
    id: String,
    subject: String,
    messages: Vec<MailMsg>,
}
impl Thread {
    fn last_seq(&self) -> u64 {
        self.messages.last().map_or(0, |m| m.seq)
    }
    fn participants(&self) -> Vec<AgentId> {
        let mut result: Vec<AgentId> = Vec::new();
        for message in &self.messages {
            for id in [&message.from, &message.to] {
                if !result.contains(id) {
                    result.push(id.clone());
                }
            }
        }
        result
    }
}

impl Bridge {
    fn safe(&self, text: &str, limit: usize) -> String {
        message_text(text, &self.config.token, limit)
    }
    pub(super) async fn show(
        &self,
        chat: i64,
        existing: Option<i64>,
        text: String,
        rows: Vec<Vec<Value>>,
    ) -> Option<i64> {
        let mut body = json!({"chat_id":chat,"text":text,"parse_mode":"HTML","reply_markup":{"inline_keyboard":rows}});
        let method = if let Some(message) = existing {
            body["message_id"] = json!(message);
            "editMessageText"
        } else {
            "sendMessage"
        };
        let result: Result<Value, _> = self.bot.request(method, Some(body)).await;
        existing.or_else(|| {
            result
                .ok()
                .and_then(|v| v.pointer("/result/message_id").and_then(Value::as_i64))
        })
    }
    fn agent_status(&self, id: &AgentId) -> &'static str {
        let state = self.agents.get(id).map(|a| a.state()).unwrap_or_default();
        if state.last_error.is_some() {
            "🔴"
        } else if !state.pending.is_empty() {
            "🟡"
        } else if state.busy {
            "🟢"
        } else {
            "⚪"
        }
    }

    // ---------- доска ----------
    fn board_counts(&self) -> [usize; 6] {
        let mut counts = [0; 6];
        for task in self.hub.tasks(false) {
            if let Some(index) = STATUSES.iter().position(|s| *s == task.status) {
                counts[index] += 1;
            }
        }
        counts
    }
    pub(super) async fn open_board(
        &mut self,
        chat: i64,
        existing: Option<i64>,
        column: usize,
        page: usize,
    ) {
        let counts = self.board_counts();
        let column = if column < 6 {
            column
        } else if counts[2] > 0 {
            2
        } else if counts[3] > 0 {
            3
        } else if counts[1] > 0 {
            1
        } else {
            counts.iter().position(|n| *n > 0).unwrap_or(2)
        };
        let mut items: Vec<TaskSnapshot> = self
            .hub
            .tasks(false)
            .into_iter()
            .filter(|t| t.status == STATUSES[column])
            .collect();
        items.sort_by_key(|t| std::cmp::Reverse(t.updated_ms));
        let pages = items.len().div_ceil(PAGE).max(1);
        let page = page.min(pages - 1);
        let summary = STATUSES
            .iter()
            .zip(counts)
            .map(|(s, n)| format!("{} {n}", short_label(s)))
            .collect::<Vec<_>>()
            .join(" · ");
        let mut text = format!(
            "<b>📋 Доска</b>\n{summary}\n\n<b>{} {}</b> ({})",
            status_icon(&STATUSES[column]),
            status_label(&STATUSES[column]),
            items.len()
        );
        let mut numbers = Vec::new();
        for (offset, task) in items.iter().skip(page * PAGE).take(PAGE).enumerate() {
            let number = page * PAGE + offset + 1;
            text.push_str(&format!(
                "\n{number}. {} <b>{}</b> · {} · {}",
                status_icon(&task.status),
                self.safe(&task.title, 80),
                self.safe(
                    task.assignee
                        .as_ref()
                        .map_or("не назначена", AgentId::as_str),
                    40
                ),
                bar(task.progress)
            ));
            numbers.push(button(&number.to_string(), format!("s:t:{}", task.id)));
        }
        if items.is_empty() {
            text.push_str("\nПусто");
        }
        let mut rows: Vec<Vec<Value>> = STATUSES
            .iter()
            .zip(counts)
            .enumerate()
            .map(|(index, (status, count))| {
                button(
                    &format!(
                        "{}{} {count}",
                        if index == column { "• " } else { "" },
                        short_label(status)
                    ),
                    format!("s:b:{index}:0"),
                )
            })
            .collect::<Vec<_>>()
            .chunks(3)
            .map(<[Value]>::to_vec)
            .collect();
        rows.extend(numbers.chunks(4).map(<[Value]>::to_vec));
        let mut nav = Vec::new();
        if page > 0 {
            nav.push(button("◂ Раньше", format!("s:b:{column}:{}", page - 1)));
        }
        if page + 1 < pages {
            nav.push(button("Дальше ▸", format!("s:b:{column}:{}", page + 1)));
        }
        if !nav.is_empty() {
            rows.push(nav);
        }
        rows.push(vec![
            button("＋ Новая задача", "s:nt"),
            button("🔄 Обновить", format!("s:b:{column}:{page}")),
        ]);
        self.show(chat, existing, text, rows).await;
    }
    fn task_card(&self, task: &TaskSnapshot, flash: &str) -> (String, Vec<Vec<Value>>) {
        let mut text = String::new();
        if !flash.is_empty() {
            text.push_str(&format!("{}\n\n", self.safe(flash, 200)));
        }
        text.push_str(&format!(
            "{} <b>{}</b>\n{}\nИсполнитель: {}\n{}",
            status_icon(&task.status),
            self.safe(&task.title, 200),
            status_label(&task.status),
            self.safe(
                task.assignee
                    .as_ref()
                    .map_or("не назначена", AgentId::as_str),
                40
            ),
            bar(task.progress)
        ));
        if !task.description.is_empty() {
            text.push_str(&format!("\n\n{}", self.safe(&task.description, 600)));
        }
        if !task.note.is_empty() {
            text.push_str(&format!("\nПримечание: {}", self.safe(&task.note, 300)));
        }
        text.push_str(&format!(
            "\nОбновлено {}",
            format_time(task.updated_ms, now_ms())
        ));
        if task.thread.is_some() {
            text.push_str("\nСвязана с перепиской");
        }
        let id = &task.id;
        let mut rows = Vec::new();
        if task.assignee.is_some()
            && matches!(
                task.status,
                TaskStatus::Backlog | TaskStatus::Todo | TaskStatus::Blocked
            )
        {
            rows.push(vec![button("▶ Запустить", format!("s:ta:{id}:run"))]);
        }
        if task.status == TaskStatus::Review {
            rows.push(vec![
                button("✅ Принять", format!("s:ta:{id}:acc")),
                button("↩ Вернуть в работу", format!("s:ta:{id}:ret")),
            ]);
        }
        rows.push(vec![
            button("Статус ▸", format!("s:ta:{id}:sm")),
            button("👤 Исполнитель ▸", format!("s:ta:{id}:am")),
        ]);
        rows.push(vec![
            button("✏ Название", format!("s:ta:{id}:et")),
            button("✏ Описание", format!("s:ta:{id}:ed")),
        ]);
        rows.push(vec![button("🗄 В архив", format!("s:ta:{id}:ar"))]);
        let column = STATUSES.iter().position(|s| *s == task.status).unwrap_or(9);
        rows.push(vec![button("← К доске", format!("s:b:{column}:0"))]);
        (text, rows)
    }
    pub(super) async fn open_task(
        &mut self,
        chat: i64,
        existing: Option<i64>,
        id: &str,
        flash: &str,
    ) {
        match self.hub.task(id) {
            Some(task) => {
                let (text, rows) = self.task_card(&task, flash);
                self.show(chat, existing, text, rows).await;
            }
            None => {
                self.open_board(chat, existing, 9, 0).await;
            }
        }
    }
    async fn task_action(
        &mut self,
        chat: i64,
        message: Option<i64>,
        id: &str,
        action: &str,
    ) -> String {
        let patch = |status| TaskPatch {
            status: Some(status),
            ..TaskPatch::default()
        };
        let result = match action {
            "run" => self.hub.dispatch_task(id).map(|_| "Запущено"),
            "accy" => self
                .hub
                .update_task(AgentId::orchestrator(), id, patch(TaskStatus::Done))
                .map(|_| "Принято"),
            "ret" => self
                .hub
                .update_task(AgentId::orchestrator(), id, patch(TaskStatus::InProgress))
                .map(|_| "Возвращено в работу"),
            "ary" => self
                .hub
                .update_task(AgentId::orchestrator(), id, patch(TaskStatus::Archived))
                .map(|_| "Перенесено в архив"),
            "acc" | "ar" => {
                let (question, yes) = if action == "acc" {
                    ("Принять задачу как выполненную?", "accy")
                } else {
                    ("Перенести задачу в архив?", "ary")
                };
                let title = self.hub.task(id).map(|t| t.title).unwrap_or_default();
                self.show(
                    chat,
                    message,
                    format!("{question}\n<b>{}</b>", self.safe(&title, 200)),
                    vec![vec![
                        button("Да", format!("s:ta:{id}:{yes}")),
                        button("Нет", format!("s:t:{id}")),
                    ]],
                )
                .await;
                return String::new();
            }
            "sm" => {
                let mut rows: Vec<Vec<Value>> = STATUSES
                    .iter()
                    .enumerate()
                    .map(|(index, status)| {
                        button(
                            &format!("{} {}", status_icon(status), status_label(status)),
                            format!("s:ts:{id}:{index}"),
                        )
                    })
                    .collect::<Vec<_>>()
                    .chunks(2)
                    .map(<[Value]>::to_vec)
                    .collect();
                rows.push(vec![button("← Назад", format!("s:t:{id}"))]);
                self.show(chat, message, "Какой статус поставить?".into(), rows)
                    .await;
                return String::new();
            }
            "am" => {
                let mut buttons: Vec<Value> = self
                    .team
                    .members
                    .iter()
                    .enumerate()
                    .map(|(index, member)| button(member.id.as_str(), format!("s:tu:{id}:{index}")))
                    .collect();
                buttons.push(button("не назначена", format!("s:tu:{id}:-")));
                let mut rows: Vec<Vec<Value>> = buttons.chunks(2).map(<[Value]>::to_vec).collect();
                rows.push(vec![button("← Назад", format!("s:t:{id}"))]);
                self.show(chat, message, "Кому поручить задачу?".into(), rows)
                    .await;
                return String::new();
            }
            "et" => {
                self.prompt_input(
                    chat,
                    message,
                    InputKind::EditTitle(id.into()),
                    "Новое название задачи? Следующее сообщение заменит его.",
                    &format!("s:t:{id}"),
                )
                .await;
                return String::new();
            }
            "ed" => {
                self.prompt_input(
                    chat,
                    message,
                    InputKind::EditDescription(id.into()),
                    "Новое описание задачи? Следующее сообщение заменит его.",
                    &format!("s:t:{id}"),
                )
                .await;
                return String::new();
            }
            _ => return String::new(),
        };
        let flash = match result {
            Ok(text) => text.to_owned(),
            Err(error) => task_error_text(&error),
        };
        self.open_task(chat, message, id, &flash).await;
        flash
    }
    async fn prompt_input(
        &mut self,
        chat: i64,
        message: Option<i64>,
        kind: InputKind,
        text: &str,
        back: &str,
    ) {
        let message = self
            .show(
                chat,
                message,
                text.to_owned(),
                vec![vec![button("Отмена", back)]],
            )
            .await;
        self.inputs.insert(
            chat,
            Input {
                kind,
                expires: tokio::time::Instant::now() + Duration::from_secs(600),
                message,
            },
        );
    }
    async fn create_task_from(&mut self, chat: i64, existing: Option<i64>, text: &str) {
        let (title, description) = text
            .split_once('|')
            .map_or((text, ""), |(a, b)| (a.trim(), b.trim()));
        match self.hub.create_task(
            AgentId::orchestrator(),
            NewTask {
                title: title.to_owned(),
                description: description.to_owned(),
                assignee: None,
                status: None,
                labels: Vec::new(),
            },
        ) {
            Ok(task) => {
                let (text, rows) = self.task_card(
                    &task,
                    "✔ Задача создана. Назначьте исполнителя и запустите.",
                );
                self.show(chat, existing, text, rows).await;
            }
            Err(error) => self.queue(chat, &task_error_text(&error), None),
        }
    }

    // ---------- почта ----------
    fn threads(&self) -> Vec<Thread> {
        let mut result: Vec<Thread> = Vec::new();
        for message in self.hub.mail_all() {
            if let Some(thread) = result.iter_mut().find(|t| t.id == message.thread) {
                thread.messages.push(message);
            } else {
                result.push(Thread {
                    id: message.thread.clone(),
                    subject: message.subject.clone(),
                    messages: vec![message],
                });
            }
        }
        result
    }
    fn unseen(&self, thread: &Thread) -> bool {
        thread
            .messages
            .iter()
            .any(|m| m.to == AgentId::orchestrator() && !self.mail_seen.contains(&m.id))
    }
    fn mail_times(&self) -> HashMap<String, u64> {
        let (history, _) = self.hub.subscribe();
        history
            .into_iter()
            .filter_map(|e| match e.event {
                Event::Mail { id, .. } => Some((id, e.ts_ms)),
                _ => None,
            })
            .collect()
    }
    fn route_names(&self, participants: &[AgentId]) -> String {
        participants
            .iter()
            .map(|id| {
                if *id == AgentId::orchestrator() {
                    "вы".to_owned()
                } else {
                    self.safe(id.as_str(), 40)
                }
            })
            .collect::<Vec<_>>()
            .join(" ⇄ ")
    }
    pub(super) async fn open_mail(&mut self, chat: i64, existing: Option<i64>, page: usize) {
        if !self.hub.mail_enabled() {
            self.show(
                chat,
                existing,
                "<b>✉ Почта</b>\nПочта выключена".into(),
                Vec::new(),
            )
            .await;
            return;
        }
        let times = self.mail_times();
        let mut threads = self.threads();
        threads.sort_by_key(|t| {
            (
                std::cmp::Reverse(self.unseen(t)),
                std::cmp::Reverse(t.last_seq()),
            )
        });
        let pages = threads.len().div_ceil(MAIL_PAGE).max(1);
        let page = page.min(pages - 1);
        let mut text = "<b>✉ Почта</b>".to_owned();
        let mut buttons = Vec::new();
        for (offset, thread) in threads
            .iter()
            .skip(page * MAIL_PAGE)
            .take(MAIL_PAGE)
            .enumerate()
        {
            let when = thread
                .messages
                .last()
                .and_then(|m| times.get(&m.id))
                .map_or(String::new(), |ts| {
                    format!(" · {}", format_time(*ts, now_ms()))
                });
            text.push_str(&format!(
                "\n{}. ✉ <b>{}</b> · {} · {} п.{when}{}",
                page * MAIL_PAGE + offset + 1,
                self.route_names(&thread.participants()),
                self.safe(&thread.subject, 60),
                thread.messages.len(),
                if self.unseen(thread) { " ●" } else { "" }
            ));
            buttons.push(button(
                &(page * MAIL_PAGE + offset + 1).to_string(),
                format!("s:mt:{}:0", thread.id),
            ));
        }
        if threads.is_empty() {
            text.push_str("\nПисем пока нет");
        }
        let mut rows: Vec<Vec<Value>> = buttons.chunks(6).map(<[Value]>::to_vec).collect();
        let mut nav = Vec::new();
        if page > 0 {
            nav.push(button("◂ Раньше", format!("s:m:{}", page - 1)));
        }
        if page + 1 < pages {
            nav.push(button("Дальше ▸", format!("s:m:{}", page + 1)));
        }
        if !nav.is_empty() {
            rows.push(nav);
        }
        rows.push(vec![
            button("✉ Новое письмо", "s:mn"),
            button("🔄 Обновить", format!("s:m:{page}")),
        ]);
        self.show(chat, existing, text, rows).await;
    }
    async fn open_thread(&mut self, chat: i64, existing: Option<i64>, id: &str, older: usize) {
        let Some(thread) = self.threads().into_iter().find(|t| t.id == id) else {
            self.open_mail(chat, existing, 0).await;
            return;
        };
        for message in &thread.messages {
            if message.to == AgentId::orchestrator() {
                self.mail_seen.insert(message.id.clone());
            }
        }
        let times = self.mail_times();
        let total = thread.messages.len();
        let end = total.saturating_sub(older * 3);
        let start = end.saturating_sub(3);
        let mut text = format!(
            "<b>✉ {}</b>\n{}",
            self.safe(&thread.subject, 120),
            self.route_names(&thread.participants())
        );
        for message in &thread.messages[start..end] {
            let when = times.get(&message.id).map_or(String::new(), |ts| {
                format!(" · {}", format_time(*ts, now_ms()))
            });
            text.push_str(&format!(
                "\n\n<b>{} → {}</b>{when}\n{}",
                self.route_names(std::slice::from_ref(&message.from)),
                self.route_names(std::slice::from_ref(&message.to)),
                self.safe(&message.body, 500)
            ));
        }
        let mut rows = Vec::new();
        if start > 0 {
            rows.push(vec![button(
                "◂ Показать раньше",
                format!("s:mt:{id}:{}", older + 1),
            )]);
        }
        rows.push(vec![
            button("↩ Ответить", format!("s:mr:{id}")),
            button("✉ Новое письмо", "s:mn"),
        ]);
        rows.push(vec![button("← Назад", "s:m:0")]);
        self.show(chat, existing, text, rows).await;
    }
    async fn send_mail_text(
        &mut self,
        chat: i64,
        to: AgentId,
        subject: &str,
        body: &str,
        thread: Option<String>,
    ) {
        if !self.hub.mail_enabled() {
            self.queue(chat, "Почта выключена", None);
            return;
        }
        match self.hub.send_mail(
            AgentId::orchestrator(),
            to.clone(),
            subject.trim().to_owned(),
            body.trim().to_owned(),
            thread,
        ) {
            Ok(mail) => {
                let text = format!("✔ Письмо отправлено: <b>{}</b>", self.safe(to.as_str(), 40));
                self.show(
                    chat,
                    None,
                    text,
                    vec![vec![button(
                        "Открыть переписку",
                        format!("s:mt:{}:0", mail.thread),
                    )]],
                )
                .await;
            }
            Err(error) => self.queue(chat, mail_error_text(&error), None),
        }
    }

    // ---------- workflow ----------
    fn workflow_text(&self) -> Option<(String, Vec<Vec<Value>>)> {
        let view = self.wf.as_ref()?;
        if let Some(result) = &view.result {
            return Some((self.safe(result, 400), Vec::new()));
        }
        let agent = self.control.as_ref().and_then(|control| {
            let spec = control.specs.iter().find(|s| s.name == view.name)?;
            let id = view.step.as_deref()?;
            spec.step
                .iter()
                .find(|s| s.id == id)
                .map(|s| s.agent.clone())
        });
        let total = self
            .control
            .as_ref()
            .and_then(|c| c.specs.iter().find(|s| s.name == view.name))
            .map_or(0, |s| s.step.len());
        let step = view.step.as_deref().map_or("запуск".to_owned(), |s| {
            format!(
                "шаг {} из {total}: {}",
                view.steps_started,
                self.safe(s, 60)
            )
        });
        Some((
            format!(
                "⚙ <b>{}</b>: {step}{}",
                self.safe(&view.name, 80),
                agent.map_or(String::new(), |a| format!(" · {}", self.safe(&a, 40)))
            ),
            vec![vec![button("⏹ Остановить", "s:wx")]],
        ))
    }
    pub(super) fn track_workflow(&mut self, event: &Event) {
        let Event::Workflow {
            name,
            step,
            status,
            detail,
        } = event
        else {
            return;
        };
        match status {
            WorkflowStatus::Started => {
                self.wf = Some(WorkflowView {
                    name: name.clone(),
                    ..WorkflowView::default()
                })
            }
            WorkflowStatus::StepStarted => {
                if let Some(view) = self.wf.as_mut() {
                    view.steps_started += 1;
                    view.step = step.clone();
                }
            }
            WorkflowStatus::Finished => {
                if let Some(view) = self.wf.as_mut() {
                    view.result = Some(format!("🔁 Workflow «{name}»: завершён"));
                }
            }
            WorkflowStatus::Failed => {
                if let Some(view) = self.wf.as_mut() {
                    view.result = Some(if detail == "stopped" {
                        format!("⏹ Workflow «{name}»: остановлен")
                    } else {
                        format!("❌ Workflow «{name}»: с ошибкой: {detail}")
                    });
                }
            }
            _ => {}
        }
    }
    pub(super) async fn refresh_workflow(&mut self) {
        let Some((text, rows)) = self.workflow_text() else {
            return;
        };
        let finished = self.wf.as_ref().is_some_and(|v| v.result.is_some());
        let live: Vec<_> = self
            .wf_live
            .iter()
            .filter(|(_, (_, old))| *old != text)
            .map(|(chat, (message, _))| (*chat, *message))
            .collect();
        for (chat, message) in live {
            self.show(chat, Some(message), text.clone(), rows.clone())
                .await;
            if let Some(entry) = self.wf_live.get_mut(&chat) {
                entry.1 = text.clone();
            }
        }
        if finished {
            self.wf_live.clear();
        }
    }
    async fn open_workflows(&mut self, chat: i64, existing: Option<i64>) {
        let Some(control) = self.control.clone() else {
            self.show(
                chat,
                existing,
                "<b>⚙ Workflow</b>\nЗапуск недоступен".into(),
                Vec::new(),
            )
            .await;
            return;
        };
        if control.manager.state().running {
            if let Some((text, rows)) = self
                .workflow_text()
                .filter(|_| self.wf.as_ref().is_some_and(|v| v.result.is_none()))
            {
                let message = self.show(chat, existing, text.clone(), rows).await;
                if let Some(message) = message {
                    self.wf_live.insert(chat, (message, text));
                }
                return;
            }
        }
        let mut text = "<b>⚙ Запуск workflow</b>\nВыберите сценарий:".to_owned();
        let mut buttons = Vec::new();
        for (index, spec) in control.specs.iter().enumerate() {
            text.push_str(&format!(
                "\n• <b>{}</b>{}",
                self.safe(&spec.name, 60),
                spec.description
                    .as_deref()
                    .map_or(String::new(), |d| format!(" - {}", self.safe(d, 160)))
            ));
            buttons.push(button(&spec.name, format!("s:ws:{index}")));
        }
        let mut rows: Vec<Vec<Value>> = buttons.chunks(2).map(<[Value]>::to_vec).collect();
        rows.push(vec![button("← Назад", "s:more")]);
        self.show(chat, existing, text, rows).await;
    }
    async fn confirm_workflow(
        &mut self,
        chat: i64,
        existing: Option<i64>,
        index: usize,
        task: String,
    ) {
        let Some(spec) = self
            .control
            .as_ref()
            .and_then(|c| c.specs.get(index))
            .cloned()
        else {
            return;
        };
        let agents = serde_json::to_value(spec.summary())
            .ok()
            .and_then(|v| v.get("agents").cloned())
            .and_then(|v| v.as_array().cloned())
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(|a| self.safe(a, 40))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let preview: String = task.chars().take(300).collect();
        let text = format!(
            "Запустить «{}» с задачей:\n<pre>{}</pre>\nУчаствуют: {}",
            self.safe(&spec.name, 60),
            self.safe(&preview, 400),
            agents
        );
        self.show(
            chat,
            existing,
            text,
            vec![vec![button("▶ Запустить", "s:wy"), button("Отмена", "s:w")]],
        )
        .await;
        self.inputs.insert(
            chat,
            Input {
                kind: InputKind::WorkflowConfirm(index, task),
                expires: tokio::time::Instant::now() + Duration::from_secs(600),
                message: existing,
            },
        );
    }
    async fn run_workflow(
        &mut self,
        chat: i64,
        message: Option<i64>,
        index: usize,
        task: String,
    ) -> String {
        let Some(control) = self.control.clone() else {
            return "Запуск недоступен".into();
        };
        let Some(spec) = control.specs.get(index).cloned() else {
            return "Такого workflow нет".into();
        };
        match control.manager.start(spec.clone(), task) {
            Ok(_) => {
                self.wf = Some(WorkflowView {
                    name: spec.name.clone(),
                    ..WorkflowView::default()
                });
                if let Some((text, rows)) = self.workflow_text() {
                    if let Some(message) = self.show(chat, message, text.clone(), rows).await {
                        self.wf_live.insert(chat, (message, text));
                    }
                }
                "Запущено".into()
            }
            Err(crate::workflow::StartError::Busy) => "Workflow уже идёт".into(),
            Err(crate::workflow::StartError::InvalidTeam(error)) => {
                format!("Не подходит этой команде: {error}")
            }
            Err(crate::workflow::StartError::Closed) => "Сервис останавливается".into(),
        }
    }

    // ---------- статистика, история, экспорт ----------
    pub(super) async fn open_stats(&mut self, chat: i64, existing: Option<i64>) {
        let stats = serde_json::to_value(self.hub.stats()).unwrap_or_default();
        let number = |path: &str| stats.pointer(path).and_then(Value::as_u64).unwrap_or(0);
        let tasks = STATUSES
            .iter()
            .map(|s| {
                let code = serde_json::to_value(s)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default();
                format!("{} {}", short_label(s), number(&format!("/tasks/{code}")))
            })
            .collect::<Vec<_>>()
            .join(" · ");
        let mut text = format!(
            "<b>📊 Статистика</b>\nЗадачи: {tasks}\nWorkflow: {} ({} успешно, {} с ошибкой, {} остановлено)\nПисем: {}",
            number("/totals/workflows_total"),
            number("/totals/workflows_finished"),
            number("/totals/workflows_failed"),
            number("/totals/workflows_stopped"),
            number("/totals/mails_total"),
        );
        let mut total_input = 0u64;
        let mut total_output = 0u64;
        let mut total_cost = 0.0;
        let now = now_ms();
        for member in &self.team.members {
            let base = format!("/agents/{}", member.id);
            let input = number(&format!("{base}/input_tokens"));
            let output = number(&format!("{base}/output_tokens"));
            let cost = stats
                .pointer(&format!("{base}/cost_usd"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            total_input = total_input.saturating_add(input);
            total_output = total_output.saturating_add(output);
            total_cost += cost;
            let last = number(&format!("{base}/last_activity_ms"));
            let activity = if last == 0 {
                "нет".to_owned()
            } else {
                let minutes = now.saturating_sub(last) / 60_000;
                if minutes < 1 {
                    "только что".to_owned()
                } else {
                    format!("{minutes} мин назад")
                }
            };
            text.push_str(&format!(
                "\n{} <b>{}</b> · ходов {} · команд {} · запросов {} · отказов guard {} · работал {} мин · активность {activity}",
                self.agent_status(&member.id),
                self.safe(member.id.as_str(), 40),
                number(&format!("{base}/turns")),
                number(&format!("{base}/tool_calls")),
                number(&format!("{base}/permission_requests")),
                number(&format!("{base}/guard_denied")),
                number(&format!("{base}/busy_ms")) / 60_000,
            ));
            text.push_str(&format!(" · токенов {input}/{output}"));
            if cost > 0.0 {
                text.push_str(&format!(" · ${cost:.2}"));
            }
        }
        text.push_str(&format!("\nВсего токенов: {total_input}/{total_output}"));
        if total_cost > 0.0 {
            text.push_str(&format!(" · стоимость (только Claude) ${total_cost:.2}"));
        }
        self.show(
            chat,
            existing,
            text,
            vec![vec![
                button("← Назад", "s:more"),
                button("🔄 Обновить", "s:st"),
            ]],
        )
        .await;
    }
    async fn open_log_picker(&mut self, chat: i64, existing: Option<i64>) {
        let buttons: Vec<Value> = self
            .team
            .members
            .iter()
            .enumerate()
            .map(|(index, member)| button(member.id.as_str(), format!("s:l:{index}:0:10")))
            .collect();
        let mut rows: Vec<Vec<Value>> = buttons.chunks(2).map(<[Value]>::to_vec).collect();
        rows.push(vec![button("← Назад", "s:more")]);
        self.show(
            chat,
            existing,
            "<b>📜 Что делал агент</b>\nВыберите агента:".into(),
            rows,
        )
        .await;
    }
    pub(super) async fn open_log(
        &mut self,
        chat: i64,
        existing: Option<i64>,
        agent: &AgentId,
        page: usize,
        size: usize,
    ) {
        let size = size.clamp(1, 30);
        let (history, _) = self.hub.subscribe();
        let lines: Vec<String> = history
            .iter()
            .filter(|e| e.agent == *agent)
            .filter_map(|e| event_line(&e.event).map(|l| (e.ts_ms, l)))
            .map(|(ts, line)| format!("{} {}", format_time(ts, now_ms()), line))
            .collect();
        let end = lines.len().saturating_sub(page * size);
        let start = end.saturating_sub(size);
        let mut text = format!("<b>📜 {}</b>", self.safe(agent.as_str(), 40));
        if end == 0 {
            text.push_str("\nПока нет событий");
        }
        for line in lines[start..end].iter().rev() {
            text.push_str(&format!("\n{}", self.safe(line, 200)));
        }
        let index = self
            .team
            .members
            .iter()
            .position(|m| m.id == *agent)
            .unwrap_or(0);
        let mut rows = Vec::new();
        if start > 0 {
            rows.push(vec![button(
                "Дальше ▸",
                format!("s:l:{index}:{}:{size}", page + 1),
            )]);
        }
        rows.push(vec![
            button("← Назад", "s:lg"),
            button("🔄 Обновить", format!("s:l:{index}:{page}:{size}")),
        ]);
        self.show(chat, existing, text, rows).await;
    }
    async fn open_export(&mut self, chat: i64, existing: Option<i64>) {
        self.show(
            chat,
            existing,
            "<b>⬇ Экспорт</b>\nВ каком виде прислать переписку и историю?".into(),
            vec![
                vec![button("Markdown", "s:ex:md"), button("JSONL", "s:ex:jsonl")],
                vec![button("← Назад", "s:more")],
            ],
        )
        .await;
    }
    async fn export(
        &mut self,
        chat: i64,
        existing: Option<i64>,
        format: &str,
        confirmed: bool,
    ) -> String {
        if !matches!(format, "md" | "jsonl") {
            return "Формат: md или jsonl".into();
        }
        if !self.exported && !confirmed {
            self.show(
                chat,
                existing,
                "Файл содержит всю переписку и команды и пройдёт через серверы Telegram. Отправить?".into(),
                vec![vec![
                    button("Да", format!("s:exy:{format}")),
                    button("Отмена", "s:more"),
                ]],
            )
            .await;
            return String::new();
        }
        self.exported = true;
        let (snapshot, _) = self.hub.subscribe();
        let content = if format == "jsonl" {
            crate::export::render_jsonl(&snapshot)
        } else {
            let tasks = self.hub.tasks(true);
            if tasks.is_empty() {
                crate::export::render_markdown(&snapshot)
            } else {
                crate::export::render_markdown_with_tasks(&snapshot, &tasks)
            }
        }
        .replace(&self.config.token, "[redacted]");
        if content.len() > 45 * 1024 * 1024 {
            return "Файл больше 45 МБ - Telegram его не примет".into();
        }
        let path = std::env::temp_dir().join(format!("agentsflow-{}.{format}", now_ms()));
        let file = TemporaryCommand(path.clone());
        if std::fs::write(&path, content).is_err() {
            return "Не удалось подготовить файл".into();
        }
        let result: Result<Value, _> = self
            .bot
            .request_with_form_data(
                "sendDocument",
                json!({"chat_id":chat,"caption":"Экспорт сессии"}),
                vec![("document", path)],
            )
            .await;
        drop(file);
        if result.is_ok_and(|v| v.get("ok").and_then(Value::as_bool) == Some(true)) {
            "Файл отправлен".into()
        } else {
            "Не удалось отправить файл".into()
        }
    }

    // ---------- ввод текста ----------
    pub(super) async fn process_input(&mut self, chat: i64, text: &str) {
        let Some(input) = self.inputs.remove(&chat) else {
            return;
        };
        if input.expires <= tokio::time::Instant::now() {
            self.queue(chat, "Время ввода истекло", None);
            return;
        }
        let text = text.trim();
        if text.is_empty() || text.chars().count() > 8000 {
            self.inputs.insert(chat, input);
            self.queue(chat, "Введите текст длиной до 8000 символов", None);
            return;
        }
        let message = input.message;
        match input.kind {
            InputKind::NewTask => self.create_task_from(chat, None, text).await,
            InputKind::EditTitle(id) => {
                let patch = TaskPatch {
                    title: Some(text.to_owned()),
                    ..TaskPatch::default()
                };
                let flash = match self.hub.update_task(AgentId::orchestrator(), &id, patch) {
                    Ok(_) => "✔ Название изменено".to_owned(),
                    Err(error) => task_error_text(&error),
                };
                self.open_task(chat, None, &id, &flash).await;
            }
            InputKind::EditDescription(id) => {
                let patch = TaskPatch {
                    description: Some(text.to_owned()),
                    ..TaskPatch::default()
                };
                let flash = match self.hub.update_task(AgentId::orchestrator(), &id, patch) {
                    Ok(_) => "✔ Описание изменено".to_owned(),
                    Err(error) => task_error_text(&error),
                };
                self.open_task(chat, None, &id, &flash).await;
            }
            InputKind::MailReply(thread) => {
                let Some(found) = self.threads().into_iter().find(|t| t.id == thread) else {
                    self.queue(chat, "Такой переписки нет", None);
                    return;
                };
                let last = found.messages.last().expect("thread has mail");
                let to = if last.from == AgentId::orchestrator() {
                    last.to.clone()
                } else {
                    last.from.clone()
                };
                let subject = if found.subject.starts_with("Re: ") {
                    found.subject.clone()
                } else {
                    format!("Re: {}", found.subject)
                };
                self.send_mail_text(chat, to, &subject, text, Some(thread))
                    .await;
            }
            InputKind::MailNew(to) => {
                let Some((subject, body)) = text.split_once('|') else {
                    self.inputs.insert(
                        chat,
                        Input {
                            kind: InputKind::MailNew(to),
                            expires: input.expires,
                            message,
                        },
                    );
                    self.queue(chat, "Формат: тема | текст письма", None);
                    return;
                };
                self.send_mail_text(chat, to, subject, body, None).await;
            }
            InputKind::WorkflowTask(index) => {
                self.confirm_workflow(chat, message, index, text.to_owned())
                    .await
            }
            InputKind::WorkflowConfirm(..) => {
                self.inputs.insert(chat, input);
            }
        }
    }

    // ---------- команды и кнопки ----------
    pub(super) async fn handle_command(&mut self, chat: i64, raw: &str) -> bool {
        let (command, rest) = match raw {
            "📋 Доска" => ("/board", ""),
            "✉ Почта" => ("/mail", ""),
            _ => raw
                .split_once(char::is_whitespace)
                .map_or((raw, ""), |(c, r)| (c, r.trim())),
        };
        let command = command.split('@').next().unwrap_or("");
        match command {
            "/board" => self.open_board(chat, None, 9, 0).await,
            "/task" => {
                if rest.is_empty() {
                    self.prompt_input(
                        chat,
                        None,
                        InputKind::NewTask,
                        "Как назвать задачу? Следующее сообщение станет названием. Можно: название | описание",
                        "s:b:9:0",
                    )
                    .await;
                } else {
                    self.create_task_from(chat, None, rest).await;
                }
            }
            "/mail" => {
                if rest.is_empty() {
                    self.open_mail(chat, None, 0).await;
                } else {
                    let (agent, tail) = rest
                        .split_once(char::is_whitespace)
                        .map_or((rest, ""), |(a, t)| (a, t.trim()));
                    let to = AgentId::from(agent);
                    match tail.split_once('|') {
                        Some((subject, body)) if self.team.get(&to).is_some() => {
                            self.send_mail_text(chat, to, subject, body, None).await
                        }
                        _ => self.queue(chat, "Формат: /mail <агент> <тема> | <текст>", None),
                    }
                }
            }
            "/run" => self.open_workflows(chat, None).await,
            "/stats" => self.open_stats(chat, None).await,
            "/log" => {
                let mut words = rest.split_whitespace();
                let agent = words.next().map(AgentId::from);
                let size = words.next().and_then(|n| n.parse().ok()).unwrap_or(10);
                match agent.filter(|a| self.team.get(a).is_some()) {
                    Some(agent) => self.open_log(chat, None, &agent, 0, size).await,
                    None if rest.is_empty() => self.open_log_picker(chat, None).await,
                    None => self.queue(chat, "Нет такого агента", None),
                }
            }
            "/export" => {
                if rest.is_empty() {
                    self.open_export(chat, None).await;
                } else {
                    let flash = self.export(chat, None, rest, false).await;
                    if !flash.is_empty() {
                        self.queue(chat, &flash, None);
                    }
                }
            }
            _ => return false,
        }
        true
    }
    pub(super) async fn route(&mut self, chat: i64, message: Option<i64>, data: &str) -> String {
        self.drafts.remove(&chat);
        let pending_confirm = match self.inputs.remove(&chat) {
            Some(Input {
                kind: InputKind::WorkflowConfirm(index, task),
                ..
            }) => Some((index, task)),
            _ => None,
        };
        let parts: Vec<&str> = data.split(':').collect();
        let arg = |i: usize| parts.get(i).copied().unwrap_or("");
        let number = |i: usize| arg(i).parse::<usize>().ok();
        let mut flash = String::new();
        match arg(1) {
            "more" => self.open_more(chat, message).await,
            "team" => self.open_team(chat, None, message).await,
            "help" => self.queue(chat, &help_text(), None),
            "b" => {
                self.open_board(chat, message, number(2).unwrap_or(9), number(3).unwrap_or(0))
                    .await
            }
            "t" => self.open_task(chat, message, arg(2), "").await,
            "nt" => {
                self.prompt_input(
                    chat,
                    message,
                    InputKind::NewTask,
                    "Как назвать задачу? Следующее сообщение станет названием. Можно: название | описание",
                    "s:b:9:0",
                )
                .await
            }
            "ta" => flash = self.task_action(chat, message, arg(2), arg(3)).await,
            "ts" => {
                let status = number(3).and_then(|i| STATUSES.get(i)).cloned();
                if let Some(status) = status {
                    let patch = TaskPatch {
                        status: Some(status),
                        ..TaskPatch::default()
                    };
                    flash = match self.hub.update_task(AgentId::orchestrator(), arg(2), patch) {
                        Ok(_) => "Статус изменён".into(),
                        Err(error) => task_error_text(&error),
                    };
                }
                self.open_task(chat, message, arg(2), &flash).await;
            }
            "tu" => {
                let assignee = if arg(3) == "-" {
                    Some(None)
                } else {
                    number(3)
                        .and_then(|i| self.team.members.get(i))
                        .map(|m| Some(m.id.clone()))
                };
                if let Some(assignee) = assignee {
                    let patch = TaskPatch {
                        assignee: Some(assignee),
                        ..TaskPatch::default()
                    };
                    flash = match self.hub.update_task(AgentId::orchestrator(), arg(2), patch) {
                        Ok(_) => "Исполнитель изменён".into(),
                        Err(error) => task_error_text(&error),
                    };
                }
                self.open_task(chat, message, arg(2), &flash).await;
            }
            "m" => self.open_mail(chat, message, number(2).unwrap_or(0)).await,
            "mt" => {
                self.open_thread(chat, message, arg(2), number(3).unwrap_or(0))
                    .await
            }
            "mr" => {
                self.prompt_input(
                    chat,
                    message,
                    InputKind::MailReply(arg(2).to_owned()),
                    "Что ответить? Следующее сообщение уйдёт в эту переписку.",
                    &format!("s:mt:{}:0", arg(2)),
                )
                .await
            }
            "mn" => match number(2).and_then(|i| self.team.members.get(i)) {
                Some(member) => {
                    let id = member.id.clone();
                    self.prompt_input(
                        chat,
                        message,
                        InputKind::MailNew(id.clone()),
                        &format!("Письмо для {}. Напишите одним сообщением: тема | текст", self.safe(id.as_str(), 40)),
                        "s:m:0",
                    )
                    .await
                }
                None => {
                    let buttons: Vec<Value> = self
                        .team
                        .members
                        .iter()
                        .enumerate()
                        .map(|(i, m)| button(m.id.as_str(), format!("s:mn:{i}")))
                        .collect();
                    let mut rows: Vec<Vec<Value>> =
                        buttons.chunks(2).map(<[Value]>::to_vec).collect();
                    rows.push(vec![button("← Назад", "s:m:0")]);
                    self.show(chat, message, "Кому написать?".into(), rows).await;
                }
            },
            "w" => self.open_workflows(chat, message).await,
            "ws" => {
                if let Some(index) = number(2).filter(|i| {
                    self.control.as_ref().is_some_and(|c| *i < c.specs.len())
                }) {
                    self.prompt_input(
                        chat,
                        message,
                        InputKind::WorkflowTask(index),
                        "Что поручить команде? Следующее сообщение станет задачей для workflow.",
                        "s:w",
                    )
                    .await;
                }
            }
            "wy" => match pending_confirm {
                Some((index, task)) => flash = self.run_workflow(chat, message, index, task).await,
                None => flash = "Запрос устарел".into(),
            },
            "wx" => {
                flash = match self.control.as_ref().map(|c| c.manager.stop()) {
                    Some(true) => "Останавливаю".into(),
                    _ => "Сейчас ничего не запущено".into(),
                }
            }
            "st" => self.open_stats(chat, message).await,
            "lg" => self.open_log_picker(chat, message).await,
            "l" => {
                if let Some(agent) = number(2)
                    .and_then(|i| self.team.members.get(i))
                    .map(|m| m.id.clone())
                {
                    self.open_log(
                        chat,
                        message,
                        &agent,
                        number(3).unwrap_or(0),
                        number(4).unwrap_or(10),
                    )
                    .await;
                }
            }
            "ex" if arg(2).is_empty() => self.open_export(chat, message).await,
            "ex" => flash = self.export(chat, message, arg(2), false).await,
            "exy" => flash = self.export(chat, message, arg(2), true).await,
            _ => {}
        }
        flash
    }
}

#[cfg(test)]
mod tests;
