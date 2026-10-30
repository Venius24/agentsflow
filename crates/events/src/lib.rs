use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub session: String,
    pub agent: AgentKind,
    pub seq: u64,
    pub ts_ms: u64,
    pub event: Event,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    Claude,
    Codex,
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
pub enum Role {
    User,
    Assistant,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
}
