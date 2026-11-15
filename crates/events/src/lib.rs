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
    Acp,
    Gemini,
    Opencode,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuardNote {
    pub rule: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Backlog,
    Todo,
    InProgress,
    Review,
    Done,
    Blocked,
    Archived,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskSnapshot {
    pub id: String,
    pub title: String,
    pub description: String,
    pub status: TaskStatus,
    pub assignee: Option<AgentId>,
    pub progress: u8,
    pub labels: Vec<String>,
    pub note: String,
    pub thread: Option<String>,
    pub created_ms: u64,
    pub updated_ms: u64,
    /// Present on department tasks (`serve --home`); old cards have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work: Option<WorkInfo>,
}

/// Where a department task is in its life. The board status is derived from
/// it (`WorkState::status`), so the two never disagree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkState {
    #[default]
    Queued,
    /// Waits for the owner: answers to questions or a decision.
    Waiting,
    Running,
    /// Stopped by loops, time, budget, a failure or the owner; see `stuck_reason`.
    Stuck,
    /// Finished by the department, waits for acceptance.
    Review,
    Done,
    Cancelled,
}

impl WorkState {
    pub fn status(self) -> TaskStatus {
        match self {
            Self::Queued => TaskStatus::Todo,
            Self::Waiting | Self::Stuck => TaskStatus::Blocked,
            Self::Running => TaskStatus::InProgress,
            Self::Review => TaskStatus::Review,
            Self::Done => TaskStatus::Done,
            Self::Cancelled => TaskStatus::Archived,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Internal,
    /// Leaves the machine (post, letter, change in someone else's system):
    /// only the owner accepts it.
    #[default]
    External,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRef {
    pub id: String,
    pub started_ms: u64,
    #[serde(default)]
    pub finished_ms: Option<u64>,
    /// `finished`, `stuck`, `interrupted`; empty while running.
    #[serde(default)]
    pub outcome: String,
    #[serde(default)]
    pub cost_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub text: String,
    #[serde(default)]
    pub answer: Option<String>,
    /// Who asked: `orchestrator` or a department agent.
    #[serde(default)]
    pub from: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WorkInfo {
    pub department: String,
    #[serde(default)]
    pub state: WorkState,
    #[serde(default)]
    pub risk: Risk,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub runs: Vec<RunRef>,
    #[serde(default)]
    pub questions: Vec<Question>,
    #[serde(default)]
    pub cost_usd: f64,
    /// Tokens of all turns, Codex without a price included.
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub budget_usd: Option<f64>,
    #[serde(default)]
    pub result_summary: String,
    #[serde(default)]
    pub result_path: Option<String>,
    #[serde(default)]
    pub stuck_reason: Option<String>,
    /// How many times the result was sent back to the department.
    #[serde(default)]
    pub returns: u8,
    /// `owner`, `orchestrator`, `schedule` or the requesting department.
    #[serde(default)]
    pub source: String,
    /// Remarks of the acceptance that sent the result back.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub review_notes: Vec<String>,
    /// The result waits for the owner's acceptance (external, or the
    /// orchestrator handed it over).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub owner_review: bool,
    /// A new department the orchestrator proposes for this task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal: Option<DepartmentProposal>,
    /// The orchestrator judged it small: one agent of role `small`, no review.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub small: bool,
    /// The doer hit its usage limit: the agent of role `fallback` takes over.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fallback: bool,
    /// Files the owner sent for this task, relative to the home folder.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<String>,
    /// The repository of the department this task works in, chosen by the
    /// orchestrator; none means the department's first one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Since when the running task is silent: no event from the agent of the
    /// current step. Cleared by the next event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub silent_since_ms: Option<u64>,
    /// The external event (CI failure, issue, comment) that created the task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<EventRef>,
    /// The draft pull request the department opened for this task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    /// The closing comment waits for the owner's "publish".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment_draft: Option<String>,
    /// The closing comment was written, or the owner said it is not needed:
    /// a task sent back for another round does not write it again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub commented: bool,
}

/// Where a task came from outside: a repository event.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EventRef {
    /// `ci`, `issue` or `comment`.
    pub kind: String,
    /// `owner/name`.
    pub repo: String,
    /// Deduplication key, unique per event.
    pub key: String,
    /// Issue or pull request number, where there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<u64>,
    /// Whether `number` is a pull request (comment on it) or an issue.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_pr: bool,
    #[serde(default)]
    pub url: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DepartmentProposal {
    pub id: String,
    pub title: String,
    pub description: String,
    #[serde(default)]
    pub rules: String,
    #[serde(default)]
    pub default_risk: Risk,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleStatus {
    Proposed,
    Added,
    Rejected,
    Reverted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cost_usd: Option<f64>,
    },
    Task {
        task: Box<TaskSnapshot>,
    },
    /// A department rule changed: proposed by an agent, added, rejected or
    /// reverted by the owner. `id` is the proposal, or the commit once added.
    Rule {
        target: String,
        id: String,
        text: String,
        status: RuleStatus,
    },
    Message {
        role: Role,
        text: String,
        delta: bool,
    },
    /// What the agent thought before acting: Claude's thinking summary or
    /// Codex's reasoning summary. Shown apart from messages, never sent on.
    Thinking {
        text: String,
    },
    /// Live-only text fragment, identified within an agent session.
    StreamDelta {
        block_id: String,
        thinking: bool,
        text: String,
    },
    /// Associates the next completed message or thinking event with a draft.
    StreamEnd {
        block_id: String,
        thinking: bool,
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
    /// The owner wrote to an agent of a running department task. The note
    /// waits for the end of the agent's turn and goes in with its next prompt.
    Note {
        id: String,
        text: String,
        status: NoteStatus,
    },
}

impl Event {
    /// Transient events share the preceding durable sequence and never enter history.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::StreamDelta { .. } | Self::StreamEnd { .. } | Self::Message { delta: true, .. }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteStatus {
    Queued,
    Delivered,
    /// The run ended (or the server restarted) before the agent took another turn.
    Dropped,
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
            Event::Usage {
                input_tokens: 31,
                output_tokens: 7,
                cost_usd: Some(0.12),
            },
            Event::Usage {
                input_tokens: 0,
                output_tokens: 0,
                cost_usd: None,
            },
            Event::Task {
                task: Box::new(TaskSnapshot {
                    id: "12345678".into(),
                    title: "task".into(),
                    description: String::new(),
                    status: TaskStatus::Todo,
                    assignee: Some(AgentId::from("claude")),
                    progress: 10,
                    labels: vec!["test".into()],
                    note: "note".into(),
                    thread: None,
                    created_ms: 1,
                    updated_ms: 2,
                    work: None,
                }),
            },
            Event::Message {
                role: Role::Assistant,
                text: "hello".into(),
                delta: true,
            },
            Event::Thinking {
                text: "plan".into(),
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
            Event::Note {
                id: "n1".into(),
                text: "check".into(),
                status: NoteStatus::Queued,
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
