use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentId(pub String);

impl AgentId {
    pub fn orchestrator() -> Self {
        Self("orchestrator".into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl From<&str> for AgentId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}
impl From<String> for AgentId {
    fn from(value: String) -> Self {
        Self(value)
    }
}
impl AsRef<str> for AgentId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}
impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub session: String,
    pub agent: AgentId,
    pub seq: u64,
    pub ts_ms: u64,
    pub event: Event,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    Claude,
    Codex,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuardNote {
    pub rule: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Message {
        role: Role,
        text: String,
        delta: bool,
    },
    ToolCall {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        id: String,
        output: String,
        is_error: bool,
    },
    Diff {
        path: String,
        patch: String,
    },
    PermissionRequest {
        id: String,
        tool: String,
        input: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        guard: Option<GuardNote>,
    },
    PermissionResolved {
        id: String,
        allow: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        blocked_by_guard: bool,
    },
    Mail {
        id: String,
        thread: String,
        from: AgentId,
        to: AgentId,
        subject: String,
        body: String,
    },
    MailRead {
        agent: AgentId,
        ids: Vec<String>,
    },
    Handoff {
        from: AgentId,
        to: AgentId,
        step: String,
        text: String,
    },
    Workflow {
        name: String,
        step: Option<String>,
        status: WorkflowStatus,
        detail: String,
    },
    Done {
        reason: String,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStatus {
    Started,
    StepStarted,
    StepDone,
    Looped,
    Finished,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn permission_without_guard_stays_compatible() {
        let value =
            json!({"type":"permission_request","id":"1","tool":"Bash","input":{"command":"pwd"}});
        let event: Event = serde_json::from_value(value.clone()).unwrap();
        assert!(matches!(
            event,
            Event::PermissionRequest { guard: None, .. }
        ));
        assert_eq!(serde_json::to_value(event).unwrap(), value);
    }

    #[test]
    fn events_roundtrip_json() {
        let events = [
            Event::Message {
                role: Role::Assistant,
                text: "hello".into(),
                delta: true,
            },
            Event::ToolCall {
                id: "1".into(),
                name: "read".into(),
                input: json!({"path": "a"}),
            },
            Event::ToolResult {
                id: "1".into(),
                output: "ok".into(),
                is_error: false,
            },
            Event::Diff {
                path: "a".into(),
                patch: "+line".into(),
            },
            Event::PermissionRequest {
                id: "2".into(),
                tool: "write".into(),
                input: json!({"path": "a"}),
                guard: Some(GuardNote {
                    rule: "example".into(),
                    reason: "reason".into(),
                }),
            },
            Event::PermissionResolved {
                id: "2".into(),
                allow: true,
                blocked_by_guard: false,
            },
            Event::Mail {
                id: "1".into(),
                thread: "t".into(),
                from: AgentId::from("claude"),
                to: AgentId::from("codex"),
                subject: "s".into(),
                body: "b".into(),
            },
            Event::MailRead {
                agent: AgentId::from("codex"),
                ids: vec!["1".into()],
            },
            Event::Handoff {
                from: AgentId::from("claude"),
                to: AgentId::from("codex"),
                step: "fix".into(),
                text: "hello".into(),
            },
            Event::Workflow {
                name: "flow".into(),
                step: Some("fix".into()),
                status: WorkflowStatus::StepStarted,
                detail: String::new(),
            },
            Event::Done {
                reason: "completed".into(),
            },
            Event::Error {
                message: "failed".into(),
            },
        ];
        for event in events {
            let json = serde_json::to_string(&event).unwrap();
            assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), event);
        }
    }

    #[test]
    fn message_json_shape() {
        let event = Event::Message {
            role: Role::User,
            text: "hello".into(),
            delta: false,
        };
        assert_eq!(
            serde_json::to_value(event).unwrap(),
            json!({
                "type": "message", "role": "user", "text": "hello", "delta": false
            })
        );
    }

    #[test]
    fn agent_id_json_roundtrip() {
        let id = AgentId::from("junior");
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"junior\"");
        assert_eq!(serde_json::from_str::<AgentId>("\"junior\"").unwrap(), id);
        assert_eq!(AgentId::orchestrator().as_str(), "orchestrator");
    }

    #[test]
    fn legacy_pair_envelope_json() {
        let json = r#"{"session":"s","agent":"claude","seq":1,"ts_ms":2,"event":{"type":"mail","id":"i","thread":"t","from":"claude","to":"orchestrator","subject":"s","body":"b"}}"#;
        let envelope: Envelope = serde_json::from_str(json).unwrap();
        assert_eq!(envelope.agent, AgentId::from("claude"));
        assert_eq!(
            serde_json::to_value(envelope).unwrap(),
            serde_json::from_str::<serde_json::Value>(json).unwrap()
        );
    }

    #[test]
    fn workflow_status_roundtrip() {
        for status in [
            WorkflowStatus::Started,
            WorkflowStatus::StepStarted,
            WorkflowStatus::StepDone,
            WorkflowStatus::Looped,
            WorkflowStatus::Finished,
            WorkflowStatus::Failed,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(
                serde_json::from_str::<WorkflowStatus>(&json).unwrap(),
                status
            );
        }
        assert_eq!(
            serde_json::to_string(&AgentId::orchestrator()).unwrap(),
            "\"orchestrator\""
        );
    }
}
