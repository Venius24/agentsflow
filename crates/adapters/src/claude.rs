use std::{collections::HashMap, path::PathBuf};

use agentsflow_events::{Event, Role};
use anyhow::{Context, Result};
use claude_codes::{
    AsyncClient, ClaudeCliBuilder, ClaudeInput, ClaudeOutput, ContentBlock, ControlRequestPayload,
    ControlResponse, PermissionResult, ToolResultContent,
};
use serde_json::Value;

pub fn map_output(out: &ClaudeOutput) -> Vec<Event> {
    match out {
        ClaudeOutput::Assistant(message) => message
            .message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(Event::Message {
                    role: Role::Assistant,
                    text: text.text.clone(),
                    delta: false,
                }),
                ContentBlock::ToolUse(tool) => Some(Event::ToolCall {
                    id: tool.id.clone(),
                    name: tool.name.clone(),
                    input: tool.input.clone(),
                }),
                _ => None,
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

#[derive(Debug, Default)]
pub struct ClaudeOptions {
    pub model: Option<String>,
    pub cwd: Option<PathBuf>,
}

pub struct ClaudeSession {
    client: AsyncClient,
    pending: HashMap<String, Value>,
}

impl ClaudeSession {
    pub async fn start(opts: ClaudeOptions) -> Result<Self> {
        let mut builder = ClaudeCliBuilder::new().permission_prompt_tool("stdio");
        if let Some(model) = opts.model {
            builder = builder.model(model);
        }
        if let Some(cwd) = opts.cwd {
            builder = builder.working_directory(cwd);
        }
        let mut client = AsyncClient::new(builder.spawn().await?)?;
        if let Err(error) = client.enable_tool_approval().await {
            let _ = client.shutdown().await;
            return Err(error.into());
        }
        Ok(Self {
            client,
            pending: HashMap::new(),
        })
    }

    pub async fn send(&mut self, prompt: &str) -> Result<()> {
        self.client
            .send(&ClaudeInput::user_message_without_session(prompt))
            .await?;
        Ok(())
    }

    pub async fn next_events(&mut self) -> Result<Vec<Event>> {
        let output = self.client.receive().await?;
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
        self.client
            .send_control_response(ControlResponse::from_result(request_id, result))
            .await?;
        self.pending.remove(request_id);
        Ok(())
    }

    pub async fn shutdown(self) -> Result<()> {
        self.client.shutdown().await?;
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

    #[tokio::test]
    #[ignore = "requires a configured Claude CLI and network access"]
    async fn real_claude_session() -> Result<()> {
        let mut session = ClaudeSession::start(ClaudeOptions {
            model: Some("haiku".into()),
            cwd: None,
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
