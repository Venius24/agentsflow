use std::{collections::HashMap, path::PathBuf};

use agentsflow_events::{Event, Role};
use anyhow::{Context, Result, bail};
pub use codex_codes::SandboxMode;
use codex_codes::{
    AppServerBuilder, AskForApproval, AsyncClient, CommandExecutionStatus, McpToolCallStatus,
    Notification, PatchApplyStatus, RequestId, ServerMessage, ServerRequest, ThreadItemPayload,
    ThreadStartParams, TurnStartParams, TurnStatus, UserInput,
};
use serde_json::json;

pub fn map_message(message: &ServerMessage) -> Vec<Event> {
    match message {
        ServerMessage::Notification(Notification::ItemStarted(item)) => match &item.item.payload {
            ThreadItemPayload::CommandExecution {
                id, command, cwd, ..
            } => vec![Event::ToolCall {
                id: id.clone(),
                name: "shell".into(),
                input: json!({"command": command, "cwd": cwd}),
            }],
            ThreadItemPayload::FileChange { id, changes, .. } => vec![Event::ToolCall {
                id: id.clone(),
                name: "file_change".into(),
                input: json!(
                    changes
                        .iter()
                        .map(|change| json!({"path": change.path, "kind": change.kind}))
                        .collect::<Vec<_>>()
                ),
            }],
            ThreadItemPayload::McpToolCall {
                id,
                server,
                tool,
                arguments,
                ..
            } => vec![Event::ToolCall {
                id: id.clone(),
                name: format!("{server}/{tool}"),
                input: arguments.clone(),
            }],
            _ => vec![],
        },
        ServerMessage::Notification(Notification::ItemCompleted(item)) => {
            match &item.item.payload {
                ThreadItemPayload::AgentMessage { text, .. } => vec![Event::Message {
                    role: Role::Assistant,
                    text: text.clone(),
                    delta: false,
                }],
                ThreadItemPayload::CommandExecution {
                    id,
                    aggregated_output,
                    status,
                    exit_code,
                    ..
                } => vec![Event::ToolResult {
                    id: id.clone(),
                    output: aggregated_output.clone().unwrap_or_default(),
                    is_error: matches!(
                        status,
                        CommandExecutionStatus::Failed | CommandExecutionStatus::Declined
                    ) || exit_code.is_some_and(|code| code != 0),
                }],
                ThreadItemPayload::FileChange {
                    id,
                    changes,
                    status,
                } => {
                    let mut events = vec![Event::ToolResult {
                        id: id.clone(),
                        output: format!("{status:?}").to_lowercase(),
                        is_error: matches!(
                            status,
                            PatchApplyStatus::Failed | PatchApplyStatus::Declined
                        ),
                    }];
                    events.extend(changes.iter().filter(|change| !change.diff.is_empty()).map(
                        |change| Event::Diff {
                            path: change.path.clone(),
                            patch: change.diff.clone(),
                        },
                    ));
                    events
                }
                ThreadItemPayload::McpToolCall {
                    id,
                    result,
                    error,
                    status,
                    ..
                } => vec![Event::ToolResult {
                    id: id.clone(),
                    output: error
                        .as_ref()
                        .map(|error| error.message.clone())
                        .or_else(|| result.as_ref().map(|result| json!(result).to_string()))
                        .unwrap_or_default(),
                    is_error: error.is_some() || matches!(status, McpToolCallStatus::Failed),
                }],
                _ => vec![],
            }
        }
        ServerMessage::Request {
            id,
            request: ServerRequest::CmdExecApproval(params),
        } => vec![Event::PermissionRequest {
            id: id.to_string(),
            tool: "shell".into(),
            input: json!({"command": params.command, "cwd": params.cwd}),
        }],
        ServerMessage::Request {
            id,
            request: ServerRequest::FileChangeApproval(params),
        } => vec![Event::PermissionRequest {
            id: id.to_string(),
            tool: "file_change".into(),
            input: json!({"reason": params.reason, "grantRoot": params.grant_root}),
        }],
        ServerMessage::Notification(Notification::TurnCompleted(notification)) => {
            let mut events = Vec::new();
            if let Some(error) = &notification.turn.error {
                events.push(Event::Error {
                    message: error.message.clone(),
                });
            }
            let reason = match notification.turn.status {
                TurnStatus::Completed => "completed",
                TurnStatus::Failed => "failed",
                TurnStatus::Interrupted => "interrupted",
                TurnStatus::InProgress => "inprogress",
            };
            events.push(Event::Done {
                reason: reason.into(),
            });
            events
        }
        ServerMessage::Notification(Notification::Error(notification))
            if !notification.will_retry =>
        {
            vec![Event::Error {
                message: notification.error.message.clone(),
            }]
        }
        _ => vec![],
    }
}

#[derive(Debug, Default)]
pub struct CodexOptions {
    pub model: Option<String>,
    pub cwd: Option<PathBuf>,
    pub sandbox: Option<SandboxMode>,
    pub bin: Option<PathBuf>,
    pub mail: Option<crate::MailConfig>,
    pub effort: Option<String>,
}

fn choose_binary(
    explicit: Option<PathBuf>,
    environment: Option<PathBuf>,
    installed: impl FnOnce() -> Option<PathBuf>,
) -> PathBuf {
    explicit
        .or(environment)
        .or_else(installed)
        .unwrap_or_else(|| PathBuf::from("codex"))
}

#[cfg(windows)]
fn installed_binary() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("OpenAI/Codex/bin");
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| {
            let path = entry.ok()?.path().join("codex.exe");
            let meta = path.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            Some((meta.modified().ok()?, path))
        })
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, path)| path)
}

#[cfg(not(windows))]
fn installed_binary() -> Option<PathBuf> {
    None
}

fn find_binary(explicit: Option<PathBuf>) -> PathBuf {
    choose_binary(
        explicit,
        std::env::var_os("CODEX_BIN")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from),
        installed_binary,
    )
}

pub struct CodexSession {
    client: AsyncClient,
    thread_id: String,
    pending: HashMap<String, RequestId>,
}

impl CodexSession {
    pub async fn start(opts: CodexOptions) -> Result<Self> {
        let cwd = match opts.cwd {
            Some(cwd) => std::path::absolute(cwd)?,
            None => std::env::current_dir()?,
        };
        let cwd_string = cwd
            .to_str()
            .context("working directory is not valid Unicode")?
            .to_owned();
        let mut builder = AppServerBuilder::new()
            .command(find_binary(opts.bin))
            .working_directory(&cwd);
        if let Some(effort) = opts.effort {
            builder = builder.config_override("model_reasoning_effort", json!(effort).to_string());
        }
        if let Some(mail) = opts.mail {
            let exe = mail
                .exe
                .to_str()
                .context("mail executable path is not valid Unicode")?;
            for (key, value) in [
                ("mcp_servers.mail.command", json!(exe).to_string()),
                (
                    "mcp_servers.mail.args",
                    json!(["mail-mcp", "--as", mail.id]).to_string(),
                ),
                (
                    "mcp_servers.mail.env.AGENTSFLOW_URL",
                    json!(mail.url).to_string(),
                ),
                (
                    "mcp_servers.mail.env.AGENTSFLOW_TOKEN",
                    json!(mail.token).to_string(),
                ),
                (
                    "mcp_servers.mail.env.AGENTSFLOW_PEERS",
                    json!(mail.peers.join(",")).to_string(),
                ),
                (
                    "mcp_servers.mail.default_tools_approval_mode",
                    json!("approve").to_string(),
                ),
            ] {
                builder = builder.config_override(key, value);
            }
        }
        let mut client = AsyncClient::start_with(builder).await?;
        let thread = client
            .thread_start(&ThreadStartParams {
                cwd: Some(cwd_string),
                model: opts.model,
                approval_policy: Some(AskForApproval::OnRequest),
                sandbox: Some(opts.sandbox.unwrap_or(SandboxMode::Read_only)),
                ..Default::default()
            })
            .await;
        let thread = match thread {
            Ok(thread) => thread,
            Err(error) => {
                let _ = client.shutdown().await;
                return Err(error.into());
            }
        };
        Ok(Self {
            client,
            thread_id: thread.thread.id,
            pending: HashMap::new(),
        })
    }

    pub async fn send(&mut self, prompt: &str) -> Result<()> {
        self.client
            .turn_start(&TurnStartParams {
                thread_id: self.thread_id.clone(),
                input: vec![UserInput::Text {
                    text: prompt.into(),
                    text_elements: None,
                }],
                ..Default::default()
            })
            .await?;
        Ok(())
    }

    pub async fn next_events(&mut self) -> Result<Vec<Event>> {
        let message = self
            .client
            .next_message()
            .await?
            .context("codex app-server closed")?;
        if let ServerMessage::Request { id, request } = &message {
            if !matches!(
                request,
                ServerRequest::CmdExecApproval(_) | ServerRequest::FileChangeApproval(_)
            ) {
                self.client
                    .respond_error(id.clone(), -32601, "unsupported by agentsflow")
                    .await?;
                return Ok(vec![]);
            }
            let key = id.to_string();
            if self.pending.contains_key(&key) {
                bail!("duplicate permission request ID: {key}");
            }
            self.pending.insert(key, id.clone());
        }
        Ok(map_message(&message))
    }

    pub async fn respond(&mut self, request_id: &str, allow: bool) -> Result<()> {
        let id = self
            .pending
            .get(request_id)
            .context("unknown permission request")?
            .clone();
        let decision = if allow { "accept" } else { "decline" };
        self.client
            .respond(id, &json!({"decision": decision}))
            .await?;
        self.pending.remove(request_id);
        Ok(())
    }

    pub async fn shutdown(self) -> Result<()> {
        self.client.shutdown().await?;
        Ok(())
    }
}

impl crate::Agent for CodexSession {
    async fn send(&mut self, prompt: &str) -> Result<()> {
        Self::send(self, prompt).await
    }
    async fn next_events(&mut self) -> Result<Vec<Event>> {
        Self::next_events(self).await
    }
    async fn respond(&mut self, request_id: &str, allow: bool) -> Result<()> {
        Self::respond(self, request_id, allow).await
    }
    async fn shutdown(self) -> Result<()> {
        Self::shutdown(self).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn map(frame: Value) -> Vec<Event> {
        map_message(&ServerMessage::from_value(frame).unwrap())
    }
    fn item(method: &str, item: Value) -> Vec<Event> {
        map(json!({"method":method, "params":{"threadId":"t", "turnId":"v", "item":item}}))
    }
    fn command(status: &str, exit: Option<i32>) -> Value {
        json!({"type":"commandExecution", "id":"i", "command":"echo hi", "commandActions":[], "cwd":"/tmp", "status":status, "exitCode":exit, "aggregatedOutput":"hi"})
    }

    #[test]
    fn agent_message_and_deltas() {
        assert_eq!(
            item(
                "item/completed",
                json!({"type":"agentMessage", "id":"i", "text":"hello"})
            ),
            vec![Event::Message {
                role: Role::Assistant,
                text: "hello".into(),
                delta: false
            }]
        );
        assert!(map(json!({"method":"item/agentMessage/delta", "params":{"threadId":"t", "turnId":"v", "itemId":"i", "delta":"x"}})).is_empty());
    }
    #[test]
    fn command_start_and_results() {
        assert_eq!(
            item("item/started", command("inProgress", None)),
            vec![Event::ToolCall {
                id: "i".into(),
                name: "shell".into(),
                input: json!({"command":"echo hi", "cwd":"/tmp"})
            }]
        );
        for (status, code, error) in [
            ("completed", Some(0), false),
            ("failed", Some(1), true),
            ("declined", None, true),
            ("completed", Some(1), true),
        ] {
            assert_eq!(
                item("item/completed", command(status, code)),
                vec![Event::ToolResult {
                    id: "i".into(),
                    output: "hi".into(),
                    is_error: error
                }]
            );
        }
    }
    #[test]
    fn approvals() {
        assert_eq!(
            map(
                json!({"id":7,"method":"item/commandExecution/requestApproval", "params":{"command":"pwd","cwd":"/tmp"}})
            ),
            vec![Event::PermissionRequest {
                id: "7".into(),
                tool: "shell".into(),
                input: json!({"command":"pwd","cwd":"/tmp"})
            }]
        );
        assert_eq!(
            map(
                json!({"id":"f","method":"item/fileChange/requestApproval", "params":{"reason":"edit","grantRoot":"/tmp"}})
            ),
            vec![Event::PermissionRequest {
                id: "f".into(),
                tool: "file_change".into(),
                input: json!({"reason":"edit","grantRoot":"/tmp"})
            }]
        );
    }
    #[test]
    fn file_change_and_mcp() {
        let file = |status| json!({"type":"fileChange","id":"f","status":status,"changes":[{"path":"a","kind":{"type":"add"},"diff":"+hello"}]});
        assert_eq!(
            item("item/started", file("inProgress")),
            vec![Event::ToolCall {
                id: "f".into(),
                name: "file_change".into(),
                input: json!([{"path":"a","kind":{"type":"add"}}])
            }]
        );
        assert_eq!(
            item("item/completed", file("completed")),
            vec![
                Event::ToolResult {
                    id: "f".into(),
                    output: "completed".into(),
                    is_error: false
                },
                Event::Diff {
                    path: "a".into(),
                    patch: "+hello".into()
                }
            ]
        );
        let mcp = json!({"type":"mcpToolCall","id":"m","server":"s","tool":"t","arguments":{"x":1},"status":"inProgress"});
        assert_eq!(
            item("item/started", mcp),
            vec![Event::ToolCall {
                id: "m".into(),
                name: "s/t".into(),
                input: json!({"x":1})
            }]
        );
    }
    #[test]
    fn turn_results_and_noise() {
        assert_eq!(
            map(
                json!({"method":"turn/completed", "params":{"turn":{"id":"v","status":"completed","items":[]}}})
            ),
            vec![Event::Done {
                reason: "completed".into()
            }]
        );
        assert_eq!(
            map(
                json!({"method":"turn/completed", "params":{"turn":{"id":"v","status":"failed","items":[],"error":{"message":"bad"}}}})
            ),
            vec![
                Event::Error {
                    message: "bad".into()
                },
                Event::Done {
                    reason: "failed".into()
                }
            ]
        );
        assert!(
            map(json!({"method":"error","params":{"error":{"message":"retry"},"willRetry":true}}))
                .is_empty()
        );
        assert_eq!(
            map(json!({"method":"error","params":{"error":{"message":"bad"},"willRetry":false}})),
            vec![Event::Error {
                message: "bad".into()
            }]
        );
        assert!(map(json!({"method":"future/noise", "params":{}})).is_empty());
    }
    #[test]
    fn binary_priority() {
        let path = |s: &str| Some(PathBuf::from(s));
        assert_eq!(
            choose_binary(path("explicit"), path("env"), || panic!(
                "must not inspect installation"
            )),
            PathBuf::from("explicit")
        );
        assert_eq!(
            choose_binary(None, path("env"), || panic!(
                "must not inspect installation"
            )),
            PathBuf::from("env")
        );
        assert_eq!(
            choose_binary(None, None, || path("installed")),
            PathBuf::from("installed")
        );
        assert_eq!(choose_binary(None, None, || None), PathBuf::from("codex"));
    }
    #[tokio::test]
    #[ignore = "requires a configured Codex CLI and network access"]
    async fn real_codex_session() -> Result<()> {
        let mut session = CodexSession::start(CodexOptions::default()).await?;
        session
            .send("Reply with a short greeting without using tools.")
            .await?;
        let mut saw_message = false;
        loop {
            let events = session.next_events().await?;
            for event in &events {
                match event {
                    Event::Message { .. } => saw_message = true,
                    Event::PermissionRequest { id, .. } => session.respond(id, false).await?,
                    Event::Error { message } => anyhow::bail!("Codex error: {message}"),
                    _ => {}
                }
            }
            if events
                .iter()
                .any(|event| matches!(event, Event::Done { .. }))
            {
                break;
            }
        }
        session.shutdown().await?;
        assert!(saw_message);
        Ok(())
    }
}
