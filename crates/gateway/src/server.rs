use std::{collections::HashMap, convert::Infallible, path::PathBuf, time::Duration};

use crate::hub::{Actor, Hub, HubError};
use crate::{
    runtime::{MailAgentTokens, Runtime, RuntimeOptions},
    workflow::{self, Manager, Spec},
};
use agentsflow_adapters::codex::SandboxMode;
use agentsflow_events::AgentKind;
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
    claude_model: Option<String>,
    codex_model: Option<String>,
    sandbox: Option<SandboxMode>,
    codex_bin: Option<PathBuf>,
    yes: bool,
    workflows: Vec<PathBuf>,
    log: Option<PathBuf>,
}
impl ServeOptions {
    pub fn parse(args: &[String]) -> Option<Self> {
        let mut options = Self {
            port: 7878,
            cwd: std::env::current_dir().ok()?,
            claude: true,
            codex: true,
            claude_model: None,
            codex_model: None,
            sandbox: None,
            codex_bin: None,
            yes: false,
            workflows: Vec::new(),
            log: None,
        };
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            if arg == "--yes" {
                options.yes = true;
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
            claude: self.claude,
            codex: self.codex,
            claude_model: self.claude_model.clone(),
            codex_model: self.codex_model.clone(),
            sandbox: self.sandbox.clone(),
            codex_bin: self.codex_bin.clone(),
            yes: self.yes,
            log: self.log.clone(),
            mail: None,
            mail_agent_tokens: MailAgentTokens::default(),
            mail_thread_limit: crate::hub::MAX_THREAD_MAILS,
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
    claude: Option<Actor>,
    codex: Option<Actor>,
    shutdown: watch::Receiver<bool>,
    workflows: Vec<Spec>,
    manager: Manager,
}
impl AppState {
    fn actor(&self, agent: &str) -> Option<&Actor> {
        match agent {
            "claude" => self.claude.as_ref(),
            "codex" => self.codex.as_ref(),
            _ => None,
        }
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

#[derive(Clone, Copy, Debug, PartialEq)]
enum Identity {
    Main,
    Agent(AgentKind),
}

fn identify(token: &str, main: &str, agents: &MailAgentTokens) -> Option<Identity> {
    if token.is_empty() {
        return None;
    }
    if verify_token(main, token) {
        return Some(Identity::Main);
    }
    if agents
        .claude
        .as_deref()
        .is_some_and(|expected| verify_token(expected, token))
    {
        return Some(Identity::Agent(AgentKind::Claude));
    }
    if agents
        .codex
        .as_deref()
        .is_some_and(|expected| verify_token(expected, token))
    {
        return Some(Identity::Agent(AgentKind::Codex));
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
    if !authorize(identity, method, path) {
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
fn parse_agent(name: &str) -> Option<agentsflow_events::AgentKind> {
    match name {
        "claude" => Some(agentsflow_events::AgentKind::Claude),
        "codex" => Some(agentsflow_events::AgentKind::Codex),
        "orchestrator" => Some(agentsflow_events::AgentKind::Orchestrator),
        _ => None,
    }
}
fn recipient_enabled(agent: agentsflow_events::AgentKind, claude: bool, codex: bool) -> bool {
    match agent {
        agentsflow_events::AgentKind::Claude => claude,
        agentsflow_events::AgentKind::Codex => codex,
        agentsflow_events::AgentKind::Orchestrator => true,
    }
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
    if matches!(identity, Identity::Agent(agent) if parse_agent(&input.from) != Some(agent)) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error":"sender does not match token"})),
        )
            .into_response();
    }
    let Some(from) = parse_agent(&input.from) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"unknown sender"})),
        )
            .into_response();
    };
    let Some(to) = parse_agent(&input.to) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"unknown recipient"})),
        )
            .into_response();
    };
    if !recipient_enabled(to, state.claude.is_some(), state.codex.is_some()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"recipient agent is disabled"})),
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
    if matches!(identity, Identity::Agent(owner) if parse_agent(&input.agent) != Some(owner)) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error":"inbox does not match token"})),
        )
            .into_response();
    }
    let Some(agent) = parse_agent(&input.agent) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"unknown agent"})),
        )
            .into_response();
    };
    let messages: Vec<_> = state.hub.take_inbox(agent).into_iter().map(|mail| json!({"id":mail.id,"thread":mail.thread,"from":mail.from,"subject":mail.subject,"body":mail.body})).collect();
    Json(json!({"messages":messages})).into_response()
}
fn failure(error: HubError) -> Response {
    match error {
        HubError::Busy => (StatusCode::CONFLICT, Json(json!({"error":"busy"}))).into_response(),
        HubError::NotFound => StatusCode::NOT_FOUND.into_response(),
        HubError::Closed => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}
async fn prompt(State(state): State<AppState>, Json(input): Json<Prompt>) -> Response {
    if input.text.trim().is_empty() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some(actor) = state.actor(&input.agent) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match actor.prompt(input.text).await {
        Ok(()) => (StatusCode::ACCEPTED, Json(json!({"ok":true}))).into_response(),
        Err(error) => failure(error),
    }
}
async fn permission(State(state): State<AppState>, Json(input): Json<Permission>) -> Response {
    let Some(actor) = state.actor(&input.agent) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match actor.respond(input.id, input.allow).await {
        Ok(()) => Json(json!({"ok":true})).into_response(),
        Err(error) => failure(error),
    }
}
async fn get_state(State(state): State<AppState>) -> Json<Value> {
    let info = |actor: &Option<Actor>| actor.as_ref().map(Actor::state).unwrap_or_default();
    Json(
        json!({"cwd":state.cwd, "agents":{"claude":info(&state.claude),"codex":info(&state.codex)}, "workflow":state.manager.state()}),
    )
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
        Err("busy") => (StatusCode::CONFLICT, Json(json!({"error":"busy"}))).into_response(),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
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
        .route("/api/prompt", post(prompt))
        .route("/api/permission", post(permission))
        .route("/api/mail/send", post(send_mail))
        .route("/api/mail/inbox", post(inbox))
        .route("/api/state", get(get_state))
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
    let mail_agent_tokens = MailAgentTokens {
        claude: options.claude.then(|| Uuid::new_v4().simple().to_string()),
        codex: options.codex.then(|| Uuid::new_v4().simple().to_string()),
    };
    let runtime = Runtime::new(RuntimeOptions {
        cwd: options.cwd,
        claude: options.claude,
        codex: options.codex,
        claude_model: options.claude_model,
        codex_model: options.codex_model,
        sandbox: options.sandbox,
        codex_bin: options.codex_bin,
        yes: options.yes,
        log: options.log,
        mail: Some(agentsflow_adapters::MailConfig {
            url: format!("http://127.0.0.1:{port}"),
            token: String::new(),
            exe: std::env::current_exe()?,
            thread_limit: crate::hub::MAX_THREAD_MAILS,
        }),
        mail_agent_tokens: mail_agent_tokens.clone(),
        mail_thread_limit: crate::hub::MAX_THREAD_MAILS,
    })?;
    let manager = Manager::new(
        runtime.hub.clone(),
        runtime.claude.clone(),
        runtime.codex.clone(),
    );
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let state = AppState {
        hub: runtime.hub.clone(),
        token: token.clone(),
        mail_agent_tokens,
        port,
        cwd: runtime.cwd.to_string_lossy().into_owned(),
        claude: runtime.claude.clone(),
        codex: runtime.codex.clone(),
        shutdown: shutdown_rx,
        workflows: specs,
        manager: manager.clone(),
    };
    println!("agentsflow: http://127.0.0.1:{port}/?token={token}");
    let result = axum::serve(listener, app(state))
        .with_graceful_shutdown(async move {
            if let Err(error) = tokio::signal::ctrl_c().await {
                eprintln!("signal: {error}");
            }
            let _ = shutdown_tx.send(true);
        })
        .await;
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
        let tokens = MailAgentTokens {
            claude: Some("claude-token".into()),
            codex: Some("codex-token".into()),
        };
        let claude = authenticate("claude-token", "", "main-token", &tokens).unwrap();
        let codex = authenticate("", "codex-token", "main-token", &tokens).unwrap();
        assert_eq!(claude, Identity::Agent(AgentKind::Claude));
        assert_eq!(codex, Identity::Agent(AgentKind::Codex));
        assert_ne!(claude, codex);
        assert_eq!(authenticate("bad", "", "main-token", &tokens), None);
        assert_eq!(
            authenticate("bad", "main-token", "main-token", &tokens),
            Some(Identity::Main)
        );
        assert_eq!(
            authenticate("claude-token", "main-token", "main-token", &tokens),
            Some(claude)
        );
        for identity in [claude, codex] {
            for path in ["/api/mail/send", "/api/mail/inbox"] {
                assert!(authorize(identity, &axum::http::Method::POST, path));
                assert!(!authorize(identity, &axum::http::Method::GET, path));
            }
            for (method, path) in [
                (axum::http::Method::POST, "/api/prompt"),
                (axum::http::Method::GET, "/api/state"),
                (axum::http::Method::POST, "/api/permission"),
                (axum::http::Method::POST, "/api/workflow/run"),
            ] {
                assert!(!authorize(identity, &method, path));
                assert!(authorize(Identity::Main, &method, path));
            }
        }
        for (method, path) in [
            (axum::http::Method::POST, "/api/prompt"),
            (axum::http::Method::GET, "/api/state"),
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
    async fn personal_mail_identity() {
        LocalSet::new()
            .run_until(async {
                let hub = Hub::new();
                let (claude, claude_task) =
                    crate::hub::spawn(hub.clone(), AgentKind::Claude, false, || async {
                        Ok(NoAgent)
                    });
                let (codex, codex_task) =
                    crate::hub::spawn(hub.clone(), AgentKind::Codex, false, || async {
                        Ok(NoAgent)
                    });
                let manager = Manager::new(hub.clone(), Some(claude.clone()), Some(codex.clone()));
                let (_shutdown, shutdown) = watch::channel(false);
                let state = AppState {
                    hub,
                    token: "main".into(),
                    mail_agent_tokens: MailAgentTokens::default(),
                    port: 7878,
                    cwd: ".".into(),
                    claude: Some(claude.clone()),
                    codex: Some(codex.clone()),
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
                let c = Identity::Agent(AgentKind::Claude);
                let d = Identity::Agent(AgentKind::Codex);
                assert_eq!(
                    send_mail(
                        State(state.clone()),
                        Extension(c),
                        Ok(Json(make("claude", "codex")))
                    )
                    .await
                    .status(),
                    StatusCode::OK
                );
                let denied = send_mail(
                    State(state.clone()),
                    Extension(c),
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
                        Extension(d),
                        Ok(Json(make("claude", "codex")))
                    )
                    .await
                    .status(),
                    StatusCode::FORBIDDEN
                );
                assert_eq!(
                    inbox(
                        State(state.clone()),
                        Extension(c),
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
                        Extension(d),
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
                        Extension(c),
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
                        Extension(d),
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
        assert_eq!(
            parse_agent("claude"),
            Some(agentsflow_events::AgentKind::Claude)
        );
        assert_eq!(
            parse_agent("codex"),
            Some(agentsflow_events::AgentKind::Codex)
        );
        assert_eq!(
            parse_agent("orchestrator"),
            Some(agentsflow_events::AgentKind::Orchestrator)
        );
        assert_eq!(parse_agent("other"), None);
        assert!(!recipient_enabled(
            agentsflow_events::AgentKind::Claude,
            false,
            true
        ));
        assert!(recipient_enabled(
            agentsflow_events::AgentKind::Codex,
            false,
            true
        ));
        assert!(recipient_enabled(
            agentsflow_events::AgentKind::Orchestrator,
            false,
            false
        ));
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
    fn workflow_request_validation() {
        let specs = workflow::builtins().unwrap();
        let summaries = serde_json::json!(specs.iter().map(Spec::summary).collect::<Vec<_>>());
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
