use std::{collections::HashMap, convert::Infallible, path::PathBuf, time::Duration};

use crate::hub::{Actor, Hub, HubError};
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
    routing::{get, post},
};
use futures_util::{StreamExt, stream};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::watch, task::LocalSet};
use uuid::Uuid;

pub struct ServeOptions {
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
            mail_thread_limit: crate::hub::MAX_THREAD_MAILS,
        };
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            if arg == "--yes" {
                options.yes = true;
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
    (
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            (header::CONTENT_DISPOSITION, disposition),
        ],
        format(&snapshot),
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
        json!({"cwd": state.cwd, "agents": agents, "team": state.team.members, "workflow": state.manager.state()}),
    )
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

fn app(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/events", get(events))
        .route("/api/export", get(export))
        .route("/api/prompt", post(prompt))
        .route("/api/agent/interrupt", post(interrupt))
        .route("/api/permission", post(permission))
        .route("/api/mail/send", post(send_mail))
        .route("/api/mail/inbox", post(inbox))
        .route("/api/state", get(get_state))
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
async fn serve_local(options: ServeOptions, specs: Vec<Spec>) -> Result<()> {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, options.port)).await?;
    let port = listener.local_addr()?.port();
    let token = Uuid::new_v4().simple().to_string();
    let mut runtime_options = options.runtime_options();
    let mail_agent_tokens: MailAgentTokens = runtime_options
        .team
        .ids()
        .map(|id| (id.clone(), Uuid::new_v4().simple().to_string()))
        .collect();
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
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let telegram_stop = shutdown_rx.clone();
    let state = AppState {
        hub: runtime.hub.clone(),
        token: token.clone(),
        mail_agent_tokens,
        port,
        cwd: runtime.cwd.to_string_lossy().into_owned(),
        agents: runtime.agents.clone(),
        team: runtime.team.clone(),
        shutdown: shutdown_rx,
        workflows: specs,
        manager: manager.clone(),
    };
    println!("agentsflow: http://127.0.0.1:{port}/?token={token}");
    eprintln!("{}", crate::team::format_compact(&runtime.team));
    let telegram_task = options.telegram.map(|config| {
        tokio::task::spawn_local(crate::telegram::run(
            config,
            runtime.hub.clone(),
            runtime.agents.clone(),
            runtime.team.clone(),
            telegram_stop,
        ))
    });
    let result = axum::serve(listener, app(state))
        .with_graceful_shutdown(async move {
            if let Err(error) = tokio::signal::ctrl_c().await {
                eprintln!("signal: {error}");
            }
            let _ = shutdown_tx.send(true);
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
    result?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
                assert_eq!(body["team"][0]["mode"], "ask");
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
        assert!(ServeOptions::parse(&args(&["--agents", "claude,other"])).is_none());
        assert!(ServeOptions::parse(&args(&["--port", "99999"])).is_none());
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
