use std::{
    collections::HashMap,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use agentsflow_events::{Event, Role};
use anyhow::{Context, Result, bail};
use claude_codes::{
    ClaudeCliBuilder, ClaudeInput, ClaudeOutput, ContentBlock, ControlRequestMessage,
    ControlRequestPayload, ControlResponse, ControlResponseMessage, ControlResponsePayload,
    PermissionResult, Protocol, ToolResultContent,
};
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Child,
    sync::mpsc,
};

pub fn map_output(out: &ClaudeOutput) -> Vec<Event> {
    match out {
        ClaudeOutput::Assistant(message) => message
            .message
            .content
            .iter()
            .flat_map(|block| match block {
                ContentBlock::Text(text) => vec![Event::Message {
                    role: Role::Assistant,
                    text: text.text.clone(),
                    delta: false,
                }],
                ContentBlock::ToolUse(tool) => {
                    let mut events = vec![Event::ToolCall {
                        id: tool.id.clone(),
                        name: tool.name.clone(),
                        input: tool.input.clone(),
                    }];
                    if let Some(diff) = tool_diff(&tool.name, &tool.input) {
                        events.push(diff);
                    }
                    events
                }
                _ => vec![],
            })
            .collect(),
        ClaudeOutput::User(message) => message
            .message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolResult(result) => Some(Event::ToolResult {
                    id: result.tool_use_id.clone(),
                    output: match &result.content {
                        Some(ToolResultContent::Text(text)) => text.clone(),
                        Some(ToolResultContent::Structured(value)) => {
                            Value::Array(value.clone()).to_string()
                        }
                        None => String::new(),
                    },
                    is_error: result.is_error.unwrap_or(false),
                }),
                _ => None,
            })
            .collect(),
        ClaudeOutput::ControlRequest(request) => match &request.request {
            ControlRequestPayload::CanUseTool(tool) => vec![Event::PermissionRequest {
                id: request.request_id.clone(),
                tool: tool.tool_name.clone(),
                input: tool.input.clone(),
            }],
            _ => vec![],
        },
        ClaudeOutput::Result(result) => {
            let reason = result.subtype.to_string();
            let mut events = Vec::new();
            if result.is_error {
                let message = result
                    .result
                    .clone()
                    .filter(|s| !s.is_empty())
                    .or_else(|| (!result.errors.is_empty()).then(|| result.errors.join("; ")))
                    .unwrap_or_else(|| reason.clone());
                events.push(Event::Error { message });
            }
            events.push(Event::Done { reason });
            events
        }
        ClaudeOutput::Error(error) => vec![Event::Error {
            message: error.error.message.clone(),
        }],
        _ => vec![],
    }
}

fn tool_diff(name: &str, input: &Value) -> Option<Event> {
    let path = input.get("file_path")?.as_str()?;
    let lines = |text: &str, prefix: char| {
        text.lines()
            .map(|line| format!("{prefix}{line}\n"))
            .collect::<String>()
    };
    let patch = match name {
        "Write" => format!(
            "--- /dev/null\n+++ {path}\n@@ new file @@\n{}",
            lines(input.get("content")?.as_str()?, '+')
        ),
        "Edit" => format!(
            "--- {path}\n+++ {path}\n@@ edit @@\n{}{}",
            lines(input.get("old_string")?.as_str()?, '-'),
            lines(input.get("new_string")?.as_str()?, '+')
        ),
        "MultiEdit" => {
            let mut patch = format!("--- {path}\n+++ {path}\n");
            for (i, edit) in input.get("edits")?.as_array()?.iter().enumerate() {
                let old = edit.get("old_string")?.as_str()?;
                let new = edit.get("new_string")?.as_str()?;
                patch.push_str(&format!(
                    "@@ edit {} @@\n{}{}",
                    i + 1,
                    lines(old, '-'),
                    lines(new, '+')
                ));
            }
            patch
        }
        _ => return None,
    };
    Some(Event::Diff {
        path: path.into(),
        patch,
    })
}

#[derive(Debug, Default)]
pub struct ClaudeOptions {
    pub model: Option<String>,
    pub cwd: Option<PathBuf>,
    pub mail: Option<crate::MailConfig>,
    pub effort: Option<String>,
}

const STDOUT_BUFFER_SIZE: usize = 10 * 1024 * 1024;
static INIT_ID: AtomicU64 = AtomicU64::new(1);
static INTERRUPT_ID: AtomicU64 = AtomicU64::new(1);

type Lines = mpsc::UnboundedReceiver<std::io::Result<String>>;

/// The CLI is driven through two tasks: stdout lines arrive over a channel
/// (receiving from it is cancel-safe, a half-read line is never lost) and
/// every stdin write is queued, so an interrupt can be sent while
/// `next_events` is still waiting.
pub struct ClaudeSession {
    child: Child,
    stdin: mpsc::UnboundedSender<String>,
    lines: Lines,
    pending: HashMap<String, Value>,
}

fn enqueue(stdin: &mpsc::UnboundedSender<String>, line: String) -> Result<()> {
    stdin
        .send(line)
        .map_err(|_| anyhow::anyhow!("claude stdin closed"))
}

fn interrupt_line(request_id: &str) -> Result<String> {
    Ok(Protocol::serialize(&ClaudeInput::interrupt(request_id))?)
}

fn parse_line(line: &str) -> Result<ClaudeOutput> {
    ClaudeOutput::parse_json_tolerant(line)
        .map_err(claude_codes::Error::from)
        .map_err(Into::into)
}

async fn next_line(lines: &mut Lines) -> Result<String> {
    Ok(lines.recv().await.context("claude closed")??)
}

impl ClaudeSession {
    pub async fn start(opts: ClaudeOptions) -> Result<Self> {
        let mut builder = ClaudeCliBuilder::new().permission_prompt_tool("stdio");
        if let Some(mail) = opts.mail {
            let config = serde_json::json!({"mcpServers":{"mail":{"command":mail.exe,"args":["mail-mcp","--as",mail.id],"env":{"AGENTSFLOW_URL":mail.url,"AGENTSFLOW_TOKEN":mail.token,"AGENTSFLOW_PEERS":mail.peers.join(",")}}}});
            builder = builder
                .mcp_config([config.to_string()])
                .allowed_tools(["mcp__mail__send_mail", "mcp__mail__check_inbox"]);
        }
        if let Some(model) = opts.model {
            builder = builder.model(model);
        }
        if let Some(cwd) = opts.cwd {
            builder = builder.working_directory(cwd);
        }
        let mut command = builder.build_command()?;
        if let Some(effort) = opts.effort {
            command.arg("--effort").arg(effort);
        }
        let mut child = command.spawn()?;
        let (Some(mut input), Some(output), Some(mut errors)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            let _ = child.kill().await;
            anyhow::bail!("claude stdio is not piped");
        };
        tokio::spawn(async move {
            let _ = tokio::io::copy(&mut errors, &mut tokio::io::sink()).await;
        });
        let (stdin, mut queue) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            while let Some(line) = queue.recv().await {
                if input.write_all(line.as_bytes()).await.is_err() || input.flush().await.is_err() {
                    break;
                }
            }
        });
        let (sender, lines) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut output = BufReader::with_capacity(STDOUT_BUFFER_SIZE, output);
            loop {
                let mut line = String::new();
                match output.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) if line.trim().is_empty() => {}
                    Ok(_) => {
                        if sender.send(Ok(line.trim().to_owned())).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(Err(error));
                        break;
                    }
                }
            }
        });
        let mut session = Self {
            child,
            stdin,
            lines,
            pending: HashMap::new(),
        };
        if let Err(error) = session.initialize().await {
            let _ = session.shutdown().await;
            return Err(error);
        }
        Ok(session)
    }

    /// Tool approval handshake: the CLI asks for permissions only after it.
    async fn initialize(&mut self) -> Result<()> {
        let request_id = format!("init-{}", INIT_ID.fetch_add(1, Ordering::Relaxed));
        enqueue(
            &self.stdin,
            Protocol::serialize(&ControlRequestMessage::initialize(&request_id))?,
        )?;
        loop {
            let line = next_line(&mut self.lines).await?;
            if let ClaudeOutput::ControlResponse(response) = parse_line(&line)? {
                match response.response {
                    ControlResponsePayload::Success { request_id: id, .. } if id == request_id => {
                        return Ok(());
                    }
                    ControlResponsePayload::Error { error, .. } => {
                        bail!("tool approval initialization failed: {error}");
                    }
                    _ => {}
                }
            }
        }
    }

    pub async fn send(&mut self, prompt: &str) -> Result<()> {
        enqueue(
            &self.stdin,
            Protocol::serialize(&ClaudeInput::user_message_without_session(prompt))?,
        )
    }

    pub async fn next_events(&mut self) -> Result<Vec<Event>> {
        let output = parse_line(&next_line(&mut self.lines).await?)?;
        if let ClaudeOutput::ControlRequest(request) = &output {
            if let ControlRequestPayload::CanUseTool(tool) = &request.request {
                self.pending
                    .insert(request.request_id.clone(), tool.input.clone());
            }
        }
        if let ClaudeOutput::ControlCancelRequest(request) = &output {
            self.pending.remove(&request.request_id);
        }
        Ok(map_output(&output))
    }

    pub async fn respond(&mut self, request_id: &str, allow: bool) -> Result<()> {
        let input = self
            .pending
            .get(request_id)
            .context("unknown permission request")?;
        let result = if allow {
            PermissionResult::allow(input.clone())
        } else {
            PermissionResult::deny("denied by user")
        };
        enqueue(
            &self.stdin,
            Protocol::serialize(&ControlResponseMessage::from(ControlResponse::from_result(
                request_id, result,
            )))?,
        )?;
        self.pending.remove(request_id);
        Ok(())
    }

    /// Asks the CLI to end the current turn; the session stays usable and the
    /// turn finishes with a `result` frame.
    pub fn interrupter(&self) -> crate::Interrupter {
        let stdin = self.stdin.clone();
        crate::Interrupter::new(move || {
            let id = format!("interrupt-{}", INTERRUPT_ID.fetch_add(1, Ordering::Relaxed));
            enqueue(&stdin, interrupt_line(&id)?)
        })
    }

    pub async fn shutdown(mut self) -> Result<()> {
        self.child.kill().await?;
        Ok(())
    }
}

impl crate::Agent for ClaudeSession {
    async fn send(&mut self, prompt: &str) -> Result<()> {
        Self::send(self, prompt).await
    }
    async fn next_events(&mut self) -> Result<Vec<Event>> {
        Self::next_events(self).await
    }
    async fn respond(&mut self, request_id: &str, allow: bool) -> Result<()> {
        Self::respond(self, request_id, allow).await
    }
    fn interrupter(&self) -> crate::Interrupter {
        Self::interrupter(self)
    }
    async fn shutdown(self) -> Result<()> {
        Self::shutdown(self).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map(frame: Value) -> Vec<Event> {
        map_output(&ClaudeOutput::parse_json_tolerant(&frame.to_string()).unwrap())
    }

    fn assistant(content: Value) -> Value {
        json!({"type":"assistant", "session_id":"s1", "message":{"id":"m1", "role":"assistant", "model":"haiku", "content":content}})
    }

    #[test]
    fn assistant_text_and_tool_use() {
        assert_eq!(
            map(assistant(json!([
                {"type":"text","text":"hello"},
                {"type":"tool_use","id":"t1","name":"Write","input":{"path":"a"}}
            ]))),
            vec![
                Event::Message {
                    role: Role::Assistant,
                    text: "hello".into(),
                    delta: false
                },
                Event::ToolCall {
                    id: "t1".into(),
                    name: "Write".into(),
                    input: json!({"path":"a"})
                },
            ]
        );
    }

    #[test]
    fn tool_diffs_and_invalid_inputs() {
        let cases = [
            (
                "Write",
                json!({"file_path":"a", "content":"one\ntwo\n"}),
                "--- /dev/null\n+++ a\n@@ new file @@\n+one\n+two\n",
            ),
            (
                "Edit",
                json!({"file_path":"a", "old_string":"old", "new_string":"new"}),
                "--- a\n+++ a\n@@ edit @@\n-old\n+new\n",
            ),
            (
                "MultiEdit",
                json!({"file_path":"a", "edits":[{"old_string":"a","new_string":"b"},{"old_string":"c","new_string":"d"}]}),
                "--- a\n+++ a\n@@ edit 1 @@\n-a\n+b\n@@ edit 2 @@\n-c\n+d\n",
            ),
        ];
        for (name, input, patch) in cases {
            let events = map(assistant(
                json!([{"type":"tool_use", "id":"i", "name":name, "input":input}]),
            ));
            assert!(matches!(&events[0], Event::ToolCall { .. }));
            assert_eq!(
                events[1],
                Event::Diff {
                    path: "a".into(),
                    patch: patch.into()
                }
            );
        }
        assert!(tool_diff("Write", &json!({"file_path":"a"})).is_none());
        assert!(
            tool_diff(
                "MultiEdit",
                &json!({"file_path":"a","edits":[{"old_string":"x"}]})
            )
            .is_none()
        );
    }

    #[test]
    fn user_tool_results() {
        assert_eq!(
            map(
                json!({"type":"user", "session_id":"7fbc568e-2bd6-45aa-b217-a1cf80004ba1", "message":{"role":"user", "content":[
                    {"type":"tool_result", "tool_use_id":"t1", "content":"ok"},
                    {"type":"tool_result", "tool_use_id":"t2", "content":[{"type":"text","text":"bad"}], "is_error":true},
                    {"type":"tool_result", "tool_use_id":"t3"}
                ]}})
            ),
            vec![
                Event::ToolResult {
                    id: "t1".into(),
                    output: "ok".into(),
                    is_error: false
                },
                Event::ToolResult {
                    id: "t2".into(),
                    output: r#"[{"text":"bad","type":"text"}]"#.into(),
                    is_error: true
                },
                Event::ToolResult {
                    id: "t3".into(),
                    output: String::new(),
                    is_error: false
                },
            ]
        );
    }

    #[test]
    fn permission_request() {
        assert_eq!(
            map(
                json!({"type":"control_request", "request_id":"r1", "request":{"subtype":"can_use_tool", "tool_name":"Write", "input":{"path":"a"}}})
            ),
            vec![Event::PermissionRequest {
                id: "r1".into(),
                tool: "Write".into(),
                input: json!({"path":"a"})
            }]
        );
    }

    fn result(subtype: &str, error: bool) -> Value {
        json!({"type":"result", "subtype":subtype, "is_error":error, "duration_ms":1, "duration_api_ms":1, "num_turns":1, "result":"details", "session_id":"s1", "total_cost_usd":0.0, "usage":{"input_tokens":1,"output_tokens":1}, "permission_denials":[], "uuid":"u1"})
    }

    #[test]
    fn result_success_and_error() {
        assert_eq!(
            map(result("success", false)),
            vec![Event::Done {
                reason: "success".into()
            }]
        );
        assert_eq!(
            map(result("error_during_execution", true)),
            vec![
                Event::Error {
                    message: "details".into()
                },
                Event::Done {
                    reason: "error_during_execution".into()
                }
            ]
        );
    }

    #[test]
    fn api_error_and_unhandled_event() {
        assert_eq!(
            map(json!({"type":"error", "error":{"type":"api_error", "message":"failed"}})),
            vec![Event::Error {
                message: "failed".into()
            }]
        );
        assert!(map(json!({"type":"stream_event", "event":{"type":"future_event"}, "uuid":"u", "session_id":"s"})).is_empty());
        assert!(map(assistant(json!([{ "type":"future_block", "value":1 }]))).is_empty());
    }

    #[test]
    fn interrupt_is_a_control_request() {
        let line = interrupt_line("interrupt-1").unwrap();
        assert!(line.ends_with('\n'));
        let frame: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(frame["type"], "control_request");
        assert_eq!(frame["request_id"], "interrupt-1");
        assert_eq!(frame["request"]["subtype"], "interrupt");
    }

    #[tokio::test]
    #[ignore = "requires a configured Claude CLI and network access"]
    async fn real_claude_session() -> Result<()> {
        let mut session = ClaudeSession::start(ClaudeOptions {
            model: Some("haiku".into()),
            cwd: None,
            mail: None,
            effort: None,
        })
        .await?;
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
                    Event::Error { message } => anyhow::bail!("Claude error: {message}"),
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
