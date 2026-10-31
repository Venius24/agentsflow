use std::{collections::HashMap, convert::Infallible, path::PathBuf, time::Duration};

use crate::hub::{Actor, Hub, HubError, NewTask, TaskError, TaskPatch};
use crate::{
    runtime::{Agents, MailAgentTokens, Runtime, RuntimeOptions},
    team::Team,
    workflow::{self, Manager, Spec},
};
use agentsflow_adapters::codex::SandboxMode;
use agentsflow_events::AgentId;
use anyhow::Result;
use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Query, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{
        Html, IntoResponse, Response,
        sse::{Event as SseEvent, KeepAlive, Sse},
    },
    routing::{get, patch, post},
};
use futures_util::{StreamExt, stream};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::watch, task::LocalSet};
use uuid::Uuid;

#[derive(Clone)]
pub struct ServeOptions {
    projects: Vec<PathBuf>,
    port: u16,
    cwd: PathBuf,
    claude: bool,
    codex: bool,
    team: Team,
    claude_model: Option<String>,
    codex_model: Option<String>,
    sandbox: Option<SandboxMode>,
    codex_bin: Option<PathBuf>,
    yes: bool,
    allow_bypass: bool,
    telegram_requested: bool,
    telegram: Option<crate::telegram::Config>,
    workflows: Vec<PathBuf>,
    log: Option<PathBuf>,
    no_log: bool,
    mail_thread_limit: usize,
}
impl ServeOptions {
    #[cfg(test)]
    pub fn parse(args: &[String]) -> Option<Self> {
        Self::parse_checked(args).ok().flatten()
    }
    pub fn parse_checked(args: &[String]) -> Result<Option<Self>> {
        if args.iter().any(|arg| arg == "--team") && args.iter().any(|arg| arg == "--agents") {
            anyhow::bail!("--team and --agents cannot be used together");
        }
        let Some(mut options) = Self::parse_basic(args) else {
            return Ok(None);
        };
        if let Some(index) = args.iter().position(|arg| arg == "--team") {
            options.team = Team::load(&args[index + 1])?;
        } else {
            options.team = Team::filtered_pair(options.claude, options.codex)?;
        }
        options.team.check_bypass(options.allow_bypass)?;
        if options.telegram_requested {
            options.telegram = Some(crate::telegram::Config::from_env()?);
        }
        Ok(Some(options))
    }
    fn parse_basic(args: &[String]) -> Option<Self> {
        let mut options = Self {
            projects: Vec::new(),
            port: 7878,
            cwd: std::env::current_dir().ok()?,
            claude: true,
            codex: true,
            team: Team::preset("pair").ok()?,
            claude_model: None,
            codex_model: None,
            sandbox: None,
            codex_bin: None,
            yes: false,
            allow_bypass: false,
            telegram_requested: false,
            telegram: None,
            workflows: Vec::new(),
            log: None,
            no_log: false,
            mail_thread_limit: crate::hub::MAX_THREAD_MAILS,
        };
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            if arg == "--yes" {
                options.yes = true;
                continue;
            }
            if arg == "--no-log" {
                options.no_log = true;
                continue;
            }
            if arg == "--allow-bypass" {
                options.allow_bypass = true;
                continue;
            }
            if arg == "--telegram" {
                options.telegram_requested = true;
                continue;
            }
            let value = args.next()?;
            if value.is_empty() || value.starts_with("--") {
                return None;
            }
            match arg.as_str() {
                "--projects" => options.projects.push(value.into()),
                "--log" => options.log = Some(value.into()),
                "--workflow" => options.workflows.push(value.into()),
                "--port" => options.port = value.parse().ok()?,
                "--mail-thread-limit" => {
                    let limit = value.parse().ok()?;
                    if !(2..=100).contains(&limit) {
                        return None;
                    }
                    options.mail_thread_limit = limit;
                }
                "--team" => {}
                "--cwd" => options.cwd = PathBuf::from(value),
                "--agents" => {
                    options.claude = false;
                    options.codex = false;
                    for name in value.split(',') {
                        match name {
                            "claude" if !options.claude => options.claude = true,
                            "codex" if !options.codex => options.codex = true,
                            _ => return None,
                        }
                    }
                }
                "--claude-model" => options.claude_model = Some(value.clone()),
                "--codex-model" => options.codex_model = Some(value.clone()),
                "--codex-bin" => options.codex_bin = Some(value.into()),
                "--sandbox" => {
                    options.sandbox = Some(match value.as_str() {
                        "read-only" => SandboxMode::Read_only,
                        "workspace-write" => SandboxMode::Workspace_write,
                        _ => return None,
                    })
                }
                _ => return None,
            }
        }
        Some(options)
    }
    pub fn workflows(&self) -> Result<Vec<Spec>> {
        let mut specs = workflow::builtins()?;
        for path in &self.workflows {
            specs.push(Spec::load(path)?);
        }
        let mut names = std::collections::HashSet::new();
        for spec in &specs {
            if !names.insert(&spec.name) {
                anyhow::bail!("duplicate workflow name: {}", spec.name);
            }
        }
        Ok(specs)
    }
    pub fn runtime_options(&self) -> RuntimeOptions {
        RuntimeOptions {
            cwd: self.cwd.clone(),
            team: self.team.clone(),
            claude_model: self.claude_model.clone(),
            codex_model: self.codex_model.clone(),
            sandbox: self.sandbox.clone(),
            codex_bin: self.codex_bin.clone(),
            yes: self.yes,
            allow_bypass: self.allow_bypass,
            log: self.log.clone(),
            mail: None,
            mail_agent_tokens: MailAgentTokens::default(),
            mail_thread_limit: self.mail_thread_limit,
        }
    }
}

#[derive(Clone)]
struct AppState {
    projects: Option<crate::projects::Control>,
    hub: Hub,
    token: String,
    mail_agent_tokens: MailAgentTokens,
    port: u16,
    cwd: String,
    agents: Agents,
    team: Team,
    shutdown: watch::Receiver<bool>,
    workflows: Vec<Spec>,
    manager: Manager,
}
impl AppState {
    fn actor(&self, agent: &str) -> Option<&Actor> {
        self.agents.get(&AgentId::from(agent))
    }
}

fn verify_token(expected: &str, supplied: &str) -> bool {
    let a = expected.as_bytes();
    let b = supplied.as_bytes();
    let mut diff = a.len() ^ b.len();
    for (i, byte) in a.iter().enumerate() {
        diff |= usize::from(byte ^ b.get(i).copied().unwrap_or(0));
    }
    diff == 0
}
fn host_allowed(host: &str, port: u16) -> bool {
    host == format!("127.0.0.1:{port}") || host == format!("localhost:{port}")
}
fn origin_allowed(origin: &str, host: &str) -> bool {
    origin == format!("http://{host}")
}

#[derive(Clone, Debug, PartialEq)]
enum Identity {
    Main,
    Agent(AgentId),
}

fn identify(token: &str, main: &str, agents: &MailAgentTokens) -> Option<Identity> {
    if token.is_empty() {
        return None;
    }
    if verify_token(main, token) {
        return Some(Identity::Main);
    }
    for (agent, expected) in agents {
        if verify_token(expected, token) {
            return Some(Identity::Agent(agent.clone()));
        }
    }
    None
}

fn authorize(identity: Identity, method: &axum::http::Method, path: &str) -> bool {
    identity == Identity::Main
        || (*method == axum::http::Method::POST
            && matches!(path, "/api/mail/send" | "/api/mail/inbox"))
        || (*method == axum::http::Method::GET && path == "/api/tasks/mine")
        || (*method == axum::http::Method::POST
            && path
                .strip_prefix("/api/tasks/")
                .and_then(|s| s.strip_suffix("/update"))
                .is_some_and(|id| !id.is_empty() && !id.contains('/')))
}
fn authenticate(
    header: &str,
    query: &str,
    main: &str,
    agents: &MailAgentTokens,
) -> Option<Identity> {
    identify(header, main, agents).or_else(|| identify(query, main, agents))
}

fn authorize_request(
    header: &str,
    query: &str,
    main: &str,
    agents: &MailAgentTokens,
    method: &axum::http::Method,
    path: &str,
) -> Result<Identity, StatusCode> {
    let identity = authenticate(header, query, main, agents).ok_or(StatusCode::UNAUTHORIZED)?;
    if !authorize(identity.clone(), method, path) {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(identity)
}

async fn guard(State(state): State<AppState>, mut request: Request, next: Next) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !host_allowed(host, state.port) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if request.method() == axum::http::Method::POST {
        if let Some(origin) = request.headers().get(header::ORIGIN) {
            if !origin
                .to_str()
                .ok()
                .is_some_and(|value| origin_allowed(value, host))
            {
                return StatusCode::FORBIDDEN.into_response();
            }
        }
    }
    if request.uri().path().starts_with("/api/") {
        let header_token = request
            .headers()
            .get("x-agentsflow-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let query = Query::<HashMap<String, String>>::try_from_uri(request.uri()).ok();
        let query_token = query
            .as_ref()
            .and_then(|q| q.get("token"))
            .map(String::as_str)
            .unwrap_or("");
        let identity = match authorize_request(
            header_token,
            query_token,
            &state.token,
            &state.mail_agent_tokens,
            request.method(),
            request.uri().path(),
        ) {
            Ok(identity) => identity,
            Err(status) => return status.into_response(),
        };
        request.extensions_mut().insert(identity);
    }
    next.run(request).await
}

async fn export(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let (format, content_type, extension) =
        match query.get("format").map(String::as_str).unwrap_or("md") {
            "md" => (
                crate::export::render_markdown as fn(&[_]) -> String,
                "text/markdown; charset=utf-8",
                "md",
            ),
            "jsonl" => (
                crate::export::render_jsonl as fn(&[_]) -> String,
                "application/x-ndjson",
                "jsonl",
            ),
            _ => return StatusCode::BAD_REQUEST.into_response(),
        };
    let (snapshot, _) = state.hub.subscribe();
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let disposition = format!("attachment; filename=\"agentsflow-{timestamp}.{extension}\"");
    let tasks = state.hub.tasks(true);
    let content = if extension == "md" && !tasks.is_empty() {
        crate::export::render_markdown_with_tasks(&snapshot, &tasks)
    } else {
        format(&snapshot)
    };
    (
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            (header::CONTENT_DISPOSITION, disposition),
        ],
        content,
    )
        .into_response()
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../../../web/index.html"))
}
fn sse_event(envelope: agentsflow_events::Envelope) -> std::result::Result<SseEvent, Infallible> {
    Ok(SseEvent::default()
        .id(envelope.seq.to_string())
        .data(serde_json::to_string(&envelope).expect("event serializes")))
}
async fn events(State(state): State<AppState>) -> impl IntoResponse {
    let (snapshot, receiver) = state.hub.subscribe();
    let live = stream::unfold(
        (receiver, state.shutdown),
        |(mut receiver, mut shutdown)| async move {
            tokio::select! {
                biased;
                _ = shutdown.changed() => None,
                item = receiver.recv() => match item { Ok(item) => Some((sse_event(item), (receiver, shutdown))), Err(_) => None },
            }
        },
    );
    let stream = stream::iter(snapshot.into_iter().map(sse_event)).chain(live);
    let mut response = Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());
    response
}
#[derive(Deserialize)]
struct Prompt {
    agent: String,
    text: String,
}
#[derive(Deserialize)]
struct Interrupt {
    agent: String,
}
#[derive(Deserialize)]
struct Permission {
    agent: String,
    id: String,
    allow: bool,
}
#[derive(Deserialize)]
struct SendMail {
    from: String,
    to: String,
    subject: String,
    body: String,
    thread: Option<String>,
}
#[derive(Deserialize)]
struct Inbox {
    agent: String,
}
fn parse_agent(name: &str, team: &Team) -> Option<AgentId> {
    let id = AgentId::from(name);
    (id == AgentId::orchestrator() || team.get(&id).is_some()).then_some(id)
}
fn unknown_agent(state: &AppState, name: &str, status: StatusCode) -> Response {
    let valid = state
        .team
        .ids()
        .map(ToString::to_string)
        .chain(std::iter::once("orchestrator".into()))
        .collect::<Vec<_>>()
        .join(", ");
    (
        status,
        Json(json!({"error":format!("unknown agent {name}; valid: {valid}")})),
    )
        .into_response()
}

async fn send_mail(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    input: Result<Json<SendMail>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(input)) = input else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid mail request"})),
        )
            .into_response();
    };
    let Some(from) = parse_agent(&input.from, &state.team) else {
        return unknown_agent(&state, &input.from, StatusCode::BAD_REQUEST);
    };
    let Some(to) = parse_agent(&input.to, &state.team) else {
        return unknown_agent(&state, &input.to, StatusCode::BAD_REQUEST);
    };
    if matches!(identity, Identity::Agent(agent) if agent != from) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error":"sender does not match token"})),
        )
            .into_response();
    }
    match state
        .hub
        .send_mail(from, to, input.subject, input.body, input.thread)
    {
        Ok(mail) => Json(json!({"id":mail.id,"thread":mail.thread})).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":error.to_string()})),
        )
            .into_response(),
    }
}
async fn inbox(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    input: Result<Json<Inbox>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(input)) = input else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid inbox request"})),
        )
            .into_response();
    };
    let Some(agent) = parse_agent(&input.agent, &state.team) else {
        return unknown_agent(&state, &input.agent, StatusCode::BAD_REQUEST);
    };
    if matches!(identity, Identity::Agent(owner) if owner != agent) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error":"inbox does not match token"})),
        )
            .into_response();
    }
    let messages: Vec<_> = state.hub.take_inbox(agent).into_iter().map(|mail| json!({"id":mail.id,"thread":mail.thread,"from":mail.from,"subject":mail.subject,"body":mail.body})).collect();
    Json(json!({"messages":messages})).into_response()
}
fn failure(error: HubError) -> Response {
    match error {
        HubError::Busy => (StatusCode::CONFLICT, Json(json!({"error":"busy"}))).into_response(),
        HubError::NotBusy => {
            (StatusCode::CONFLICT, Json(json!({"error":"not busy"}))).into_response()
        }
        HubError::NotFound => StatusCode::NOT_FOUND.into_response(),
        HubError::Closed => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}
async fn prompt(State(state): State<AppState>, Json(input): Json<Prompt>) -> Response {
    if input.text.trim().is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some(actor) = state.actor(&input.agent) else {
        return unknown_agent(&state, &input.agent, StatusCode::NOT_FOUND);
    };
    match actor.prompt(input.text).await {
        Ok(()) => (StatusCode::ACCEPTED, Json(json!({"ok":true}))).into_response(),
        Err(error) => failure(error),
    }
}
async fn interrupt(State(state): State<AppState>, Json(input): Json<Interrupt>) -> Response {
    let Some(actor) = state.actor(&input.agent) else {
        return unknown_agent(&state, &input.agent, StatusCode::NOT_FOUND);
    };
    match actor.interrupt().await {
        Ok(()) => Json(json!({"ok":true})).into_response(),
        Err(error) => failure(error),
    }
}
async fn restart(State(state): State<AppState>, Json(input): Json<Interrupt>) -> Response {
    let Some(actor) = state.actor(&input.agent) else {
        return unknown_agent(&state, &input.agent, StatusCode::NOT_FOUND);
    };
    match actor.restart().await {
        Ok(()) => Json(json!({"ok":true})).into_response(),
        Err(error) => failure(error),
    }
}
async fn health(State(state): State<AppState>) -> Json<Value> {
    let agents: HashMap<_, _> = state
        .agents
        .iter()
        .map(|(id, actor)| {
            let state = actor.state();
            (
                id.to_string(),
                json!({"enabled":state.enabled,"busy":state.busy,"last_error":state.last_error}),
            )
        })
        .collect();
    Json(
        json!({"ok":true,"version":env!("CARGO_PKG_VERSION"),"uptime_s":state.hub.uptime_s(),"agents":agents}),
    )
}
async fn permission(State(state): State<AppState>, Json(input): Json<Permission>) -> Response {
    let Some(actor) = state.actor(&input.agent) else {
        return unknown_agent(&state, &input.agent, StatusCode::NOT_FOUND);
    };
    match actor.respond(input.id, input.allow).await {
        Ok(()) => Json(json!({"ok":true})).into_response(),
        Err(error) => failure(error),
    }
}
async fn get_state(State(state): State<AppState>) -> Json<Value> {
    let agents: HashMap<_, _> = state
        .agents
        .iter()
        .map(|(id, actor)| (id.to_string(), actor.state()))
        .collect();
    Json(
        json!({"version": env!("CARGO_PKG_VERSION"), "cwd": state.cwd, "projects_enabled": state.projects.as_ref().is_some_and(|p| p.enabled()), "agents": agents, "team": state.team.members, "workflow": state.manager.state()}),
    )
}
async fn get_projects(State(state): State<AppState>) -> Response {
    let Some(control) = &state.projects else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    match control.list() {
        Ok(projects) => {
            let current = projects
                .iter()
                .find(|p| p.current)
                .map(|p| json!({"name":p.name,"path":p.path}));
            Json(json!({"enabled":control.enabled(),"current":current,"projects":projects}))
                .into_response()
        }
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"Не удалось прочитать проекты"})),
        )
            .into_response(),
    }
}
#[derive(Deserialize)]
struct SwitchProject {
    path: PathBuf,
}
async fn switch_project(
    State(state): State<AppState>,
    Json(input): Json<SwitchProject>,
) -> Response {
    let Some(control) = &state.projects else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match control.switch(&input.path) {
        Ok(false) => Json(json!({"ok":true,"unchanged":true})).into_response(),
        Ok(true) => (StatusCode::ACCEPTED, Json(json!({"ok":true}))).into_response(),
        Err(crate::projects::SwitchError::Unknown) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error":"unknown project"})),
        )
            .into_response(),
        Err(crate::projects::SwitchError::Busy) => (
            StatusCode::CONFLICT,
            Json(json!({"error":"Сначала остановите работу агентов"})),
        )
            .into_response(),
        Err(crate::projects::SwitchError::Unavailable) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"Не удалось сменить проект"})),
        )
            .into_response(),
    }
}
async fn get_team(State(state): State<AppState>) -> Json<Value> {
    Json(json!(state.team.members))
}

#[derive(Deserialize)]
struct RunWorkflow {
    name: String,
    task: String,
}
fn validate_workflow_request<'a>(
    specs: &'a [Spec],
    name: &str,
    task: &str,
) -> Result<&'a Spec, StatusCode> {
    if task.trim().is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    specs
        .iter()
        .find(|spec| spec.name == name)
        .ok_or(StatusCode::NOT_FOUND)
}
async fn list_workflows(State(state): State<AppState>) -> Json<Value> {
    Json(json!(
        state
            .workflows
            .iter()
            .map(Spec::summary)
            .collect::<Vec<_>>()
    ))
}
async fn run_workflow(State(state): State<AppState>, Json(input): Json<RunWorkflow>) -> Response {
    let spec = match validate_workflow_request(&state.workflows, &input.name, &input.task) {
        Ok(spec) => spec,
        Err(status) => return status.into_response(),
    };
    match state.manager.start(spec.clone(), input.task) {
        Ok(id) => (StatusCode::ACCEPTED, Json(json!({"ok":true,"run":id}))).into_response(),
        Err(workflow::StartError::Busy) => {
            (StatusCode::CONFLICT, Json(json!({"error":"busy"}))).into_response()
        }
        Err(workflow::StartError::InvalidTeam(message)) => {
            (StatusCode::BAD_REQUEST, Json(json!({"error":message}))).into_response()
        }
        Err(workflow::StartError::Closed) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}
async fn stop_workflow(State(state): State<AppState>) -> Response {
    if state.manager.stop() {
        Json(json!({"ok":true})).into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

fn task_failure(error: TaskError) -> Response {
    let status = match error {
        TaskError::NotFound => StatusCode::NOT_FOUND,
        TaskError::Limit | TaskError::MailDisabled | TaskError::NoAssignee => StatusCode::CONFLICT,
        TaskError::Forbidden => StatusCode::FORBIDDEN,
        _ => StatusCode::BAD_REQUEST,
    };
    (status, Json(json!({"error":error.to_string()}))).into_response()
}

async fn list_tasks(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let include = match query.get("include_archived").map(String::as_str) {
        None | Some("0") => false,
        Some("1") => true,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":"invalid include_archived"})),
            )
                .into_response();
        }
    };
    Json(json!({"tasks":state.hub.tasks(include)})).into_response()
}
async fn create_task(
    State(state): State<AppState>,
    input: Result<Json<NewTask>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(input)) = input else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid task"})),
        )
            .into_response();
    };
    if let Some(id) = &input.assignee {
        if state.team.get(id).is_none() {
            return unknown_agent(&state, id.as_str(), StatusCode::BAD_REQUEST);
        }
    }
    match state.hub.create_task(AgentId::orchestrator(), input) {
        Ok(task) => (StatusCode::CREATED, Json(json!({"task":task}))).into_response(),
        Err(error) => task_failure(error),
    }
}
async fn patch_task(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    input: Result<Json<TaskPatch>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(patch)) = input else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid task patch"})),
        )
            .into_response();
    };
    if let Some(Some(agent)) = &patch.assignee {
        if state.team.get(agent).is_none() {
            return unknown_agent(&state, agent.as_str(), StatusCode::BAD_REQUEST);
        }
    }
    match state.hub.update_task(AgentId::orchestrator(), &id, patch) {
        Ok(task) => Json(json!({"task":task})).into_response(),
        Err(error) => task_failure(error),
    }
}
async fn dispatch_task(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    match state.hub.dispatch_task(&id) {
        Ok(task) => Json(json!({"task":task})).into_response(),
        Err(error) => task_failure(error),
    }
}
async fn my_tasks(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
) -> Response {
    let tasks: Vec<_> = state
        .hub
        .tasks(false)
        .into_iter()
        .filter(|task| match &identity {
            Identity::Main => true,
            Identity::Agent(agent) => task.assignee.as_ref() == Some(agent),
        })
        .collect();
    Json(json!({"tasks":tasks})).into_response()
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentTaskPatch {
    status: Option<agentsflow_events::TaskStatus>,
    progress: Option<u8>,
    note: Option<String>,
}
async fn update_my_task(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Extension(identity): Extension<Identity>,
    input: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(value)) = input else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid task patch"})),
        )
            .into_response();
    };
    if identity == Identity::Main {
        let Ok(patch) = serde_json::from_value::<TaskPatch>(value) else {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":"invalid task patch"})),
            )
                .into_response();
        };
        return patch_task(State(state), axum::extract::Path(id), Ok(Json(patch))).await;
    }
    let Identity::Agent(agent) = identity else {
        unreachable!()
    };
    if !value.as_object().is_some_and(|obj| {
        obj.keys()
            .all(|key| matches!(key.as_str(), "status" | "progress" | "note"))
    }) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error":"only status, progress and note may be updated"})),
        )
            .into_response();
    }
    let Ok(input) = serde_json::from_value::<AgentTaskPatch>(value) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid task patch"})),
        )
            .into_response();
    };
    if matches!(
        input.status,
        Some(agentsflow_events::TaskStatus::Done | agentsflow_events::TaskStatus::Archived)
    ) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error":"only the orchestrator can close a task; set review"})),
        )
            .into_response();
    }
    let patch = TaskPatch {
        status: input.status,
        progress: input.progress,
        note: input.note,
        ..Default::default()
    };
    match state.hub.update_task_owned(agent, &id, patch) {
        Ok(task) => Json(json!({"task":task})).into_response(),
        Err(error) => task_failure(error),
    }
}

async fn get_stats(State(state): State<AppState>) -> Json<crate::hub::StatsSnapshot> {
    let mut stats = state.hub.stats();
    for member in &state.team.members {
        stats.agents.entry(member.id.clone()).or_default();
    }
    stats.agents.entry(AgentId::orchestrator()).or_default();
    Json(stats)
}

fn app(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/events", get(events))
        .route("/api/export", get(export))
        .route("/api/prompt", post(prompt))
        .route("/api/agent/interrupt", post(interrupt))
        .route("/api/agent/restart", post(restart))
        .route("/api/health", get(health))
        .route("/api/permission", post(permission))
        .route("/api/mail/send", post(send_mail))
        .route("/api/mail/inbox", post(inbox))
        .route("/api/tasks", get(list_tasks).post(create_task))
        .route("/api/tasks/mine", get(my_tasks))
        .route("/api/tasks/{id}", patch(patch_task))
        .route("/api/tasks/{id}/dispatch", post(dispatch_task))
        .route("/api/tasks/{id}/update", post(update_my_task))
        .route("/api/state", get(get_state))
        .route("/api/projects", get(get_projects))
        .route("/api/project/switch", post(switch_project))
        .route("/api/stats", get(get_stats))
        .route("/api/team", get(get_team))
        .route("/api/workflows", get(list_workflows))
        .route("/api/workflow/run", post(run_workflow))
        .route("/api/workflow/stop", post(stop_workflow))
        .layer(DefaultBodyLimit::max(256 * 1024))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

pub async fn serve(options: ServeOptions, specs: Vec<Spec>) -> Result<()> {
    LocalSet::new().run_until(serve_local(options, specs)).await
}
async fn serve_local(mut options: ServeOptions, specs: Vec<Spec>) -> Result<()> {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, options.port)).await?;
    let port = listener.local_addr()?.port();
    let token = Uuid::new_v4().simple().to_string();
    let listener = listener.into_std()?;
    options.cwd = crate::projects::canonical(&options.cwd)?;
    let mut previous = None;
    let mut announce = None;
    let mut first = true;
    loop {
        let socket = TcpListener::from_std(listener.try_clone()?)?;
        match run_session(
            &options,
            &specs,
            socket,
            port,
            &token,
            first,
            announce.take(),
        )
        .await
        {
            Ok(Some(path)) => {
                previous = Some(options.cwd.clone());
                options.cwd = path;
                announce = Some(
                    options
                        .cwd
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                );
                first = false;
            }
            Ok(None) => return Ok(()),
            Err(error) => {
                if let Some(old) = previous.take() {
                    crate::server_diag!("project startup failed: {error:#}");
                    options.cwd = old;
                    announce = None;
                    first = false;
                } else {
                    return Err(error);
                }
            }
        }
    }
}

async fn run_session(
    options: &ServeOptions,
    specs: &[Spec],
    listener: TcpListener,
    port: u16,
    token: &str,
    first: bool,
    announce: Option<String>,
) -> Result<Option<PathBuf>> {
    let actual = crate::projects::canonical(&options.cwd)?;
    if actual != options.cwd || !actual.is_dir() {
        anyhow::bail!("project directory changed or is not a directory");
    }
    let mut runtime_options = options.runtime_options();
    if let Some(log) = &mut runtime_options.log {
        if log.is_relative() {
            *log = options.cwd.join(&*log);
        }
    }
    if runtime_options.log.is_none() && !options.no_log {
        // The journal is on by default so a restart keeps history and the board.
        let dir = options.cwd.join(".agentsflow");
        std::fs::create_dir_all(&dir)?;
        runtime_options.log = Some(dir.join("events.jsonl"));
    }
    let mail_agent_tokens: MailAgentTokens = runtime_options
        .team
        .ids()
        .map(|id| (id.clone(), Uuid::new_v4().simple().to_string()))
        .collect();
    let mut secrets: Vec<String> = mail_agent_tokens.values().cloned().collect();
    secrets.push(token.to_owned());
    if let Some(config) = &options.telegram {
        secrets.push(config.token().to_owned());
    }
    crate::diag::init(&options.cwd, secrets)?;
    runtime_options.mail = Some(agentsflow_adapters::MailConfig {
        url: format!("http://127.0.0.1:{port}"),
        token: String::new(),
        exe: std::env::current_exe()?,
        thread_limit: options.mail_thread_limit,
        id: String::new(),
        peers: Vec::new(),
    });
    runtime_options.mail_agent_tokens = mail_agent_tokens.clone();
    let runtime = Runtime::new(runtime_options)?;
    let manager = Manager::new(
        runtime.hub.clone(),
        runtime.agents.clone(),
        runtime.team.clone(),
    );
    let (switch_tx, mut switch_rx) = tokio::sync::mpsc::unbounded_channel();
    let projects = crate::projects::Control {
        roots: options.projects.clone(),
        current: options.cwd.clone(),
        hub: runtime.hub.clone(),
        agents: runtime.agents.clone(),
        manager: manager.clone(),
        tx: switch_tx,
    };
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let telegram_stop = shutdown_rx.clone();
    let specs_for_telegram = specs.to_vec();
    let state = AppState {
        projects: Some(projects.clone()),
        hub: runtime.hub.clone(),
        token: token.to_owned(),
        mail_agent_tokens,
        port,
        cwd: runtime.cwd.to_string_lossy().into_owned(),
        agents: runtime.agents.clone(),
        team: runtime.team.clone(),
        shutdown: shutdown_rx,
        workflows: specs.to_vec(),
        manager: manager.clone(),
    };
    if first {
        println!("agentsflow: http://127.0.0.1:{port}/?token={token}");
    }
    crate::diag::diag("INFO", &format!("проект: {}", options.cwd.display()));
    crate::diag::diag("INFO", &crate::team::format_compact(&runtime.team));
    let telegram_task = options.telegram.clone().map(|config| {
        tokio::task::spawn_local(crate::telegram::run(
            config,
            runtime.hub.clone(),
            runtime.agents.clone(),
            runtime.team.clone(),
            Some(crate::telegram::Control {
                specs: specs_for_telegram,
                manager: manager.clone(),
                projects: Some(projects),
                announce,
            }),
            telegram_stop,
        ))
    });
    let (end_tx, end_rx) = tokio::sync::oneshot::channel();
    let result = axum::serve(listener, app(state))
        .with_graceful_shutdown(async move {
            let switch_to = tokio::select! { result = tokio::signal::ctrl_c() => { if let Err(error) = result { crate::server_diag!("signal: {error}"); } None }, path = switch_rx.recv() => path };
            let _ = shutdown_tx.send(true);
            let _ = end_tx.send(switch_to);
        })
        .await;
    if let Some(task) = telegram_task {
        let _ = task.await;
    }
    manager.stop();
    while manager.state().running {
        tokio::task::yield_now().await;
    }
    runtime.shutdown().await;
    crate::diag::close();
    result?;
    Ok(end_rx.await.unwrap_or(None))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn project_request(
        port: u16,
        token: &str,
        method: &str,
        path: &str,
        body: &str,
    ) -> (u16, Value) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nx-agentsflow-token: {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let text = String::from_utf8(response).unwrap();
        let (headers, body) = text.split_once("\r\n\r\n").unwrap();
        let status = headers
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        (status, serde_json::from_str(body).unwrap())
    }

    #[tokio::test]
    async fn project_switch_preserves_port_token_and_separates_history() {
        LocalSet::new()
            .run_until(async {
                let root =
                    std::env::temp_dir().join(format!("agentsflow-switch-{}", Uuid::new_v4()));
                let a = root.join("a");
                let b = root.join("b");
                let outside = root.join("outside");
                for path in [&a, &b, &outside] {
                    std::fs::create_dir_all(path).unwrap();
                }
                std::fs::create_dir(a.join(".git")).unwrap();
                std::fs::create_dir(b.join(".git")).unwrap();
                let mut options = ServeOptions::parse(&[]).unwrap();
                options.cwd = crate::projects::canonical(&a).unwrap();
                options.projects = vec![root.clone()];
                let b = crate::projects::canonical(&b).unwrap();
                let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
                    .await
                    .unwrap();
                let port = listener.local_addr().unwrap().port();
                let listener = listener.into_std().unwrap();
                let token = "test-token";
                let one_options = options.clone();
                let socket = TcpListener::from_std(listener.try_clone().unwrap()).unwrap();
                let first = tokio::task::spawn_local(async move {
                    run_session(&one_options, &[], socket, port, token, false, None).await
                });
                let (_, list) = project_request(port, token, "GET", "/api/projects", "").await;
                assert_eq!(list["projects"].as_array().unwrap().len(), 2);
                let body = json!({"path":outside}).to_string();
                assert_eq!(
                    project_request(port, token, "POST", "/api/project/switch", &body)
                        .await
                        .0,
                    404
                );
                let body = json!({"path":a}).to_string();
                let (status, same) =
                    project_request(port, token, "POST", "/api/project/switch", &body).await;
                assert_eq!(status, 200);
                assert_eq!(same["unchanged"], true);
                assert_eq!(
                    project_request(
                        port,
                        token,
                        "POST",
                        "/api/tasks",
                        r#"{"title":"first project"}"#
                    )
                    .await
                    .0,
                    201
                );
                let body = json!({"path":b}).to_string();
                assert_eq!(
                    project_request(port, token, "POST", "/api/project/switch", &body)
                        .await
                        .0,
                    202
                );
                assert_eq!(first.await.unwrap().unwrap(), Some(b.clone()));
                options.cwd = b.clone();
                let socket = TcpListener::from_std(listener.try_clone().unwrap()).unwrap();
                let second = tokio::task::spawn_local(async move {
                    run_session(&options, &[], socket, port, token, false, None).await
                });
                let (status, state) = project_request(port, token, "GET", "/api/state", "").await;
                assert_eq!(status, 200);
                assert_eq!(state["cwd"], b.to_string_lossy().as_ref());
                let (_, tasks) = project_request(port, token, "GET", "/api/tasks", "").await;
                assert!(tasks["tasks"].as_array().unwrap().is_empty());
                assert!(b.join(".agentsflow/events.jsonl").exists());
                let body = json!({"path":a}).to_string();
                assert_eq!(
                    project_request(port, token, "POST", "/api/project/switch", &body)
                        .await
                        .0,
                    202
                );
                second.await.unwrap().unwrap();
                drop(listener);
                std::fs::remove_dir_all(root).unwrap();
            })
            .await;
    }

    #[tokio::test]
    async fn task_routes_and_agent_permissions() {
        LocalSet::new()
            .run_until(async {
                let runtime = Runtime::new(RuntimeOptions::default()).unwrap();
                let manager = Manager::new(
                    runtime.hub.clone(),
                    runtime.agents.clone(),
                    runtime.team.clone(),
                );
                let (_tx, shutdown) = watch::channel(false);
                let state = AppState {
                    projects: None,
                    hub: runtime.hub.clone(),
                    token: "main".into(),
                    mail_agent_tokens: MailAgentTokens::new(),
                    port: 1,
                    cwd: ".".into(),
                    agents: runtime.agents.clone(),
                    team: runtime.team.clone(),
                    shutdown,
                    workflows: vec![],
                    manager,
                };
                let stats = serde_json::to_value(get_stats(State(state.clone())).await.0).unwrap();
                assert!(stats["agents"]["claude"].is_object());
                assert!(stats["agents"]["orchestrator"].is_object());
                let make = |assignee: Option<&str>| NewTask {
                    title: "task".into(),
                    description: "work".into(),
                    assignee: assignee.map(AgentId::from),
                    status: None,
                    labels: vec![],
                };
                assert_eq!(
                    create_task(State(state.clone()), Ok(Json(make(Some("unknown")))))
                        .await
                        .status(),
                    StatusCode::BAD_REQUEST
                );
                assert_eq!(
                    create_task(State(state.clone()), Ok(Json(make(Some("claude")))))
                        .await
                        .status(),
                    StatusCode::CREATED
                );
                let task = runtime.hub.tasks(false).remove(0);
                assert_eq!(
                    dispatch_task(State(state.clone()), axum::extract::Path(task.id.clone()))
                        .await
                        .status(),
                    StatusCode::CONFLICT
                );
                runtime.hub.set_mail_enabled(true);
                let result =
                    dispatch_task(State(state.clone()), axum::extract::Path(task.id.clone())).await;
                assert_eq!(result.status(), StatusCode::OK);
                assert_eq!(
                    runtime.hub.task(&task.id).unwrap().status,
                    agentsflow_events::TaskStatus::InProgress
                );
                assert_eq!(
                    runtime.hub.unread(AgentId::from("claude"))[0].thread,
                    runtime.hub.task(&task.id).unwrap().thread.unwrap()
                );
                let claude = Identity::Agent(AgentId::from("claude"));
                let codex = Identity::Agent(AgentId::from("codex"));
                let listed = list_tasks(State(state.clone()), Query(HashMap::new())).await;
                let bytes = axum::body::to_bytes(listed.into_body(), 65536)
                    .await
                    .unwrap();
                assert_eq!(
                    serde_json::from_slice::<Value>(&bytes).unwrap()["tasks"]
                        .as_array()
                        .unwrap()
                        .len(),
                    1
                );
                let mine = my_tasks(State(state.clone()), Extension(claude.clone())).await;
                let bytes = axum::body::to_bytes(mine.into_body(), 65536).await.unwrap();
                assert_eq!(
                    serde_json::from_slice::<Value>(&bytes).unwrap()["tasks"]
                        .as_array()
                        .unwrap()
                        .len(),
                    1
                );
                let mine = my_tasks(State(state.clone()), Extension(codex.clone())).await;
                let bytes = axum::body::to_bytes(mine.into_body(), 65536).await.unwrap();
                assert_eq!(
                    serde_json::from_slice::<Value>(&bytes).unwrap()["tasks"]
                        .as_array()
                        .unwrap()
                        .len(),
                    0
                );
                let update = |status: &str| Ok(Json(json!({"status":status})));
                assert_eq!(
                    update_my_task(
                        State(state.clone()),
                        axum::extract::Path(task.id.clone()),
                        Extension(codex),
                        update("review")
                    )
                    .await
                    .status(),
                    StatusCode::FORBIDDEN
                );
                assert_eq!(
                    update_my_task(
                        State(state.clone()),
                        axum::extract::Path(task.id.clone()),
                        Extension(claude.clone()),
                        update("archived")
                    )
                    .await
                    .status(),
                    StatusCode::FORBIDDEN
                );
                assert_eq!(
                    update_my_task(
                        State(state.clone()),
                        axum::extract::Path(task.id.clone()),
                        Extension(claude.clone()),
                        Ok(Json(json!({"title":"bad"})))
                    )
                    .await
                    .status(),
                    StatusCode::FORBIDDEN
                );
                assert_eq!(
                    update_my_task(
                        State(state.clone()),
                        axum::extract::Path(task.id.clone()),
                        Extension(claude.clone()),
                        update("done")
                    )
                    .await
                    .status(),
                    StatusCode::FORBIDDEN
                );
                assert_eq!(
                    update_my_task(
                        State(state.clone()),
                        axum::extract::Path(task.id.clone()),
                        Extension(claude),
                        update("review")
                    )
                    .await
                    .status(),
                    StatusCode::OK
                );
                assert!(!authorize(
                    Identity::Agent(AgentId::from("claude")),
                    &axum::http::Method::PATCH,
                    &format!("/api/tasks/{}", task.id)
                ));
                assert!(authorize(
                    Identity::Agent(AgentId::from("claude")),
                    &axum::http::Method::GET,
                    "/api/tasks/mine"
                ));
                assert!(authorize(
                    Identity::Agent(AgentId::from("claude")),
                    &axum::http::Method::POST,
                    &format!("/api/tasks/{}/update", task.id)
                ));
                let patch: TaskPatch = serde_json::from_value(json!({"assignee":null})).unwrap();
                assert_eq!(
                    patch_task(
                        State(state.clone()),
                        axum::extract::Path(task.id.clone()),
                        Ok(Json(patch))
                    )
                    .await
                    .status(),
                    StatusCode::OK
                );
                assert_eq!(
                    dispatch_task(State(state), axum::extract::Path(task.id))
                        .await
                        .status(),
                    StatusCode::CONFLICT
                );
                runtime.shutdown().await;
            })
            .await;
    }

    #[test]
    fn personal_token_access() {
        let tokens = MailAgentTokens::from([
            (AgentId::from("claude"), "claude-token".into()),
            (AgentId::from("codex"), "codex-token".into()),
        ]);
        let claude = authenticate("claude-token", "", "main-token", &tokens).unwrap();
        let codex = authenticate("", "codex-token", "main-token", &tokens).unwrap();
        assert_eq!(claude, Identity::Agent(AgentId::from("claude")));
        assert_eq!(codex, Identity::Agent(AgentId::from("codex")));
        assert_ne!(claude, codex);
        assert_eq!(authenticate("bad", "", "main-token", &tokens), None);
        assert_eq!(
            authenticate("bad", "main-token", "main-token", &tokens),
            Some(Identity::Main)
        );
        assert_eq!(
            authenticate("claude-token", "main-token", "main-token", &tokens),
            Some(claude.clone())
        );
        for identity in [claude, codex] {
            for path in ["/api/mail/send", "/api/mail/inbox"] {
                assert!(authorize(identity.clone(), &axum::http::Method::POST, path));
                assert!(!authorize(identity.clone(), &axum::http::Method::GET, path));
            }
            for (method, path) in [
                (axum::http::Method::POST, "/api/prompt"),
                (axum::http::Method::GET, "/api/state"),
                (axum::http::Method::GET, "/api/stats"),
                (axum::http::Method::GET, "/api/health"),
                (axum::http::Method::GET, "/api/projects"),
                (axum::http::Method::POST, "/api/project/switch"),
                (axum::http::Method::POST, "/api/agent/restart"),
                (axum::http::Method::GET, "/api/export"),
                (axum::http::Method::POST, "/api/permission"),
                (axum::http::Method::POST, "/api/workflow/run"),
            ] {
                assert!(!authorize(identity.clone(), &method, path));
                assert!(authorize(Identity::Main, &method, path));
            }
        }
        for (method, path) in [
            (axum::http::Method::POST, "/api/prompt"),
            (axum::http::Method::POST, "/api/agent/interrupt"),
            (axum::http::Method::GET, "/api/state"),
            (axum::http::Method::GET, "/api/stats"),
            (axum::http::Method::GET, "/api/health"),
            (axum::http::Method::GET, "/api/projects"),
            (axum::http::Method::POST, "/api/project/switch"),
            (axum::http::Method::POST, "/api/agent/restart"),
            (axum::http::Method::GET, "/api/export"),
            (axum::http::Method::POST, "/api/permission"),
            (axum::http::Method::POST, "/api/workflow/run"),
        ] {
            assert_eq!(
                authorize_request("claude-token", "", "main-token", &tokens, &method, path),
                Err(StatusCode::FORBIDDEN)
            );
            assert_eq!(
                authorize_request("bad", "", "main-token", &tokens, &method, path),
                Err(StatusCode::UNAUTHORIZED)
            );
            assert_eq!(
                authorize_request("", "main-token", "main-token", &tokens, &method, path),
                Ok(Identity::Main)
            );
        }
        assert_eq!(
            authenticate("", "", "main-token", &MailAgentTokens::default()),
            None
        );
    }

    struct NoAgent;
    impl agentsflow_adapters::Agent for NoAgent {
        async fn send(&mut self, _: &str) -> Result<()> {
            Ok(())
        }
        async fn next_events(&mut self) -> Result<Vec<agentsflow_events::Event>> {
            std::future::pending().await
        }
        async fn respond(&mut self, _: &str, _: bool) -> Result<()> {
            Ok(())
        }
        async fn shutdown(self) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn interrupt_statuses() {
        LocalSet::new()
            .run_until(async {
                let hub = Hub::new();
                let (actor, task) =
                    crate::hub::spawn(hub.clone(), AgentId::from("claude"), false, || async {
                        Ok(crate::testing::Silent::new(1, false))
                    });
                let agents = Agents::from([(AgentId::from("claude"), actor.clone())]);
                let team = Team::preset("pair").unwrap();
                let manager = Manager::new(hub.clone(), agents.clone(), team.clone());
                let (_shutdown, shutdown) = watch::channel(false);
                let state = AppState {
                    projects: None,
                    hub,
                    token: "main".into(),
                    mail_agent_tokens: MailAgentTokens::default(),
                    port: 1,
                    cwd: ".".into(),
                    agents,
                    team,
                    shutdown,
                    workflows: vec![],
                    manager,
                };
                let request = |agent: &str| {
                    Json(Interrupt {
                        agent: agent.into(),
                    })
                };
                let response = interrupt(State(state.clone()), request("missing")).await;
                assert_eq!(response.status(), StatusCode::NOT_FOUND);
                let response = interrupt(State(state.clone()), request("claude")).await;
                assert_eq!(response.status(), StatusCode::CONFLICT);
                let body = axum::body::to_bytes(response.into_body(), 1024)
                    .await
                    .unwrap();
                assert_eq!(
                    serde_json::from_slice::<Value>(&body).unwrap(),
                    json!({"error":"not busy"})
                );
                actor.prompt("hello".into()).await.unwrap();
                let response = interrupt(State(state), request("claude")).await;
                assert_eq!(response.status(), StatusCode::OK);
                let body = axum::body::to_bytes(response.into_body(), 1024)
                    .await
                    .unwrap();
                assert_eq!(
                    serde_json::from_slice::<Value>(&body).unwrap(),
                    json!({"ok":true})
                );
                actor.shutdown().await;
                task.await.unwrap();
            })
            .await;
    }

    #[tokio::test]
    async fn restart_and_health_statuses() {
        LocalSet::new()
            .run_until(async {
                let hub = Hub::new();
                let (actor, task) =
                    crate::hub::spawn(hub.clone(), AgentId::from("claude"), false, || async {
                        Ok(NoAgent)
                    });
                let agents = Agents::from([(AgentId::from("claude"), actor.clone())]);
                let team = Team::preset("pair").unwrap();
                let manager = Manager::new(hub.clone(), agents.clone(), team.clone());
                let (_tx, shutdown) = watch::channel(false);
                let state = AppState {
                    projects: None,
                    hub,
                    token: "main".into(),
                    mail_agent_tokens: MailAgentTokens::default(),
                    port: 1,
                    cwd: ".".into(),
                    agents,
                    team,
                    shutdown,
                    workflows: vec![],
                    manager,
                };
                let request = |name: &str| Json(Interrupt { agent: name.into() });
                assert_eq!(
                    restart(State(state.clone()), request("missing"))
                        .await
                        .status(),
                    StatusCode::NOT_FOUND
                );
                assert_eq!(
                    restart(State(state.clone()), request("claude"))
                        .await
                        .status(),
                    StatusCode::OK
                );
                let value = health(State(state.clone())).await.0;
                assert_eq!(value["ok"], true);
                assert_eq!(value["agents"]["claude"]["last_error"], Value::Null);
                actor.prompt("work".into()).await.unwrap();
                assert_eq!(
                    restart(State(state), request("claude")).await.status(),
                    StatusCode::CONFLICT
                );
                actor.shutdown().await;
                task.await.unwrap();
            })
            .await;
    }

    #[tokio::test]
    async fn project_switch_http_rejects_disabled_and_busy() {
        LocalSet::new()
            .run_until(async {
                let root = std::env::temp_dir()
                    .join(format!("agentsflow-project-http-{}", Uuid::new_v4()));
                let a = root.join("a");
                let b = root.join("b");
                for path in [&a, &b] {
                    std::fs::create_dir_all(path.join(".git")).unwrap();
                }
                let hub = Hub::new();
                let (actor, task) =
                    crate::hub::spawn(hub.clone(), AgentId::from("claude"), false, || async {
                        Ok(NoAgent)
                    });
                let agents = Agents::from([(AgentId::from("claude"), actor.clone())]);
                let team = Team::preset("pair").unwrap();
                let manager = Manager::new(hub.clone(), agents.clone(), team.clone());
                let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
                let control = crate::projects::Control {
                    roots: vec![],
                    current: a,
                    hub: hub.clone(),
                    agents: agents.clone(),
                    manager: manager.clone(),
                    tx,
                };
                let (_shutdown, shutdown) = watch::channel(false);
                let mut state = AppState {
                    projects: Some(control),
                    hub,
                    token: "main".into(),
                    mail_agent_tokens: MailAgentTokens::new(),
                    port: 1,
                    cwd: ".".into(),
                    agents,
                    team,
                    shutdown,
                    workflows: vec![],
                    manager,
                };
                assert_eq!(
                    switch_project(
                        State(state.clone()),
                        Json(SwitchProject { path: b.clone() })
                    )
                    .await
                    .status(),
                    StatusCode::NOT_FOUND
                );
                state.projects.as_mut().unwrap().roots.push(root.clone());
                actor.prompt("work".into()).await.unwrap();
                let response = switch_project(State(state), Json(SwitchProject { path: b })).await;
                assert_eq!(response.status(), StatusCode::CONFLICT);
                let bytes = axum::body::to_bytes(response.into_body(), 1024)
                    .await
                    .unwrap();
                assert_eq!(
                    serde_json::from_slice::<Value>(&bytes).unwrap()["error"],
                    "Сначала остановите работу агентов"
                );
                actor.shutdown().await;
                task.await.unwrap();
                std::fs::remove_dir_all(root).unwrap();
            })
            .await;
    }

    #[tokio::test]
    async fn personal_mail_identity() {
        LocalSet::new()
            .run_until(async {
                let hub = Hub::new();
                let (claude, claude_task) =
                    crate::hub::spawn(hub.clone(), AgentId::from("claude"), false, || async {
                        Ok(NoAgent)
                    });
                let (codex, codex_task) =
                    crate::hub::spawn(hub.clone(), AgentId::from("codex"), false, || async {
                        Ok(NoAgent)
                    });
                let manager = Manager::new(
                    hub.clone(),
                    Agents::from([
                        (AgentId::from("claude"), claude.clone()),
                        (AgentId::from("codex"), codex.clone()),
                    ]),
                    Team::preset("pair").unwrap(),
                );
                let (_shutdown, shutdown) = watch::channel(false);
                let state = AppState {
                    projects: None,
                    hub,
                    token: "main".into(),
                    mail_agent_tokens: MailAgentTokens::default(),
                    port: 7878,
                    cwd: ".".into(),
                    agents: Agents::from([
                        (AgentId::from("claude"), claude.clone()),
                        (AgentId::from("codex"), codex.clone()),
                    ]),
                    team: Team::preset("pair").unwrap(),
                    shutdown,
                    workflows: Vec::new(),
                    manager,
                };
                let make = |from: &str, to: &str| SendMail {
                    from: from.into(),
                    to: to.into(),
                    subject: "subject".into(),
                    body: "body".into(),
                    thread: None,
                };
                let c = Identity::Agent(AgentId::from("claude"));
                let d = Identity::Agent(AgentId::from("codex"));
                assert_eq!(
                    send_mail(
                        State(state.clone()),
                        Extension(c.clone()),
                        Ok(Json(make("claude", "codex")))
                    )
                    .await
                    .status(),
                    StatusCode::OK
                );
                let denied = send_mail(
                    State(state.clone()),
                    Extension(c.clone()),
                    Ok(Json(make("codex", "claude"))),
                )
                .await;
                assert_eq!(denied.status(), StatusCode::FORBIDDEN);
                let body = axum::body::to_bytes(denied.into_body(), 1024)
                    .await
                    .unwrap();
                assert_eq!(
                    serde_json::from_slice::<Value>(&body).unwrap()["error"],
                    "sender does not match token"
                );
                assert_eq!(
                    send_mail(
                        State(state.clone()),
                        Extension(d.clone()),
                        Ok(Json(make("claude", "codex")))
                    )
                    .await
                    .status(),
                    StatusCode::FORBIDDEN
                );
                assert_eq!(
                    inbox(
                        State(state.clone()),
                        Extension(c.clone()),
                        Ok(Json(Inbox {
                            agent: "codex".into()
                        }))
                    )
                    .await
                    .status(),
                    StatusCode::FORBIDDEN
                );
                assert_eq!(
                    inbox(
                        State(state.clone()),
                        Extension(d.clone()),
                        Ok(Json(Inbox {
                            agent: "claude".into()
                        }))
                    )
                    .await
                    .status(),
                    StatusCode::FORBIDDEN
                );
                assert_eq!(
                    inbox(
                        State(state.clone()),
                        Extension(c.clone()),
                        Ok(Json(Inbox {
                            agent: "claude".into()
                        }))
                    )
                    .await
                    .status(),
                    StatusCode::OK
                );
                assert_eq!(
                    inbox(
                        State(state.clone()),
                        Extension(d.clone()),
                        Ok(Json(Inbox {
                            agent: "codex".into()
                        }))
                    )
                    .await
                    .status(),
                    StatusCode::OK
                );
                assert_eq!(
                    send_mail(
                        State(state),
                        Extension(Identity::Main),
                        Ok(Json(make("orchestrator", "codex")))
                    )
                    .await
                    .status(),
                    StatusCode::OK
                );
                claude.shutdown().await;
                codex.shutdown().await;
                claude_task.await.unwrap();
                codex_task.await.unwrap();
            })
            .await;
    }
    #[tokio::test]
    async fn workflow_with_unknown_agent_returns_bad_request() {
        LocalSet::new()
            .run_until(async {
                let runtime = Runtime::new(RuntimeOptions {
                    team: Team::preset("trio").unwrap(),
                    ..Default::default()
                })
                .unwrap();
                let manager = Manager::new(
                    runtime.hub.clone(),
                    runtime.agents.clone(),
                    runtime.team.clone(),
                );
                let (_stop, shutdown) = watch::channel(false);
                let spec =
                    Spec::parse("name='x'\n[[step]]\nid='s'\nagent='missing'\nprompt='x'").unwrap();
                let role_spec =
                    Spec::parse("name='role'\n[[step]]\nid='s'\nagent='@missing'\nprompt='x'")
                        .unwrap();
                let state = AppState {
                    projects: None,
                    hub: runtime.hub.clone(),
                    token: "main".into(),
                    mail_agent_tokens: MailAgentTokens::new(),
                    port: 1,
                    cwd: ".".into(),
                    agents: runtime.agents.clone(),
                    team: runtime.team.clone(),
                    shutdown,
                    workflows: vec![spec, role_spec],
                    manager: manager.clone(),
                };
                let response = run_workflow(
                    State(state.clone()),
                    Json(RunWorkflow {
                        name: "x".into(),
                        task: "task".into(),
                    }),
                )
                .await;
                assert_eq!(response.status(), StatusCode::BAD_REQUEST);
                let body = axum::body::to_bytes(response.into_body(), 1024)
                    .await
                    .unwrap();
                assert_eq!(
                    serde_json::from_slice::<Value>(&body).unwrap()["error"],
                    "step s: agent missing is not in the team (members: lead, main, junior)"
                );
                let response = run_workflow(
                    State(state),
                    Json(RunWorkflow {
                        name: "role".into(),
                        task: "task".into(),
                    }),
                )
                .await;
                assert_eq!(response.status(), StatusCode::BAD_REQUEST);
                let body = axum::body::to_bytes(response.into_body(), 1024)
                    .await
                    .unwrap();
                assert!(
                    serde_json::from_slice::<Value>(&body).unwrap()["error"]
                        .as_str()
                        .unwrap()
                        .contains("no team member has role missing")
                );
                assert!(!manager.state().running);
                runtime.shutdown().await;
            })
            .await;
    }
    #[tokio::test]
    async fn index_page_shows_version_from_state() {
        let page = index().await.0;
        assert!(page.contains(r#"<span class="meta" id="app-version"></span>"#));
        assert!(page.contains(r##"$("#app-version").textContent = "v" + s.version"##));
    }
    #[tokio::test]
    async fn trio_state_and_personal_mail_access() {
        LocalSet::new()
            .run_until(async {
                let runtime = Runtime::new(RuntimeOptions {
                    team: Team::preset("trio").unwrap(),
                    ..Default::default()
                })
                .unwrap();
                let tokens: MailAgentTokens = runtime
                    .team
                    .ids()
                    .map(|id| (id.clone(), format!("token-{id}")))
                    .collect();
                let manager = Manager::new(
                    runtime.hub.clone(),
                    runtime.agents.clone(),
                    runtime.team.clone(),
                );
                let (_stop, shutdown) = watch::channel(false);
                let state = AppState {
                    projects: None,
                    hub: runtime.hub.clone(),
                    token: "main".into(),
                    mail_agent_tokens: tokens.clone(),
                    port: 7878,
                    cwd: ".".into(),
                    agents: runtime.agents.clone(),
                    team: runtime.team.clone(),
                    shutdown,
                    workflows: vec![],
                    manager,
                };
                let body = get_state(State(state.clone())).await.0;
                assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
                assert_eq!(
                    body["team"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|v| v["id"].as_str().unwrap())
                        .collect::<Vec<_>>(),
                    ["lead", "main", "junior"]
                );
                for id in ["lead", "main", "junior"] {
                    assert_eq!(body["agents"][id]["enabled"], true);
                    assert_eq!(
                        authenticate(&format!("token-{id}"), "", "main", &tokens),
                        Some(Identity::Agent(AgentId::from(id)))
                    );
                }
                assert_eq!(get_team(State(state.clone())).await.0, body["team"]);
                assert_eq!(body["team"][0]["mode"], "smart");
                assert_eq!(body["team"][0]["guard"], true);
                assert!(body["team"][0]["sandbox"].is_null());
                assert_eq!(body["team"][1]["sandbox"], "workspace-write");
                assert_eq!(body["team"][2]["sandbox"], "workspace-write");
                assert_eq!(
                    authorize_request(
                        "token-lead",
                        "",
                        "main",
                        &tokens,
                        &axum::http::Method::GET,
                        "/api/team"
                    ),
                    Err(StatusCode::FORBIDDEN)
                );
                assert_eq!(
                    authorize_request(
                        "main",
                        "",
                        "main",
                        &tokens,
                        &axum::http::Method::GET,
                        "/api/team"
                    ),
                    Ok(Identity::Main)
                );
                let mail = |from: &str, to: &str| SendMail {
                    from: from.into(),
                    to: to.into(),
                    subject: "s".into(),
                    body: "b".into(),
                    thread: None,
                };
                assert_eq!(
                    send_mail(
                        State(state.clone()),
                        Extension(Identity::Agent(AgentId::from("lead"))),
                        Ok(Json(mail("junior", "main")))
                    )
                    .await
                    .status(),
                    StatusCode::FORBIDDEN
                );
                assert_eq!(
                    send_mail(
                        State(state.clone()),
                        Extension(Identity::Agent(AgentId::from("lead"))),
                        Ok(Json(mail("lead", "main")))
                    )
                    .await
                    .status(),
                    StatusCode::OK
                );
                assert_eq!(
                    inbox(
                        State(state.clone()),
                        Extension(Identity::Agent(AgentId::from("junior"))),
                        Ok(Json(Inbox {
                            agent: "main".into()
                        }))
                    )
                    .await
                    .status(),
                    StatusCode::FORBIDDEN
                );
                assert_eq!(
                    send_mail(
                        State(state.clone()),
                        Extension(Identity::Main),
                        Ok(Json(mail("orchestrator", "unknown")))
                    )
                    .await
                    .status(),
                    StatusCode::BAD_REQUEST
                );
                assert_eq!(
                    prompt(
                        State(state.clone()),
                        Json(Prompt {
                            agent: "unknown".into(),
                            text: "x".into()
                        })
                    )
                    .await
                    .status(),
                    StatusCode::NOT_FOUND
                );
                runtime.shutdown().await;
            })
            .await;
    }
    #[tokio::test]
    async fn team_api_exposes_effective_bypass() {
        LocalSet::new()
            .run_until(async {
                let mut team = Team::preset("pair").unwrap();
                team.members[0].mode = None;
                team.members[1].mode = Some(crate::team::AccessMode::Bypass);
                let runtime = Runtime::new(RuntimeOptions {
                    team,
                    allow_bypass: true,
                    yes: true,
                    ..Default::default()
                })
                .unwrap();
                let manager = Manager::new(
                    runtime.hub.clone(),
                    runtime.agents.clone(),
                    runtime.team.clone(),
                );
                let (_tx, shutdown) = watch::channel(false);
                let state = AppState {
                    projects: None,
                    hub: runtime.hub.clone(),
                    token: "main".into(),
                    mail_agent_tokens: MailAgentTokens::new(),
                    port: 1,
                    cwd: ".".into(),
                    agents: runtime.agents.clone(),
                    team: runtime.team.clone(),
                    shutdown,
                    workflows: vec![],
                    manager,
                };
                let team = get_team(State(state)).await.0;
                assert_eq!(team[0]["mode"], "auto");
                assert_eq!(team[1]["mode"], "bypass");
                assert!(team[1]["sandbox"].is_null());
                runtime.shutdown().await;
            })
            .await;
    }
    #[test]
    fn security_checks() {
        assert!(verify_token("012345", "012345"));
        assert!(!verify_token("012345", "012346"));
        assert!(!verify_token("012345", "0123456"));
        assert!(host_allowed("127.0.0.1:7878", 7878));
        assert!(host_allowed("localhost:7878", 7878));
        assert!(!host_allowed("localhost:80", 7878));
        assert!(!host_allowed("evil:7878", 7878));
        assert!(origin_allowed("http://localhost:7878", "localhost:7878"));
        assert!(!origin_allowed("http://127.0.0.1:7878", "localhost:7878"));
        let pair = Team::preset("pair").unwrap();
        assert_eq!(parse_agent("claude", &pair), Some(AgentId::from("claude")));
        assert_eq!(parse_agent("codex", &pair), Some(AgentId::from("codex")));
        assert_eq!(
            parse_agent("orchestrator", &pair),
            Some(AgentId::orchestrator())
        );
        assert_eq!(parse_agent("other", &pair), None);
        let codex_only = Team::filtered_pair(false, true).unwrap();
        assert_eq!(parse_agent("claude", &codex_only), None);
        assert_eq!(
            parse_agent("codex", &codex_only),
            Some(AgentId::from("codex"))
        );
        assert_eq!(
            parse_agent("orchestrator", &codex_only),
            Some(AgentId::orchestrator())
        );
    }
    #[test]
    fn parse_team_options() {
        let args = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let trio = ServeOptions::parse_checked(&args(&["--team", "trio"]))
            .unwrap()
            .unwrap();
        assert_eq!(trio.runtime_options().team, Team::preset("trio").unwrap());
        let path = std::env::temp_dir().join(format!("agentsflow-team-{}.toml", Uuid::new_v4()));
        std::fs::write(&path, crate::team::TEMPLATE).unwrap();
        let file = ServeOptions::parse_checked(&args(&["--team", path.to_str().unwrap()]))
            .unwrap()
            .unwrap();
        assert_eq!(file.runtime_options().team, Team::preset("trio").unwrap());
        std::fs::remove_file(path).unwrap();
        assert!(
            format!(
                "{:#}",
                ServeOptions::parse_checked(&args(&["--team", "nonexistent-preset"]))
                    .err()
                    .unwrap()
            )
            .contains("cannot read team file")
        );
        for items in [
            ["--team", "pair", "--agents", "codex"],
            ["--agents", "codex", "--team", "pair"],
        ] {
            assert!(
                ServeOptions::parse_checked(&args(&items))
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("cannot be used together")
            );
        }
    }
    #[test]
    fn bypass_requires_explicit_flag() {
        let path = std::env::temp_dir().join(format!("agentsflow-bypass-{}.toml", Uuid::new_v4()));
        std::fs::write(
            &path,
            "[[agent]]\nid='lead'\nkind='claude'\nlead=true\nmode='bypass'\n",
        )
        .unwrap();
        let mut args = vec!["--team".to_owned(), path.to_string_lossy().into_owned()];
        let error = ServeOptions::parse_checked(&args)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("lead") && error.contains("--allow-bypass"));
        args.push("--allow-bypass".into());
        args.push("--allow-bypass".to_owned());
        assert!(ServeOptions::parse_checked(&args).unwrap().is_some());
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn parse_serve_options() {
        let args = |xs: &[&str]| xs.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let opts = ServeOptions::parse(&args(&[
            "--port",
            "0",
            "--log",
            "events.jsonl",
            "--agents",
            "codex",
            "--sandbox",
            "workspace-write",
        ]))
        .unwrap();
        assert_eq!(opts.port, 0);
        assert_eq!(
            opts.log.as_deref(),
            Some(std::path::Path::new("events.jsonl"))
        );
        assert!(ServeOptions::parse(&args(&["--log"])).is_none());
        assert!(!opts.claude && opts.codex);
        assert!(!opts.no_log);
        assert!(ServeOptions::parse(&args(&["--no-log"])).unwrap().no_log);
        assert!(ServeOptions::parse(&args(&["--agents", "claude,other"])).is_none());
        assert!(ServeOptions::parse(&args(&["--port", "99999"])).is_none());
    }

    #[test]
    fn parses_multiple_project_roots() {
        let args = ["--projects", "one", "--projects", "two"].map(str::to_owned);
        let options = ServeOptions::parse(&args).unwrap();
        assert_eq!(
            options.projects,
            vec![PathBuf::from("one"), PathBuf::from("two")]
        );
        assert!(ServeOptions::parse(&["--projects".into()]).is_none());
    }
    #[test]
    fn parse_mail_thread_limit() {
        let args = |xs: &[&str]| xs.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let default = ServeOptions::parse(&[]).unwrap();
        assert_eq!(default.mail_thread_limit, crate::hub::MAX_THREAD_MAILS);
        assert_eq!(
            default.runtime_options().mail_thread_limit,
            crate::hub::MAX_THREAD_MAILS
        );
        for limit in [2, 42, 100] {
            let opts =
                ServeOptions::parse(&args(&["--mail-thread-limit", &limit.to_string()])).unwrap();
            assert_eq!(opts.mail_thread_limit, limit);
            assert_eq!(opts.runtime_options().mail_thread_limit, limit);
        }
        for value in ["1", "101", "abc"] {
            assert!(ServeOptions::parse(&args(&["--mail-thread-limit", value])).is_none());
        }
        assert!(ServeOptions::parse(&args(&["--mail-thread-limit"])).is_none());
    }
    #[test]
    fn workflow_request_validation() {
        let specs = workflow::builtins().unwrap();
        let summaries = serde_json::json!(specs.iter().map(Spec::summary).collect::<Vec<_>>());
        assert!(
            summaries
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["name"] == "mail-team" && item["steps"][0]["mode"] == "mail")
        );
        assert!(
            summaries
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["name"] == "mail-review" && item["steps"][0]["mode"] == "mail")
        );
        assert!(validate_workflow_request(&specs, "implement-and-review", "task").is_ok());
        assert_eq!(
            validate_workflow_request(&specs, "implement-and-review", " \n").unwrap_err(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            validate_workflow_request(&specs, "missing", "task").unwrap_err(),
            StatusCode::NOT_FOUND
        );
    }
}
