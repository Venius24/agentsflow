//! Файлы в боте. Входящие попадают в `.agentsflow/inbox/<пакет>/` и уходят вместе
//! с задачей в папку запуска отдела. Результаты задачи бот показывает в читалке
//! или присылает файлом. Кнопки: `s:fo` читать, `s:fd` прислать, `s:fl` список,
//! `s:fr` недавние результаты, `s:fx` отменить присланное, `s:dv` правки задачи
//! (список файлов и `.diff` документом, см. [`crate::review`]).

use super::screens::InputKind;
use super::*;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;

/// Альбом приходит отдельными сообщениями: ждём, пока он соберётся.
const ALBUM_WAIT: Duration = Duration::from_millis(1500);
const INBOX_LIFETIME: Duration = Duration::from_secs(30 * 60);
const MAX_BATCH: usize = 20;
/// Больше этого читалка не открывает: такой файл удобнее прислать целиком.
const READER_LIMIT: u64 = 1024 * 1024;
/// Предел sendDocument в Bot API.
const SEND_LIMIT: u64 = 50 * 1024 * 1024;
const MAX_ARTIFACTS: usize = 200;
const LIST_PAGE: usize = 8;
const RECENT: usize = 40;
const TEXT_EXTENSIONS: [&str; 24] = [
    "md", "markdown", "txt", "csv", "tsv", "json", "jsonl", "log", "toml", "yaml", "yml", "xml",
    "html", "htm", "rs", "py", "js", "ts", "sh", "ps1", "sql", "ini", "cfg", "srt",
];

/// Присланное владельцем, ещё не ушедшее в задачу.
pub(super) struct Inbox {
    /// Задача, к которой относятся файлы (ответ или замечание).
    target: Option<String>,
    batch: String,
    names: HashSet<String>,
    files: Vec<String>,
    caption: Option<String>,
    loading: usize,
    pub(super) updated: tokio::time::Instant,
    prompted: bool,
}

pub(super) struct Fetched {
    pub chat: i64,
    pub batch: String,
    pub name: String,
    pub result: std::result::Result<PathBuf, String>,
}

struct Attachment {
    file_id: String,
    name: String,
}

/// Вложение сообщения. Telegram шлёт фото несколькими размерами, последний самый большой.
fn attachment_of(message: &Value) -> Option<Attachment> {
    let id = message
        .get("message_id")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let file = |node: &Value, fallback: String| {
        Some(Attachment {
            file_id: node.get("file_id")?.as_str()?.to_owned(),
            name: node
                .get("file_name")
                .and_then(Value::as_str)
                .map_or(fallback, str::to_owned),
        })
    };
    if let Some(photo) = message
        .get("photo")
        .and_then(Value::as_array)
        .and_then(|sizes| sizes.last())
    {
        return file(photo, format!("photo-{id}.jpg"));
    }
    for (key, fallback) in [
        ("document", format!("file-{id}")),
        ("audio", format!("audio-{id}.mp3")),
        ("video", format!("video-{id}.mp4")),
        ("animation", format!("animation-{id}.mp4")),
        ("voice", format!("voice-{id}.ogg")),
        ("video_note", format!("video-note-{id}.mp4")),
    ] {
        if let Some(node) = message.get(key) {
            return file(node, fallback);
        }
    }
    None
}

/// Имя файла от Telegram идёт в путь на диске: без папок и запрещённого Windows.
pub(crate) fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let clean: String = base
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let clean = clean.trim().trim_matches('.').trim();
    if clean.is_empty() {
        return "file".to_owned();
    }
    let (stem, extension) = match clean.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && extension.chars().count() <= 12 => {
            (stem, Some(extension))
        }
        _ => (clean, None),
    };
    let stem: String = stem.chars().take(100).collect();
    let stem = stem.trim_end();
    let lower = stem.to_ascii_lowercase();
    let reserved = matches!(lower.as_str(), "con" | "prn" | "aux" | "nul")
        || ((lower.starts_with("com") || lower.starts_with("lpt"))
            && lower.len() == 4
            && lower.ends_with(|c: char| c.is_ascii_digit()));
    let prefix = if reserved { "_" } else { "" };
    match extension {
        Some(extension) => format!("{prefix}{stem}.{extension}"),
        None => format!("{prefix}{stem}"),
    }
}

pub(crate) fn unique_name(name: String, taken: &mut HashSet<String>) -> String {
    let mut candidate = name.clone();
    let mut number = 2;
    while !taken.insert(candidate.to_lowercase()) {
        candidate = match name.rsplit_once('.') {
            Some((stem, extension)) => format!("{stem} ({number}).{extension}"),
            None => format!("{name} ({number})"),
        };
        number += 1;
    }
    candidate
}

/// Скачивает файл на диск. Ссылка содержит токен бота, поэтому ни она, ни текст
/// ошибки HTTP-клиента в сообщения не попадают.
async fn download(
    bot: Bot,
    file_url: String,
    file_id: String,
    to: PathBuf,
    lang: Lang,
) -> std::result::Result<PathBuf, String> {
    let answer: std::result::Result<Value, frankenstein::Error> = bot
        .request("getFile", Some(json!({"file_id": file_id})))
        .await;
    let answer = answer.map_err(|error| match error {
        frankenstein::Error::Api(api) if api.description.contains("too big") => {
            tr(lang, "файл больше 20 МБ, бот не может его скачать").to_owned()
        }
        _ => tr(lang, "Telegram не отдал файл").to_owned(),
    })?;
    let path = answer
        .pointer("/result/file_path")
        .and_then(Value::as_str)
        .ok_or(tr(lang, "Telegram не отдал файл"))?;
    let mut response = bot
        .client
        .get(format!("{file_url}/{path}"))
        .send()
        .await
        .map_err(|_| tr(lang, "файл не скачался").to_owned())?;
    if !response.status().is_success() {
        return Err(tr(lang, "файл не скачался").to_owned());
    }
    let parent = to.parent().ok_or(tr(lang, "неверный путь"))?;
    tokio::fs::create_dir_all(parent).await.map_err(|error| {
        i18n::render(
            lang,
            "не записать файл: {error}",
            &[("error", &error.to_string())],
        )
    })?;
    let mut file = tokio::fs::File::create(&to).await.map_err(|error| {
        i18n::render(
            lang,
            "не записать файл: {error}",
            &[("error", &error.to_string())],
        )
    })?;
    let written: std::result::Result<(), String> = async {
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| tr(lang, "файл скачался не до конца").to_owned())?
        {
            file.write_all(&chunk).await.map_err(|error| {
                i18n::render(
                    lang,
                    "не записать файл: {error}",
                    &[("error", &error.to_string())],
                )
            })?;
        }
        file.flush().await.map_err(|error| {
            i18n::render(
                lang,
                "не записать файл: {error}",
                &[("error", &error.to_string())],
            )
        })
    }
    .await;
    if let Err(error) = written {
        drop(file);
        let _ = tokio::fs::remove_file(&to).await;
        return Err(error);
    }
    Ok(to)
}

/// sendDocument с именем файла как есть. Клиент бота кодирует его процентами,
/// и кириллица в чате рискует превратиться в `%D0%BE…`.
async fn upload(bot: &Bot, chat: i64, path: PathBuf) -> std::result::Result<(), ()> {
    upload_with(bot, chat, path, None).await
}

/// То же с подписью в HTML.
async fn upload_with(
    bot: &Bot,
    chat: i64,
    path: PathBuf,
    caption: Option<String>,
) -> std::result::Result<(), ()> {
    let name = path.file_name().map_or_else(
        || "file".to_owned(),
        |name| safe_name(&name.to_string_lossy()),
    );
    let file = tokio::fs::File::open(&path).await.map_err(|_| ())?;
    let part = reqwest::multipart::Part::stream(file).file_name(name);
    let mut form = reqwest::multipart::Form::new().text("chat_id", chat.to_string());
    if let Some(caption) = caption {
        form = form.text("caption", caption).text("parse_mode", "HTML");
    }
    let form = form.part("document", part).percent_encode_noop();
    let response = bot
        .client
        .post(format!("{}/sendDocument", bot.api_url))
        .multipart(form)
        .send()
        .await
        .map_err(|_| ())?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(())
    }
}

/// Подпись к `.diff`: откуда правки и файлы с числом строк. Telegram берёт до
/// 1024 знаков, остальные файлы есть в самом документе.
fn diff_caption(diff: &crate::review::TaskDiff, title: &str, token: &str, lang: Lang) -> String {
    let mut text = i18n::render(
        lang,
        "<b>🧾 Правки</b> «{title}»\n{source}\n",
        &[
            ("title", &message_text(title, token, 120)),
            ("source", &message_text(&diff.source_label(), token, 300)),
        ],
    );
    for (shown, file) in diff.files.iter().enumerate() {
        let kind = match (file.binary, file.status) {
            (true, _) => tr(lang, " · двоичный"),
            (_, "added") => tr(lang, " · новый"),
            (_, "deleted") => tr(lang, " · удалён"),
            _ => "",
        };
        let line = format!(
            "\n• {} +{} −{}{kind}",
            message_text(&file.path, token, 120),
            file.added,
            file.removed
        );
        if text.chars().count() + line.chars().count() > 950 {
            text.push_str(&i18n::render(
                lang,
                "\n… и ещё {count}",
                &[("count", &(diff.files.len() - shown).to_string())],
            ));
            break;
        }
        text.push_str(&line);
    }
    text
}

/// Файл результата задачи.
pub(crate) struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
}

/// Итог и файлы запуска. Ссылки не читаются: агент мог бы подсунуть ссылку на
/// чужой файл и получить его содержимое в чат.
pub(crate) struct TaskFiles {
    pub has_result: bool,
    pub entries: Vec<Entry>,
}

impl TaskFiles {
    pub(crate) fn read(run_dir: &Path) -> Self {
        let mut entries = Vec::new();
        let result = run_dir.join("result.md");
        let has_result = std::fs::symlink_metadata(&result).is_ok_and(|meta| meta.is_file());
        if has_result {
            let size = std::fs::metadata(&result).map_or(0, |meta| meta.len());
            entries.push(Entry {
                name: "result.md".into(),
                path: result,
                size,
            });
        }
        let mut artifacts = Vec::new();
        collect(&run_dir.join("artifacts"), "", &mut artifacts);
        artifacts.sort_by(|a, b| a.name.cmp(&b.name));
        entries.extend(artifacts);
        Self {
            has_result,
            entries,
        }
    }
    fn artifacts(&self) -> usize {
        self.entries.len() - usize::from(self.has_result)
    }
}

fn collect(dir: &Path, prefix: &str, out: &mut Vec<Entry>) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        if out.len() >= MAX_ARTIFACTS {
            return;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let name = format!("{prefix}{}", entry.file_name().to_string_lossy());
        if kind.is_dir() {
            collect(&entry.path(), &format!("{name}/"), out);
        } else if kind.is_file() {
            let size = entry.metadata().map_or(0, |meta| meta.len());
            out.push(Entry {
                name,
                path: entry.path(),
                size,
            });
        }
    }
}

fn extension(name: &str) -> String {
    name.rsplit_once('.')
        .map_or("", |(_, extension)| extension)
        .to_lowercase()
}

fn is_text(name: &str) -> bool {
    TEXT_EXTENSIONS.contains(&extension(name).as_str())
}

fn size_label(bytes: u64, lang: Lang) -> String {
    match bytes {
        0..1024 => i18n::render(lang, "{bytes} Б", &[("bytes", &bytes.to_string())]),
        1024..1_048_576 => {
            i18n::render(lang, "{size} КБ", &[("size", &(bytes / 1024).to_string())])
        }
        _ => i18n::render(
            lang,
            "{size} МБ",
            &[("size", &format!("{:.1}", bytes as f64 / 1_048_576.0))],
        ),
    }
}

fn button(text: &str, data: String) -> Value {
    json!({"text": text, "callback_data": data})
}

/// Короткое имя для кнопки: важнее конец, в нём расширение.
fn button_name(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= 34 {
        return name.to_owned();
    }
    format!("…{}", chars[chars.len() - 33..].iter().collect::<String>())
}

impl Bridge {
    /// Файл от владельца; `false`, если в сообщении файла нет.
    pub(super) fn take_file(&mut self, chat: i64, message: &Value) -> bool {
        let lang = self.language(chat);
        let Some(attachment) = attachment_of(message) else {
            return false;
        };
        let caption = message
            .get("caption")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|caption| !caption.is_empty())
            .map(str::to_owned);
        let Some(runs) = self.control.as_ref().and_then(|c| c.runs.clone()) else {
            self.queue(
                chat,
                tr(lang, "Файлы принимаются в режиме отделов (serve --home)"),
                None,
            );
            return true;
        };
        if !self.config.prompts {
            self.queue(
                chat,
                tr(lang, "Отправка заданий из Telegram выключена владельцем"),
                None,
            );
            return true;
        }
        if self.drafts.contains_key(&chat) {
            self.queue(chat, tr(lang, "Агенту можно написать только текстом"), None);
            return true;
        }
        let now = tokio::time::Instant::now();
        let target = match self.inputs.get(&chat) {
            None => None,
            Some(input) if input.expires <= now => {
                self.inputs.remove(&chat);
                None
            }
            Some(input) => match &input.kind {
                InputKind::Answer(id) | InputKind::ReturnNote(id) => Some(id.clone()),
                _ => {
                    self.queue(
                        chat,
                        tr(lang, "Здесь нужен текст, файл сюда не приложить"),
                        None,
                    );
                    return true;
                }
            },
        };
        if self
            .inbox
            .get(&chat)
            .is_some_and(|inbox| inbox.target != target)
        {
            self.discard_inbox(chat);
        }
        let inbox = self.inbox.entry(chat).or_insert_with(|| Inbox {
            target,
            batch: uuid::Uuid::new_v4().simple().to_string()[..8].to_owned(),
            names: HashSet::new(),
            files: Vec::new(),
            caption: None,
            loading: 0,
            updated: now,
            prompted: false,
        });
        inbox.updated = now;
        inbox.prompted = false;
        if let Some(caption) = caption {
            match &mut inbox.caption {
                Some(text) => {
                    text.push('\n');
                    text.push_str(&caption);
                }
                None => inbox.caption = Some(caption),
            }
        }
        if inbox.files.len() + inbox.loading >= MAX_BATCH {
            self.queue(
                chat,
                &i18n::render(
                    lang,
                    "За один раз не больше {MAX_BATCH} файлов, этот не принят",
                    &[("MAX_BATCH", &MAX_BATCH.to_string())],
                ),
                None,
            );
            return true;
        }
        inbox.loading += 1;
        let name = unique_name(safe_name(&attachment.name), &mut inbox.names);
        let batch = inbox.batch.clone();
        let to = runs.inbox_dir().join(&batch).join(&name);
        let bot = self.bot.clone();
        let file_url = self.config.file_url();
        let tx = self.fetched_tx.clone();
        tokio::spawn(async move {
            let result = download(bot, file_url, attachment.file_id, to, lang).await;
            let _ = tx.send(Fetched {
                chat,
                batch,
                name,
                result,
            });
        });
        true
    }

    pub(super) fn fetched(&mut self, fetched: Fetched) {
        let lang = self.language(fetched.chat);
        let root = self
            .control
            .as_ref()
            .and_then(|c| c.runs.as_ref())
            .map(|runs| runs.home().root.clone());
        let Some(inbox) = self
            .inbox
            .get_mut(&fetched.chat)
            .filter(|inbox| inbox.batch == fetched.batch)
        else {
            // Владелец уже отменил этот пакет.
            if let Some(dir) = fetched
                .result
                .ok()
                .and_then(|path| path.parent().map(Path::to_path_buf))
            {
                let _ = std::fs::remove_dir_all(dir);
            }
            return;
        };
        inbox.loading = inbox.loading.saturating_sub(1);
        inbox.updated = tokio::time::Instant::now();
        match fetched.result {
            Ok(path) => {
                let relative = root
                    .as_deref()
                    .and_then(|root| path.strip_prefix(root).ok())
                    .unwrap_or(&path);
                inbox
                    .files
                    .push(relative.to_string_lossy().replace('\\', "/"));
            }
            Err(error) => self.queue(
                fetched.chat,
                &i18n::render(
                    lang,
                    "Не принял «{name}»: {error}",
                    &[("name", &fetched.name), ("error", &error)],
                ),
                None,
            ),
        }
    }

    /// Текст, который должен уйти вместе с присланными файлами.
    pub(super) fn hold_text(&mut self, chat: i64, text: &str, targeted: bool) -> bool {
        let lang = self.language(chat);
        let text = text.trim();
        let Some(inbox) = self
            .inbox
            .get_mut(&chat)
            .filter(|inbox| inbox.target.is_some() == targeted)
        else {
            return false;
        };
        if text.is_empty() {
            return false;
        }
        match &mut inbox.caption {
            Some(caption) => {
                caption.push('\n');
                caption.push_str(text);
            }
            None => inbox.caption = Some(text.to_owned()),
        }
        inbox.updated = tokio::time::Instant::now();
        // Сверх этого он не уйдёт, а владелец уже ждёт: скажем сразу.
        if inbox
            .caption
            .as_ref()
            .is_some_and(|c| c.chars().count() > 8000)
        {
            inbox.caption = None;
            self.queue(chat, tr(lang, "Сообщение длиннее 8000 символов"), None);
        }
        true
    }

    fn discard_inbox(&mut self, chat: i64) {
        let Some(inbox) = self.inbox.remove(&chat) else {
            return;
        };
        if let Some(runs) = self.control.as_ref().and_then(|c| c.runs.as_ref()) {
            let _ = std::fs::remove_dir_all(runs.inbox_dir().join(inbox.batch));
        }
    }

    /// Разбирает собравшиеся пакеты: зовётся из общего цикла.
    pub(super) async fn settle_inbox(&mut self) {
        let now = tokio::time::Instant::now();
        let ready: Vec<i64> = self
            .inbox
            .iter()
            .filter(|(_, inbox)| {
                inbox.loading == 0 && now.duration_since(inbox.updated) >= ALBUM_WAIT
            })
            .map(|(chat, _)| *chat)
            .collect();
        for chat in ready {
            self.settle(chat, now).await;
        }
    }

    async fn settle(&mut self, chat: i64, now: tokio::time::Instant) {
        let lang = self.language(chat);
        let Some(inbox) = self.inbox.get_mut(&chat) else {
            return;
        };
        let names = inbox
            .files
            .iter()
            .map(|file| file.rsplit('/').next().unwrap_or(file).to_owned())
            .collect::<Vec<_>>()
            .join(", ");
        if inbox.files.is_empty() {
            let caption = inbox.caption.take();
            self.discard_inbox(chat);
            if let Some(caption) = caption {
                let text = caption.chars().take(100).collect::<String>();
                self.queue(
                    chat,
                    &i18n::render(
                        lang,
                        "Файлы не приняты, сообщение не отправлено: {text}",
                        &[("text", &text)],
                    ),
                    None,
                );
            }
            return;
        }
        let Some(runs) = self.control.as_ref().and_then(|c| c.runs.clone()) else {
            return;
        };
        if let Some(target) = inbox.target.clone() {
            let files = inbox.files.clone();
            let caption = inbox.caption.take();
            // Пакет закрыт, файлы теперь принадлежат задаче.
            self.inbox.remove(&chat);
            match runs.attach(&target, files) {
                Ok(_) => match caption {
                    Some(caption) => self.process_input(chat, &caption).await,
                    None => self.queue(
                        chat,
                        &i18n::render(
                            lang,
                            "📎 Приложил к задаче: {names}. Теперь напишите текст.",
                            &[("names", &names)],
                        ),
                        None,
                    ),
                },
                Err(error) => self.queue(
                    chat,
                    &i18n::render(
                        lang,
                        "Не приложил файлы: {error}",
                        &[("error", &error.to_string())],
                    ),
                    None,
                ),
            }
            return;
        }
        if let Some(caption) = inbox.caption.clone() {
            let files = inbox.files.clone();
            self.inbox.remove(&chat);
            let answer = match runs.message_with_files(&caption, files) {
                Ok(()) => i18n::render(
                    lang,
                    "🧭 Принял вместе с файлами ({names}), разбираю…",
                    &[("names", &names)],
                ),
                Err(error) => i18n::render(
                    lang,
                    "Не получилось: {error}",
                    &[("error", &error.to_string())],
                ),
            };
            self.queue(chat, &answer, None);
            return;
        }
        if inbox.prompted {
            if now.duration_since(inbox.updated) >= INBOX_LIFETIME {
                self.discard_inbox(chat);
                self.queue(
                    chat,
                    &i18n::render(
                        lang,
                        "📎 Файлы ({names}) убраны: задачу к ним не написали",
                        &[("names", &names)],
                    ),
                    None,
                );
            }
            return;
        }
        inbox.prompted = true;
        let text = i18n::render(
            lang,
            "📎 Получил: {names}\n\nНапишите, что с этим сделать, и всё уйдёт оркестратору.",
            &[("names", &message_text(&names, &self.config.token, 1500))],
        );
        self.broadcast_one(
            chat,
            text,
            json!({"inline_keyboard":[[button(tr(lang, "Отмена"), "s:fx".into())]]}),
        );
    }

    fn broadcast_one(&mut self, chat: i64, text: String, buttons: Value) {
        self.queue_html(chat, text, None);
        if let Some(item) = self.queues.get_mut(&chat).and_then(VecDeque::back_mut) {
            item.buttons = Some(buttons);
        }
    }

    pub(super) fn cancel_files(&mut self, chat: i64) -> String {
        let lang = self.language(chat);
        self.discard_inbox(chat);
        tr(lang, "Файлы убраны").to_owned()
    }

    fn task_files(&self, id: &str) -> Option<TaskFiles> {
        let runs = self.control.as_ref()?.runs.as_ref()?;
        let work = self.hub.task(id)?.work?;
        let home = runs.home();
        let dept = home.departments.get(&work.department)?;
        Some(TaskFiles::read(&dept.run_dir(id)))
    }

    /// Кнопки чтения для карточки и уведомления.
    pub(super) fn file_row(&self, id: &str, lang: Lang) -> Option<Vec<Value>> {
        let files = self.task_files(id)?;
        let mut row = Vec::new();
        if files.has_result {
            row.push(button(tr(lang, "📖 Читать итог"), format!("s:fo:{id}:0:0")));
        }
        let artifacts = files.artifacts();
        if artifacts > 0 {
            row.push(button(
                &i18n::render(
                    lang,
                    "📎 Файлы · {artifacts}",
                    &[("artifacts", &artifacts.to_string())],
                ),
                format!("s:fl:{id}:0"),
            ));
        }
        (!row.is_empty()).then_some(row)
    }

    /// «Правки» на карточке, когда задаче есть что показать: своя копия
    /// репозитория или правки в ленте её агентов.
    pub(super) fn diff_row(&self, id: &str, lang: Lang) -> Option<Vec<Value>> {
        let runs = self.control.as_ref()?.runs.as_ref()?;
        let work = self.hub.task(id)?.work?;
        let home = runs.home();
        let clone = home.departments.get(&work.department).is_some_and(|dept| {
            dept.worktree && dept.run_dir(id).join("repo").join(".git").exists()
        });
        let prefix = format!("{}:", work.department);
        let suffix = format!(":{id}");
        let edits = clone
            || self.hub.subscribe().0.iter().any(|envelope| {
                let agent = envelope.agent.as_str();
                matches!(envelope.event, agentsflow_events::Event::Diff { .. })
                    && agent.starts_with(&prefix)
                    && agent.ends_with(&suffix)
            });
        edits.then(|| vec![button(tr(lang, "🧾 Правки"), format!("s:dv:{id}"))])
    }

    /// Файлы с числом строк в подписи и все правки документом `.diff`. Строчные
    /// замечания в чате неудобны: их оставляют на странице.
    pub(super) async fn send_diff(&mut self, chat: i64, id: &str) -> String {
        let lang = self.language(chat);
        let Some(runs) = self.control.as_ref().and_then(|c| c.runs.clone()) else {
            return tr(lang, "Правки есть только в режиме отделов (serve --home)").into();
        };
        let Some(task) = self.hub.task(id).filter(|task| task.work.is_some()) else {
            return tr(lang, "Задачи нет").into();
        };
        let title = task.title.clone();
        let home = runs.home();
        let (history, _) = self.hub.subscribe();
        let Ok(diff) =
            tokio::task::spawn_blocking(move || crate::review::task_diff(&home, &history, &task))
                .await
        else {
            return tr(lang, "Не удалось собрать правки").into();
        };
        if diff.files.is_empty() {
            return diff.source_label();
        }
        let caption = diff_caption(&diff, &title, &self.config.token, lang);
        let content = redact(&diff.unified(), &self.config.token);
        let dir =
            std::env::temp_dir().join(format!("agentsflow-diff-{}", uuid::Uuid::new_v4().simple()));
        let name = safe_name(&i18n::render(
            lang,
            "Правки {title}.diff",
            &[("title", &title.chars().take(60).collect::<String>())],
        ));
        let path = dir.join(name);
        if std::fs::create_dir_all(&dir).is_err() || std::fs::write(&path, content).is_err() {
            let _ = std::fs::remove_dir_all(&dir);
            return tr(lang, "Не удалось подготовить файл").into();
        }
        let sent = upload_with(&self.bot, chat, path, Some(caption)).await;
        let _ = std::fs::remove_dir_all(&dir);
        match sent {
            Ok(()) => tr(lang, "🧾 Правки отправлены").into(),
            Err(()) => tr(lang, "Не удалось прислать правки").into(),
        }
    }

    pub(super) async fn open_files(
        &mut self,
        chat: i64,
        message: Option<i64>,
        id: &str,
        page: usize,
    ) -> String {
        let lang = self.language(chat);
        let Some(files) = self.task_files(id) else {
            return tr(lang, "Файлов нет").to_owned();
        };
        let title = self.hub.task(id).map_or(String::new(), |task| task.title);
        let pages = files.entries.len().div_ceil(LIST_PAGE).max(1);
        let page = page.min(pages - 1);
        let mut text = i18n::render(
            lang,
            "<b>📎 Файлы задачи</b>\n«{title}»",
            &[("title", &message_text(&title, &self.config.token, 200))],
        );
        if files.entries.is_empty() {
            text.push_str(tr(lang, "\n\nОтдел файлов не оставил."));
        } else {
            text.push_str(tr(lang, "\n\n📖 откроется здесь, ⬇ придёт файлом."));
        }
        let mut rows: Vec<Vec<Value>> = files
            .entries
            .iter()
            .enumerate()
            .skip(page * LIST_PAGE)
            .take(LIST_PAGE)
            .map(|(index, entry)| {
                let readable = is_text(&entry.name) && entry.size <= READER_LIMIT;
                let label = format!(
                    "{} {} · {}",
                    if readable { "📖" } else { "⬇" },
                    button_name(&entry.name),
                    size_label(entry.size, lang)
                );
                let data = if readable {
                    format!("s:fo:{id}:{index}:0")
                } else {
                    format!("s:fd:{id}:{index}")
                };
                vec![button(&label, data)]
            })
            .collect();
        if pages > 1 {
            let previous = page.saturating_sub(1);
            let next = (page + 1).min(pages - 1);
            rows.push(vec![
                button("◀", format!("s:fl:{id}:{previous}")),
                button(
                    &format!("{}/{pages}", page + 1),
                    format!("s:fl:{id}:{page}"),
                ),
                button("▶", format!("s:fl:{id}:{next}")),
            ]);
        }
        rows.push(vec![button(tr(lang, "← К задаче"), format!("s:t:{id}"))]);
        self.show(chat, message, text, rows).await;
        String::new()
    }

    pub(super) async fn open_reader(
        &mut self,
        chat: i64,
        message: Option<i64>,
        id: &str,
        index: usize,
        page: usize,
    ) -> String {
        let lang = self.language(chat);
        let Some(files) = self.task_files(id) else {
            return tr(lang, "Файла нет").to_owned();
        };
        let Some(entry) = files.entries.get(index) else {
            return tr(lang, "Файла уже нет, откройте список заново").to_owned();
        };
        let text = (entry.size <= READER_LIMIT)
            .then(|| std::fs::read_to_string(&entry.path).ok())
            .flatten()
            .filter(|text| !text.contains('\0'));
        let Some(text) = text else {
            // Двоичный или слишком большой: читать нечего, отдаём целиком.
            return self.send_file(chat, id, index);
        };
        let text = redact(&text, &self.config.token);
        let markdown = matches!(extension(&entry.name).as_str(), "md" | "markdown");
        let name = message_text(&entry.name, &self.config.token, 80);
        let mut sent = false;
        // Если Telegram не принял разметку, показываем тот же текст без неё.
        for markdown in [markdown, false] {
            let pages = reader::pages(&text, markdown);
            let page = page.min(pages.len() - 1);
            let rows = self.reader_rows(id, index, page, pages.len(), files.entries.len(), lang);
            let body = format!("📖 <b>{name}</b>\n\n{}", pages[page]);
            let mut request = json!({"chat_id":chat,"text":body,"parse_mode":"HTML","link_preview_options":{"is_disabled":true},"reply_markup":{"inline_keyboard":rows}});
            let method = if let Some(message) = message {
                request["message_id"] = json!(message);
                "editMessageText"
            } else {
                "sendMessage"
            };
            let result: Result<Value, frankenstein::Error> =
                self.bot.request(method, Some(request)).await;
            let unchanged = matches!(&result, Err(frankenstein::Error::Api(api)) if api.description.contains("not modified"));
            if unchanged || result.is_ok_and(|v| v.get("ok").and_then(Value::as_bool) == Some(true))
            {
                sent = true;
                break;
            }
        }
        if sent {
            String::new()
        } else {
            tr(lang, "Telegram не показал страницу").to_owned()
        }
    }

    fn reader_rows(
        &self,
        id: &str,
        index: usize,
        page: usize,
        pages: usize,
        entries: usize,
        lang: Lang,
    ) -> Vec<Vec<Value>> {
        let go = |to: usize| format!("s:fo:{id}:{index}:{to}");
        let mut rows = Vec::new();
        if pages > 1 {
            // Кнопки не сдвигаются: на крайней странице они просто никуда не ведут.
            rows.push(vec![
                button("⏮", go(0)),
                button("◀", go(page.saturating_sub(1))),
                button(&format!("{}/{pages}", page + 1), go(page)),
                button("▶", go((page + 1).min(pages - 1))),
                button("⏭", go(pages - 1)),
            ]);
        }
        let mut actions = vec![button(tr(lang, "⬇ Скачать"), format!("s:fd:{id}:{index}"))];
        if entries > 1 {
            actions.push(button(tr(lang, "📎 Все файлы"), format!("s:fl:{id}:0")));
        }
        rows.push(actions);
        rows.push(vec![button(tr(lang, "← К задаче"), format!("s:t:{id}"))]);
        rows
    }

    /// Отправляет файл в фоне: большой файл не должен задерживать карточки разрешений.
    pub(super) fn send_file(&mut self, chat: i64, id: &str, index: usize) -> String {
        let lang = self.language(chat);
        let Some(files) = self.task_files(id) else {
            return tr(lang, "Файла нет").to_owned();
        };
        let Some(entry) = files.entries.get(index) else {
            return tr(lang, "Файла уже нет, откройте список заново").to_owned();
        };
        if entry.size > SEND_LIMIT {
            let path = message_text(&entry.path.to_string_lossy(), &self.config.token, 3000);
            self.queue_html(
                chat,
                i18n::render(lang, "Файл больше 50 МБ, Telegram его не примет. Он лежит здесь:\n<code>{path}</code>", &[("path", &path)]),
                None,
            );
            return String::new();
        }
        let bot = self.bot.clone();
        let path = entry.path.clone();
        tokio::spawn(async move {
            if upload(&bot, chat, path).await.is_err() {
                let _: Result<Value, frankenstein::Error> = bot
                    .request(
                        "sendMessage",
                        Some(json!({"chat_id":chat,"text":tr(lang, "Не удалось прислать файл")})),
                    )
                    .await;
            }
        });
        tr(lang, "⬇ Отправляю файл").to_owned()
    }

    /// Недавние задачи, у которых есть что почитать.
    pub(super) async fn open_results(&mut self, chat: i64, message: Option<i64>, page: usize) {
        let lang = self.language(chat);
        if self
            .control
            .as_ref()
            .and_then(|c| c.runs.as_ref())
            .is_none()
        {
            self.queue(
                chat,
                tr(
                    lang,
                    "Результаты задач есть только в режиме отделов (serve --home)",
                ),
                None,
            );
            return;
        }
        let mut tasks = self.hub.tasks(true);
        tasks.retain(|task| task.work.is_some());
        tasks.sort_by_key(|task| std::cmp::Reverse(task.updated_ms));
        let found: Vec<_> = tasks
            .into_iter()
            .filter_map(|task| {
                let files = self.task_files(&task.id)?;
                (!files.entries.is_empty()).then_some((task, files))
            })
            .take(RECENT)
            .collect();
        let pages = found.len().div_ceil(LIST_PAGE).max(1);
        let page = page.min(pages - 1);
        let mut rows: Vec<Vec<Value>> = found
            .iter()
            .skip(page * LIST_PAGE)
            .take(LIST_PAGE)
            .map(|(task, files)| {
                let data = if files.has_result {
                    format!("s:fo:{}:0:0", task.id)
                } else {
                    format!("s:fl:{}:0", task.id)
                };
                let title: String = task.title.chars().take(40).collect();
                vec![button(
                    &format!("{} {title}", screens::status_icon(&task.status)),
                    data,
                )]
            })
            .collect();
        if pages > 1 {
            rows.push(vec![
                button("◀", format!("s:fr:{}", page.saturating_sub(1))),
                button(&format!("{}/{pages}", page + 1), format!("s:fr:{page}")),
                button("▶", format!("s:fr:{}", (page + 1).min(pages - 1))),
            ]);
        }
        let text = if found.is_empty() {
            tr(
                lang,
                "<b>📂 Результаты</b>\n\nПока пусто: ни одна задача не оставила итога.",
            )
            .to_owned()
        } else {
            tr(
                lang,
                "<b>📂 Результаты</b>\n\nНедавние задачи, новые сверху. Нажмите, чтобы читать.",
            )
            .to_owned()
        };
        self.show(chat, message, text, rows).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{home::Home, hub::Hub, ledger::Ledger, runs::Runs, testing::Scripted};
    use axum::{
        Json, Router,
        body::Bytes,
        extract::{Path as UrlPath, State},
        routing::{get, post},
    };
    use std::sync::{Arc, Mutex};

    #[test]
    fn file_sizes_follow_chat_language() {
        assert_eq!(size_label(1024, Lang::En), "1 KB");
        assert_eq!(size_label(1_572_864, Lang::En), "1.5 MB");
        assert_eq!(size_label(1024, Lang::Ru), "1 \u{41a}\u{411}");
    }

    #[test]
    fn names_cannot_leave_the_folder() {
        assert_eq!(safe_name("../../secret.txt"), "secret.txt");
        assert_eq!(safe_name("..\\..\\boot.ini"), "boot.ini");
        assert_eq!(safe_name("C:\\Users\\a.pdf"), "a.pdf");
        assert_eq!(safe_name("a:b*c?.xlsx"), "a_b_c_.xlsx");
        assert_eq!(safe_name("..."), "file");
        assert_eq!(safe_name(""), "file");
        assert_eq!(safe_name(".env"), "env");
        assert_eq!(safe_name("nul.txt"), "_nul.txt");
        assert_eq!(safe_name("COM1"), "_COM1");
        assert_eq!(safe_name("Отчёт за май.docx"), "Отчёт за май.docx");
        let long = safe_name(&format!("{}.pdf", "я".repeat(500)));
        assert!(long.chars().count() <= 104 && long.ends_with(".pdf"));
    }

    #[test]
    fn same_names_in_a_batch_do_not_overwrite() {
        let mut taken = HashSet::new();
        assert_eq!(unique_name("a.pdf".into(), &mut taken), "a.pdf");
        assert_eq!(unique_name("A.PDF".into(), &mut taken), "A (2).PDF");
        assert_eq!(unique_name("a.pdf".into(), &mut taken), "a (3).pdf");
        assert_eq!(unique_name("b".into(), &mut taken), "b");
        assert_eq!(unique_name("b".into(), &mut taken), "b (2)");
    }

    #[test]
    fn attachment_kinds() {
        let file = |message: Value| attachment_of(&message).map(|a| (a.file_id, a.name));
        assert_eq!(
            file(json!({"message_id":5,"document":{"file_id":"d","file_name":"a.pdf"}})),
            Some(("d".into(), "a.pdf".into()))
        );
        assert_eq!(
            file(json!({"message_id":5,"photo":[{"file_id":"small"},{"file_id":"big"}]})),
            Some(("big".into(), "photo-5.jpg".into()))
        );
        assert_eq!(
            file(json!({"message_id":5,"voice":{"file_id":"v"}})),
            Some(("v".into(), "voice-5.ogg".into()))
        );
        assert!(file(json!({"message_id":5,"text":"привет"})).is_none());
    }

    #[test]
    fn task_files_skip_what_is_not_a_plain_file() {
        let dir = std::env::temp_dir().join(format!("agentsflow-files-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("artifacts/raw")).unwrap();
        std::fs::write(dir.join("result.md"), "итог").unwrap();
        std::fs::write(dir.join("artifacts/b.xlsx"), "1").unwrap();
        std::fs::write(dir.join("artifacts/raw/a.csv"), "22").unwrap();
        std::fs::write(dir.join("brief.md"), "внутреннее").unwrap();
        let secret = dir.join("secret.txt");
        std::fs::write(&secret, "секрет").unwrap();
        #[cfg(windows)]
        let linked = std::os::windows::fs::symlink_file(&secret, dir.join("artifacts/link.txt"));
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&secret, dir.join("artifacts/link.txt"));
        let files = TaskFiles::read(&dir);
        let names: Vec<_> = files.entries.iter().map(|e| e.name.as_str()).collect();
        // Создать ссылку может быть нельзя без прав; тогда её просто нет.
        let _ = linked;
        assert_eq!(names, ["result.md", "b.xlsx", "raw/a.csv"]);
        assert!(files.has_result);
        assert_eq!(files.artifacts(), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    type Calls = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

    /// Поддельный Telegram: файл по getFile и обычные ответы на остальное.
    async fn fake_api() -> (String, Calls, tokio::task::JoinHandle<()>) {
        let calls: Calls = Arc::default();
        let app = Router::new()
            .route(
                "/{path}/{method}",
                post(
                    |UrlPath((path, method)): UrlPath<(String, String)>,
                     State(calls): State<Calls>,
                     body: Bytes| async move {
                        calls
                            .lock()
                            .unwrap()
                            .push((format!("{path}/{method}"), body.to_vec()));
                        if method == "getFile" {
                            if String::from_utf8_lossy(&body).contains("big") {
                                return (
                                    axum::http::StatusCode::BAD_REQUEST,
                                    Json(json!({"ok":false,"error_code":400,"description":"Bad Request: file is too big"})),
                                );
                            }
                            return (
                                axum::http::StatusCode::OK,
                                Json(json!({"ok":true,"result":{"file_path":"documents/x.bin"}})),
                            );
                        }
                        (
                            axum::http::StatusCode::OK,
                            Json(json!({"ok":true,"result":{"message_id":42}})),
                        )
                    },
                ),
            )
            .route(
                "/file/{bot}/documents/{name}",
                get(|| async { "содержимое файла" }),
            )
            .with_state(calls.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{address}"), calls, server)
    }

    fn sent(calls: &Calls, method: &str) -> Vec<String> {
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path.ends_with(method))
            .map(|(_, body)| String::from_utf8_lossy(body).into_owned())
            .collect()
    }

    fn with_bridge(
        reply: &'static str,
        check: impl AsyncFnOnce(Bridge, Runs, Hub, PathBuf, Calls),
    ) {
        let dir = std::env::temp_dir().join(format!("agentsflow-tgfiles-{}", uuid::Uuid::new_v4()));
        crate::home::init(&dir).unwrap();
        let home = Home::load(&dir).unwrap();
        let hub = Hub::new();
        let replies: &'static [&'static str] = Box::leak(Box::new([reply]));
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(tokio::task::LocalSet::new().run_until(async {
                let runs = Runs::start(
                    hub.clone(),
                    home,
                    Ledger::memory(),
                    crate::runs::RunEnv::default(),
                );
                let env = crate::runs::RunEnv {
                    codex_bin: None,
                    gh: None,
                    stall_clock: None,
                    spawner: Some(std::rc::Rc::new(move |hub, member, dir| {
                        let dir = dir.to_path_buf();
                        crate::hub::spawn(hub.clone(), member.id.clone(), true, move || {
                            let dir = dir.clone();
                            async move { Ok(Scripted::new(replies, Some(0.01), &dir)) }
                        })
                    })),
                };
                runs.set_orchestrator(crate::orchestrator::Orchestrator::start(runs.clone(), env));
                let (url, calls, server) = fake_api().await;
                let config = Config::parse("secret".into(), "11", url).unwrap();
                let team = Team::preset("pair").unwrap();
                let manager =
                    crate::workflow::Manager::new(hub.clone(), Agents::new(), team.clone());
                let mut bridge = Bridge::with_hub(config, Agents::new(), team, hub.clone());
                bridge.control = Some(Control {
                    specs: Vec::new(),
                    manager,
                    projects: None,
                    announce: None,
                    runs: Some(runs.clone()),
                });
                check(bridge, runs, hub, dir.clone(), calls).await;
                server.abort();
            }));
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn message(body: Value) -> Value {
        let mut message =
            json!({"message_id":7,"from":{"id":11},"chat":{"id":11,"type":"private"}});
        message
            .as_object_mut()
            .unwrap()
            .extend(body.as_object().unwrap().clone());
        json!({ "message": message })
    }

    fn callback(data: &str) -> Value {
        json!({"callback_query":{"id":"cb","from":{"id":11},"data":data,"message":{"message_id":42,"chat":{"id":11,"type":"private"}}}})
    }

    /// Дожидается скачивания и окончания паузы альбома.
    async fn settle(bridge: &mut Bridge) {
        while bridge.inbox.values().any(|inbox| inbox.loading > 0) {
            let fetched = bridge.fetched_rx.as_mut().unwrap().recv().await.unwrap();
            bridge.fetched(fetched);
        }
        for inbox in bridge.inbox.values_mut() {
            inbox.updated -= ALBUM_WAIT;
        }
        bridge.settle_inbox().await;
    }

    const PLAN: &str = r#"{"message":"","actions":[{"type":"create_task","title":"Разбор","text":"разобрать файл","department":"general","questions":["Формат?"]}]}"#;

    #[test]
    fn file_with_caption_reaches_a_task() {
        with_bridge(PLAN, async |mut bridge, runs, hub, dir, calls| {
            bridge
                .update(&message(json!({"caption":"разбери","document":{"file_id":"d1","file_name":"../отчёт.txt"}})))
                .await;
            settle(&mut bridge).await;
            assert!(bridge.inbox.is_empty());
            assert!(
                bridge.queues[&11]
                    .back()
                    .unwrap()
                    .text
                    .contains("отчёт.txt")
            );
            let task = loop {
                if let Some(task) = hub.tasks(false).into_iter().find(|t| t.title == "Разбор")
                {
                    break task;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            let files = task.work.unwrap().attachments;
            assert_eq!(files.len(), 1);
            assert!(files[0].starts_with(".agentsflow/inbox/"));
            assert!(files[0].ends_with("/отчёт.txt"));
            assert_eq!(
                std::fs::read_to_string(dir.join(&files[0])).unwrap(),
                "содержимое файла"
            );
            assert_eq!(runs.inbox_dir(), dir.join(".agentsflow/inbox"));
            assert_eq!(sent(&calls, "/getFile").len(), 1);
        });
    }

    #[test]
    fn file_without_caption_waits_for_text_and_album_stays_together() {
        with_bridge(PLAN, async |mut bridge, _, hub, _, _| {
            bridge
                .update(&message(
                    json!({"document":{"file_id":"a","file_name":"a.txt"}}),
                ))
                .await;
            bridge
                .update(&message(
                    json!({"document":{"file_id":"b","file_name":"a.txt"}}),
                ))
                .await;
            settle(&mut bridge).await;
            let prompt = bridge.queues[&11].back().unwrap();
            assert!(prompt.text.contains("Напишите"));
            assert!(prompt.text.contains("a (2).txt"));
            assert!(
                prompt
                    .buttons
                    .as_ref()
                    .unwrap()
                    .to_string()
                    .contains("s:fx")
            );
            bridge.update(&message(json!({"text":"сравни их"}))).await;
            settle(&mut bridge).await;
            let task = loop {
                if let Some(task) = hub.tasks(false).into_iter().find(|t| t.title == "Разбор")
                {
                    break task;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            assert_eq!(task.work.unwrap().attachments.len(), 2);
        });
    }

    #[test]
    fn cancel_removes_the_files() {
        with_bridge(PLAN, async |mut bridge, runs, _, _, _| {
            bridge
                .update(&message(
                    json!({"document":{"file_id":"a","file_name":"a.txt"}}),
                ))
                .await;
            settle(&mut bridge).await;
            let batch = runs.inbox_dir().join(&bridge.inbox[&11].batch);
            assert!(batch.is_dir());
            bridge.update(&callback("s:fx")).await;
            assert!(bridge.inbox.is_empty());
            assert!(!batch.exists());
        });
    }

    #[test]
    fn too_big_file_gets_a_clear_answer_without_the_token() {
        with_bridge(PLAN, async |mut bridge, _, _, _, _| {
            bridge
                .update(&message(
                    json!({"caption":"разбери","document":{"file_id":"big","file_name":"v.mp4"}}),
                ))
                .await;
            settle(&mut bridge).await;
            let texts: Vec<_> = bridge.queues[&11]
                .iter()
                .map(|item| item.text.clone())
                .collect();
            assert!(
                texts.iter().any(|text| text.contains("больше 20 МБ")),
                "{texts:?}"
            );
            assert!(
                texts.iter().any(|text| text.contains("не отправлено")),
                "{texts:?}"
            );
            assert!(!texts.iter().any(|text| text.contains("secret")));
            assert!(bridge.inbox.is_empty());
        });
    }

    #[test]
    fn file_during_an_answer_goes_to_that_task_before_the_answer() {
        with_bridge(PLAN, async |mut bridge, runs, hub, _, _| {
            let task = runs
                .create(
                    AgentId::from("оркестратор"),
                    crate::runs::NewWork {
                        title: "Счёт".into(),
                        text: "x".into(),
                        department: "general".into(),
                        questions: vec![agentsflow_events::Question {
                            text: "Какой файл?".into(),
                            answer: None,
                            from: "оркестратор".into(),
                        }],
                        ..Default::default()
                    },
                )
                .unwrap();
            bridge
                .update(&callback(&format!("s:ta:{}:ans", task.id)))
                .await;
            assert!(bridge.inputs.contains_key(&11));
            bridge
                .update(&message(
                    json!({"document":{"file_id":"a","file_name":"счёт.txt"}}),
                ))
                .await;
            // Ответ текстом пришёл раньше, чем файл докачался: ждёт его.
            bridge.update(&message(json!({"text":"вот файл"}))).await;
            let work = hub.task(&task.id).unwrap().work.unwrap();
            assert!(work.questions[0].answer.is_none());
            settle(&mut bridge).await;
            let work = hub.task(&task.id).unwrap().work.unwrap();
            assert_eq!(work.attachments.len(), 1);
            assert_eq!(work.questions[0].answer.as_deref(), Some("вот файл"));
        });
    }

    #[test]
    fn reader_pages_and_files_come_back() {
        with_bridge(PLAN, async |mut bridge, runs, _, dir, calls| {
            let task = runs
                .create(
                    AgentId::from("оркестратор"),
                    crate::runs::NewWork {
                        title: "Отчёт".into(),
                        text: "x".into(),
                        department: "general".into(),
                        questions: vec![agentsflow_events::Question {
                            text: "Какой?".into(),
                            answer: None,
                            from: "оркестратор".into(),
                        }],
                        ..Default::default()
                    },
                )
                .unwrap();
            let id = task.id.clone();
            let run = dir.join("departments/general/runs").join(&id);
            std::fs::create_dir_all(run.join("artifacts")).unwrap();
            let body = format!(
                "# Итог\n{}**конец** secret",
                "длинная строка\n".repeat(1000)
            );
            std::fs::write(run.join("result.md"), body).unwrap();
            std::fs::write(run.join("artifacts/таблица.bin"), [0_u8, 1, 2]).unwrap();
            bridge.update(&callback(&format!("s:t:{id}"))).await;
            let card = sent(&calls, "/editMessageText").pop().unwrap();
            assert!(card.contains(&format!("s:fo:{id}:0:0")));
            assert!(card.contains(&format!("s:fl:{id}:0")));

            bridge.update(&callback(&format!("s:fo:{id}:0:0"))).await;
            let first = sent(&calls, "/editMessageText").pop().unwrap();
            assert!(first.contains("<b>Итог</b>"));
            assert!(first.contains(&format!("s:fo:{id}:0:1")));
            assert!(first.contains(&format!("s:fd:{id}:0")));
            bridge.update(&callback(&format!("s:fo:{id}:0:999"))).await;
            let last = sent(&calls, "/editMessageText").pop().unwrap();
            assert!(last.contains("<b>конец</b> [redacted]"), "{last}");

            bridge.update(&callback(&format!("s:fl:{id}:0"))).await;
            let list = sent(&calls, "/editMessageText").pop().unwrap();
            assert!(list.contains("result.md"));
            assert!(list.contains(&format!("s:fd:{id}:1")));

            // Двоичный файл приходит документом и из читалки тоже.
            bridge.update(&callback(&format!("s:fo:{id}:1:0"))).await;
            for _ in 0..200 {
                if !sent(&calls, "/sendDocument").is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(sent(&calls, "/sendDocument")[0].contains("filename=\"таблица.bin\""));

            bridge.update(&message(json!({"text":"/files"}))).await;
            let recent = sent(&calls, "/sendMessage").pop().unwrap();
            assert!(recent.contains(&format!("s:fo:{id}:0:0")));
            assert!(recent.contains("Отчёт"));
        });
    }

    #[test]
    fn the_changes_button_sends_the_file_list_and_a_diff() {
        with_bridge(PLAN, async |mut bridge, runs, hub, _, calls| {
            let task = runs
                .create(
                    AgentId::from("оркестратор"),
                    crate::runs::NewWork {
                        title: "Правка".into(),
                        text: "x".into(),
                        department: "general".into(),
                        questions: vec![agentsflow_events::Question {
                            text: "Где?".into(),
                            answer: None,
                            from: "оркестратор".into(),
                        }],
                        ..Default::default()
                    },
                )
                .unwrap();
            let id = task.id.clone();
            // Nothing changed yet: no button.
            bridge.update(&callback(&format!("s:t:{id}"))).await;
            let card = sent(&calls, "/editMessageText").pop().unwrap();
            assert!(!card.contains(&format!("s:dv:{id}")), "{card}");
            hub.publish(
                AgentId::from(format!("general:doer:{id}")),
                "run",
                agentsflow_events::Event::Diff {
                    path: "notes.md".into(),
                    patch: "--- notes.md\n+++ notes.md\n@@ edit @@\n-old secret\n+new\n".into(),
                },
            );
            bridge.update(&callback(&format!("s:t:{id}"))).await;
            let card = sent(&calls, "/editMessageText").pop().unwrap();
            assert!(card.contains(&format!("s:dv:{id}")), "{card}");
            assert!(card.contains("Правки"));
            bridge.update(&callback(&format!("s:dv:{id}"))).await;
            let documents = sent(&calls, "/sendDocument");
            assert_eq!(documents.len(), 1);
            let document = &documents[0];
            assert!(
                document.contains("filename=\"Правки Правка.diff\""),
                "{document}"
            );
            assert!(document.contains("• notes.md +1 −1"), "{document}");
            assert!(document.contains("Правки из ленты агентов"));
            assert!(document.contains("# notes.md · правка 1 · doer"));
            // The bot token never leaves in the file.
            assert!(document.contains("-old [redacted]") && !document.contains("old secret"));
        });
    }

    #[test]
    fn outside_home_mode_files_are_refused_politely() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let config = Config::parse("secret".into(), "11", "http://127.0.0.1:1".into()).unwrap();
            let mut bridge = Bridge::new(config, Agents::new(), Team::preset("pair").unwrap());
            bridge
                .update(&message(
                    json!({"document":{"file_id":"a","file_name":"a.txt"}}),
                ))
                .await;
            assert!(
                bridge.queues[&11]
                    .back()
                    .unwrap()
                    .text
                    .contains("режиме отделов")
            );
            assert!(bridge.inbox.is_empty());
        });
    }
}
