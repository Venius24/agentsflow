//! Routes of `serve --home`: departments, department tasks, runs, spending.

use super::*;
use crate::runs::{NewWork, Runs};
use crate::telegram::files::{TaskFiles, safe_name, unique_name};
use agentsflow_events::Risk;
use std::collections::HashSet;
use std::path::Path;

fn no_home() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error":"доступно только в режиме serve --home"})),
    )
        .into_response()
}

fn runs(state: &AppState) -> Result<&Runs, Box<Response>> {
    state.runs.as_ref().ok_or_else(|| Box::new(no_home()))
}

pub(super) async fn departments(State(state): State<AppState>) -> Response {
    let runs = match runs(&state) {
        Ok(runs) => runs,
        Err(response) => return *response,
    };
    let home = runs.home();
    let spend = runs.month_spend();
    let tokens = runs.month_tokens();
    let running = runs.running();
    let list: Vec<Value> = home
        .departments
        .values()
        .map(|dept| {
            let agents: Vec<Value> = dept
                .team
                .members
                .iter()
                .map(|m| json!({"id":m.id,"kind":m.kind,"model":m.model,"effort":m.effort,"roles":m.roles,"title":m.title,"mode":m.mode,"sandbox":m.sandbox}))
                .collect();
            json!({
                "id": dept.id,
                "title": dept.title,
                "description": dept.description,
                "default_risk": dept.default_risk,
                "repos": dept.repos,
                "worktree": dept.worktree,
                "max_parallel": dept.max_parallel,
                "schedules": dept.schedules.iter().map(|s| json!({"cron":s.cron,"title":s.title})).collect::<Vec<_>>(),
                "limits": {
                    "task_usd": dept.limits.task_usd,
                    "month_usd": dept.limits.month_usd,
                    "timeout_mins": dept.limits.timeout_mins,
                    "max_loops": dept.limits.max_loops,
                },
                "agents": agents,
                "running": running.iter().filter(|run| run.department == dept.id).count(),
                "month_spent_usd": spend.get(&dept.id).copied().unwrap_or(0.0),
                "month_tokens": tokens.get(&dept.id).copied().unwrap_or_default(),
            })
        })
        .collect();
    Json(json!({"departments": list, "max_parallel_runs": home.config.max_parallel_runs}))
        .into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateWork {
    #[serde(default)]
    title: Option<String>,
    text: String,
    department: String,
    #[serde(default)]
    risk: Option<Risk>,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    budget_usd: Option<f64>,
}

/// A short title from the first line of the text.
pub fn title_from(text: &str) -> String {
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    let mut title: String = line.trim().chars().take(80).collect();
    if line.trim().chars().count() > 80 {
        title.push('…');
    }
    title
}

pub(super) async fn create_work(
    State(state): State<AppState>,
    input: Result<Json<CreateWork>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let runs = match runs(&state) {
        Ok(runs) => runs,
        Err(response) => return *response,
    };
    let Ok(Json(input)) = input else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid task"})),
        )
            .into_response();
    };
    if input.text.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"пустая задача"})),
        )
            .into_response();
    }
    let title = input
        .title
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| title_from(&input.text));
    match runs.create(
        AgentId::orchestrator(),
        NewWork {
            title,
            text: input.text,
            department: input.department,
            risk: input.risk,
            depends_on: input.depends_on,
            budget_usd: input.budget_usd,
            source: "owner".into(),
            ..Default::default()
        },
    ) {
        Ok(task) => (StatusCode::CREATED, Json(json!({"task":task}))).into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({"error":error}))).into_response(),
    }
}

pub(super) async fn stop_task(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let runs = match runs(&state) {
        Ok(runs) => runs,
        Err(response) => return *response,
    };
    if runs.stop(&id) {
        Json(json!({"ok":true})).into_response()
    } else {
        (
            StatusCode::CONFLICT,
            Json(json!({"error":"задача не выполняется"})),
        )
            .into_response()
    }
}

pub(super) async fn retry_task(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    if let Err(response) = runs(&state) {
        return *response;
    }
    let patch = TaskPatch {
        status: Some(agentsflow_events::TaskStatus::Todo),
        ..Default::default()
    };
    match state.hub.update_task(AgentId::orchestrator(), &id, patch) {
        Ok(task) => Json(json!({"task":task})).into_response(),
        Err(error) => task_failure(error),
    }
}

pub(super) async fn task_result(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let runs = match runs(&state) {
        Ok(runs) => runs,
        Err(response) => return *response,
    };
    let Some(task) = state.hub.task(&id) else {
        return task_failure(TaskError::NotFound);
    };
    let Some(path) = task.work.and_then(|work| work.result_path) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error":"результата нет"})),
        )
            .into_response();
    };
    let root = runs.home().root.clone();
    let full = root.join(&path);
    // The path comes from the board file; never read outside the home.
    let inside = full
        .canonicalize()
        .ok()
        .zip(root.canonicalize().ok())
        .is_some_and(|(full, root)| full.starts_with(root));
    if !inside {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error":"результата нет"})),
        )
            .into_response();
    }
    match std::fs::read_to_string(&full) {
        Ok(text) => Json(json!({"path":path,"text":text})).into_response(),
        Err(_) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error":"результата нет"})),
        )
            .into_response(),
    }
}

pub(super) async fn list_runs(State(state): State<AppState>) -> Response {
    match runs(&state) {
        Ok(runs) => Json(json!({"runs":runs.running()})).into_response(),
        Err(response) => *response,
    }
}

pub(super) async fn reload_home(State(state): State<AppState>) -> Response {
    let runs = match runs(&state) {
        Ok(runs) => runs,
        Err(response) => return *response,
    };
    match runs.reload() {
        Ok(()) => Json(json!({"ok":true})).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":format!("{error:#}")})),
        )
            .into_response(),
    }
}

pub(super) async fn spend(State(state): State<AppState>) -> Response {
    let runs = match runs(&state) {
        Ok(runs) => runs,
        Err(response) => return *response,
    };
    let month = runs.month_spend();
    let tokens = runs.month_tokens();
    let total = month.values().fold(0.0, |sum, usd| sum + usd);
    Json(json!({"month": month, "tokens": tokens, "total_usd": total})).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Text {
    text: String,
}

fn bad_request(error: String) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error":error}))).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Message {
    text: String,
    #[serde(default)]
    files: Vec<String>,
}

/// A free-form owner message: the orchestrator turns it into tasks. `files`
/// are inbox paths from `/api/upload`.
pub(super) async fn orchestrator(
    State(state): State<AppState>,
    input: Result<Json<Message>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let runs = match runs(&state) {
        Ok(runs) => runs,
        Err(response) => return *response,
    };
    let Ok(Json(input)) = input else {
        return bad_request("invalid message".into());
    };
    let sent = if input.files.is_empty() {
        runs.message(&input.text)
    } else {
        inbox_files(runs, &input.files)
            .and_then(|files| runs.message_with_files(&input.text, files))
    };
    match sent {
        Ok(()) => (StatusCode::ACCEPTED, Json(json!({"ok":true}))).into_response(),
        Err(error) => bad_request(error),
    }
}

pub(super) async fn answer(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    input: Result<Json<Text>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let runs = match runs(&state) {
        Ok(runs) => runs,
        Err(response) => return *response,
    };
    let Ok(Json(input)) = input else {
        return bad_request("invalid answer".into());
    };
    match runs.answer(&id, &input.text) {
        Ok(task) => Json(json!({"task":task})).into_response(),
        Err(error) => bad_request(error),
    }
}

/// The owner sends a result back with a note for the next run.
pub(super) async fn owner_return(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    input: Result<Json<Text>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let runs = match runs(&state) {
        Ok(runs) => runs,
        Err(response) => return *response,
    };
    let Ok(Json(input)) = input else {
        return bad_request("invalid note".into());
    };
    match runs.owner_return(&id, &input.text) {
        Ok(task) => Json(json!({"task":task})).into_response(),
        Err(error) => bad_request(error),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AgentNote {
    agent: String,
    text: String,
    #[serde(default)]
    files: Vec<String>,
}

/// The owner writes to an agent of a running task.
pub(super) async fn agent_note(
    State(state): State<AppState>,
    input: Result<Json<AgentNote>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let runs = match runs(&state) {
        Ok(runs) => runs,
        Err(response) => return *response,
    };
    let Ok(Json(input)) = input else {
        return bad_request("invalid note".into());
    };
    let files = match inbox_files(runs, &input.files) {
        Ok(files) => files,
        Err(error) => return bad_request(error),
    };
    match runs.note_with_files(&input.agent, &input.text, files) {
        Ok(id) => Json(json!({"id":id})).into_response(),
        Err(error) => (StatusCode::CONFLICT, Json(json!({"error":error}))).into_response(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Decision {
    create: bool,
}

/// The owner's decision on a proposed department.
pub(super) async fn proposal(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    input: Result<Json<Decision>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let runs = match runs(&state) {
        Ok(runs) => runs,
        Err(response) => return *response,
    };
    let Ok(Json(input)) = input else {
        return bad_request("invalid decision".into());
    };
    match runs.resolve_proposal(&id, input.create) {
        Ok(department) => Json(json!({"department":department})).into_response(),
        Err(error) => bad_request(error),
    }
}

pub(super) async fn schedules(State(state): State<AppState>) -> Response {
    match runs(&state) {
        Ok(runs) => Json(json!({"schedules": runs.schedules()})).into_response(),
        Err(response) => *response,
    }
}

/// `pause`, `resume` or `run` (put the task into the queue now).
pub(super) async fn schedule_action(
    State(state): State<AppState>,
    axum::extract::Path((key, action)): axum::extract::Path<(String, String)>,
) -> Response {
    let runs = match runs(&state) {
        Ok(runs) => runs,
        Err(response) => return *response,
    };
    let result = match action.as_str() {
        "pause" => runs
            .pause_schedule(&key, true)
            .map(|()| json!({"ok": true})),
        "resume" => runs
            .pause_schedule(&key, false)
            .map(|()| json!({"ok": true})),
        "run" => runs.run_schedule(&key).map(|task| json!({"task": task})),
        _ => Err("действие: pause, resume или run".into()),
    };
    match result {
        Ok(body) => Json(body).into_response(),
        Err(error) => bad_request(error),
    }
}

/// Open rule proposals and the latest added rules.
pub(super) async fn rules(State(state): State<AppState>) -> Response {
    match runs(&state) {
        Ok(runs) => {
            Json(json!({"proposals": runs.rule_proposals(), "recent": runs.recent_rules(20)}))
                .into_response()
        }
        Err(response) => *response,
    }
}

pub(super) async fn approve_rule(
    State(state): State<AppState>,
    axum::extract::Path((target, id)): axum::extract::Path<(String, String)>,
) -> Response {
    let runs = match runs(&state) {
        Ok(runs) => runs,
        Err(response) => return *response,
    };
    match runs.approve_rule(&target, &id) {
        Ok(rule) => Json(json!({"rule": rule})).into_response(),
        Err(error) => bad_request(error),
    }
}

pub(super) async fn reject_rule(
    State(state): State<AppState>,
    axum::extract::Path((target, id)): axum::extract::Path<(String, String)>,
) -> Response {
    let runs = match runs(&state) {
        Ok(runs) => runs,
        Err(response) => return *response,
    };
    match runs.reject_rule(&target, &id) {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(error) => bad_request(error),
    }
}

pub(super) async fn revert_rule(
    State(state): State<AppState>,
    axum::extract::Path(commit): axum::extract::Path<String>,
) -> Response {
    let runs = match runs(&state) {
        Ok(runs) => runs,
        Err(response) => return *response,
    };
    match runs.revert_rule(&commit) {
        Ok(rule) => Json(json!({"rule": rule})).into_response(),
        Err(error) => bad_request(error),
    }
}

/// Largest file the page uploads: the same as the bot can send.
pub(super) const UPLOAD_LIMIT: usize = 50 * 1024 * 1024;
/// Files in one message, as in a Telegram album.
const MAX_FILES: usize = 20;

#[derive(Deserialize)]
pub(super) struct UploadQuery {
    name: String,
    #[serde(default)]
    batch: Option<String>,
}

fn valid_batch(batch: &str) -> bool {
    (1..=32).contains(&batch.len()) && batch.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn home_relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// A file from the page lands in the inbox, like one sent to the bot: the
/// raw body is the file, `name` its name, `batch` groups one message's files.
pub(super) async fn upload(
    State(state): State<AppState>,
    Query(query): Query<UploadQuery>,
    body: axum::body::Bytes,
) -> Response {
    let runs = match runs(&state) {
        Ok(runs) => runs,
        Err(response) => return *response,
    };
    if body.is_empty() {
        return bad_request("пустой файл".into());
    }
    let batch = match query.batch {
        Some(batch) if valid_batch(&batch) => batch,
        Some(_) => return bad_request("invalid batch".into()),
        None => Uuid::new_v4().simple().to_string()[..8].to_owned(),
    };
    let dir = runs.inbox_dir().join(&batch);
    if std::fs::create_dir_all(&dir).is_err() {
        return bad_request("не удалось создать папку входящих".into());
    }
    let mut taken: HashSet<String> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_lowercase())
        .collect();
    if taken.len() >= MAX_FILES {
        return bad_request(format!("не больше {MAX_FILES} файлов за раз"));
    }
    let name = unique_name(safe_name(&query.name), &mut taken);
    let path = dir.join(&name);
    let written = async {
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await?;
        file.write_all(&body).await?;
        file.flush().await
    };
    if written.await.is_err() {
        let _ = std::fs::remove_file(&path);
        return bad_request(format!("не удалось сохранить «{name}»"));
    }
    let root = runs.home().root.clone();
    Json(json!({
        "path": home_relative(&root, &path),
        "name": name,
        "size": body.len(),
        "batch": batch,
    }))
    .into_response()
}

/// Paths the page sends back must be plain files of the inbox.
fn inbox_files(runs: &Runs, files: &[String]) -> Result<Vec<String>, String> {
    check_inbox(&runs.home().root, &runs.inbox_dir(), files)
}

fn check_inbox(root: &Path, inbox: &Path, files: &[String]) -> Result<Vec<String>, String> {
    if files.len() > MAX_FILES {
        return Err(format!("не больше {MAX_FILES} файлов за раз"));
    }
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let inbox = inbox
        .canonicalize()
        .map_err(|_| "входящих пока нет".to_owned())?;
    files
        .iter()
        .map(|file| {
            let full = root.join(file);
            let plain = std::fs::symlink_metadata(&full).is_ok_and(|meta| meta.is_file());
            let inside = full
                .canonicalize()
                .is_ok_and(|full| full.starts_with(&inbox));
            if plain && inside {
                Ok(file.replace('\\', "/"))
            } else {
                Err(format!("файла нет во входящих: {file}"))
            }
        })
        .collect()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Files {
    files: Vec<String>,
}

/// Owner's files for a task: its next round gets them in `inputs/owner/`.
pub(super) async fn attach(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    input: Result<Json<Files>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let runs = match runs(&state) {
        Ok(runs) => runs,
        Err(response) => return *response,
    };
    let Ok(Json(input)) = input else {
        return bad_request("invalid files".into());
    };
    let files = match inbox_files(runs, &input.files) {
        Ok(files) if !files.is_empty() => files,
        Ok(_) => return bad_request("файлов нет".into()),
        Err(error) => return bad_request(error),
    };
    match runs.attach(&id, files) {
        Ok(task) => Json(json!({"task":task})).into_response(),
        Err(error) => bad_request(error),
    }
}

struct TaskFile {
    path: String,
    name: String,
    size: u64,
    kind: &'static str,
    full: std::path::PathBuf,
}

/// What the owner can download from a task: their attachments, the result
/// and the run's artifacts. Links are skipped, as in the bot.
fn task_files(runs: &Runs, id: &str) -> Option<Vec<TaskFile>> {
    let work = runs.hub().task(id)?.work?;
    let home = runs.home();
    let run_dir = home
        .departments
        .get(&work.department)
        .map(|dept| dept.run_dir(id));
    Some(list_task_files(
        &home.root,
        &work.attachments,
        run_dir.as_deref(),
    ))
}

fn list_task_files(root: &Path, attachments: &[String], run_dir: Option<&Path>) -> Vec<TaskFile> {
    let mut out = Vec::new();
    for file in attachments {
        let full = root.join(file);
        let Ok(meta) = std::fs::symlink_metadata(&full) else {
            continue;
        };
        if meta.is_file() {
            out.push(TaskFile {
                path: file.replace('\\', "/"),
                name: file.rsplit(['/', '\\']).next().unwrap_or(file).to_owned(),
                size: meta.len(),
                kind: "attachment",
                full,
            });
        }
    }
    if let Some(run_dir) = run_dir {
        let result = run_dir.join("result.md");
        for entry in TaskFiles::read(run_dir).entries {
            let kind = if entry.path == result {
                "result"
            } else {
                "artifact"
            };
            out.push(TaskFile {
                path: home_relative(root, &entry.path),
                name: entry.name,
                size: entry.size,
                kind,
                full: entry.path,
            });
        }
    }
    out
}

pub(super) async fn list_files(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let runs = match runs(&state) {
        Ok(runs) => runs,
        Err(response) => return *response,
    };
    let Some(files) = task_files(runs, &id) else {
        return task_failure(TaskError::NotFound);
    };
    let list: Vec<Value> = files
        .iter()
        .map(|file| json!({"path":file.path,"name":file.name,"size":file.size,"kind":file.kind}))
        .collect();
    Json(json!({"files":list})).into_response()
}

#[derive(Deserialize)]
pub(super) struct FileQuery {
    path: String,
    #[serde(default)]
    inline: bool,
}

fn content_type(name: &str) -> &'static str {
    let extension = name
        .rsplit_once('.')
        .map_or("", |(_, extension)| extension)
        .to_ascii_lowercase();
    match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "zip" => "application/zip",
        // Text of any kind, HTML and SVG included, is shown as plain text:
        // a page an agent wrote must not run scripts next to the token.
        "md" | "txt" | "csv" | "tsv" | "json" | "jsonl" | "log" | "toml" | "yaml" | "yml"
        | "xml" | "html" | "htm" | "svg" | "rs" | "py" | "js" | "ts" | "sh" | "ps1" | "sql"
        | "ini" | "cfg" | "srt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// `Content-Disposition` with the real (often Cyrillic) name: RFC 6266
/// `filename*` plus an ASCII fallback for old clients.
fn disposition(name: &str, inline: bool) -> String {
    let fallback: String = name
        .chars()
        .map(|c| {
            if c == ' ' || (c.is_ascii_graphic() && c != '"' && c != '\\') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut encoded = String::new();
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    let kind = if inline { "inline" } else { "attachment" };
    format!("{kind}; filename=\"{fallback}\"; filename*=UTF-8''{encoded}")
}

/// One file of a task by the path `/files` listed: nothing else is served.
pub(super) async fn download(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Query(query): Query<FileQuery>,
) -> Response {
    let runs = match runs(&state) {
        Ok(runs) => runs,
        Err(response) => return *response,
    };
    let missing = || (StatusCode::NOT_FOUND, Json(json!({"error":"файла нет"}))).into_response();
    let Some(file) = task_files(runs, &id)
        .into_iter()
        .flatten()
        .find(|file| file.path == query.path)
    else {
        return missing();
    };
    let Ok(bytes) = tokio::fs::read(&file.full).await else {
        return missing();
    };
    let kind = content_type(&file.name);
    let inline = query.inline
        && (kind.starts_with("image/") || kind.starts_with("text/") || kind == "application/pdf");
    // The result downloads under the task's title, not as "result.md".
    let name = match runs.hub().task(&id) {
        Some(task) if file.kind == "result" => safe_name(&format!(
            "{}.md",
            task.title.chars().take(80).collect::<String>()
        )),
        _ => file
            .name
            .rsplit('/')
            .next()
            .unwrap_or(&file.name)
            .to_owned(),
    };
    (
        [
            (header::CONTENT_TYPE, kind.to_owned()),
            (header::CONTENT_DISPOSITION, disposition(&name, inline)),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
            (header::CONTENT_SECURITY_POLICY, "sandbox".to_owned()),
        ],
        bytes,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::{check_inbox, content_type, disposition, list_task_files, title_from};
    use std::path::Path;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("agentsflow-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn inbox_paths_must_stay_in_the_inbox() {
        let root = scratch("inbox");
        let inbox = root.join(".agentsflow/inbox");
        std::fs::create_dir_all(inbox.join("ab12")).unwrap();
        std::fs::write(inbox.join("ab12/цены.xlsx"), b"x").unwrap();
        std::fs::write(root.join("secret.txt"), b"x").unwrap();
        let ok = check_inbox(&root, &inbox, &[".agentsflow/inbox/ab12/цены.xlsx".into()]).unwrap();
        assert_eq!(ok, vec![".agentsflow/inbox/ab12/цены.xlsx".to_owned()]);
        for bad in [
            "secret.txt",
            ".agentsflow/inbox/../../secret.txt",
            ".agentsflow/inbox/ab12/нет.xlsx",
            ".agentsflow/inbox/ab12",
        ] {
            assert!(check_inbox(&root, &inbox, &[bad.into()]).is_err(), "{bad}");
        }
        let absolute = root.join("secret.txt").display().to_string();
        assert!(check_inbox(&root, &inbox, &[absolute]).is_err());
        let many = vec![".agentsflow/inbox/ab12/цены.xlsx".to_owned(); 21];
        assert!(check_inbox(&root, &inbox, &many).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn task_files_list_attachments_result_and_artifacts() {
        let root = scratch("taskfiles");
        std::fs::create_dir_all(root.join(".agentsflow/inbox/ab12")).unwrap();
        std::fs::write(root.join(".agentsflow/inbox/ab12/цены.xlsx"), b"12").unwrap();
        let run = root.join("departments/shop/runs/t1");
        std::fs::create_dir_all(run.join("artifacts/sub")).unwrap();
        std::fs::write(run.join("result.md"), b"done").unwrap();
        std::fs::write(run.join("artifacts/sub/report.csv"), b"a,b").unwrap();
        let files = list_task_files(
            &root,
            &[
                ".agentsflow/inbox/ab12/цены.xlsx".into(),
                ".agentsflow/inbox/ab12/gone.txt".into(),
            ],
            Some(&run),
        );
        let seen: Vec<_> = files
            .iter()
            .map(|f| (f.kind, f.name.as_str(), f.path.as_str(), f.size))
            .collect();
        assert_eq!(
            seen,
            vec![
                (
                    "attachment",
                    "цены.xlsx",
                    ".agentsflow/inbox/ab12/цены.xlsx",
                    2
                ),
                (
                    "result",
                    "result.md",
                    "departments/shop/runs/t1/result.md",
                    4
                ),
                (
                    "artifact",
                    "sub/report.csv",
                    "departments/shop/runs/t1/artifacts/sub/report.csv",
                    3
                ),
            ]
        );
        assert!(list_task_files(Path::new("/nowhere"), &[], None).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn downloads_never_serve_active_content() {
        assert_eq!(content_type("page.html"), "text/plain; charset=utf-8");
        assert_eq!(content_type("logo.SVG"), "text/plain; charset=utf-8");
        assert_eq!(content_type("photo.JPG"), "image/jpeg");
        assert_eq!(content_type("data.bin"), "application/octet-stream");
        assert_eq!(
            disposition("цены 1.xlsx", false),
            "attachment; filename=\"____ 1.xlsx\"; filename*=UTF-8''%D1%86%D0%B5%D0%BD%D1%8B%201.xlsx"
        );
        assert!(disposition("a\".txt", true).starts_with("inline; filename=\"a_.txt\""));
    }

    #[test]
    fn titles_come_from_the_first_line() {
        assert_eq!(
            title_from("\n  Отчёт по Байтаку \nподробности"),
            "Отчёт по Байтаку"
        );
        assert_eq!(title_from(&"я".repeat(90)).chars().count(), 81);
    }
}
