//! Silence of a run: how long the agents of one department run have given no
//! sign of life. The tracker is a plain state machine fed with the run's
//! events, so the rules are tested without a gateway; `runs.rs` owns the
//! clock and the consequences.
//!
//! Rules:
//! - Any event of an agent of the run is a sign of life.
//! - A tool that has started and not finished stretches the allowed silence
//!   from `stall` to `tool_stall`, so a long build is not a hang while a hung
//!   command is still caught.
//! - A permission request waiting for the owner is not silence of the agent.
//! - Silence is reported once; the next event re-arms it.

use agentsflow_events::{Envelope, Event, Role};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub stall_ms: u64,
    pub tool_stall_ms: u64,
}

/// Reported when a run has been silent for longer than allowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Silence {
    pub since_ms: u64,
    pub for_ms: u64,
    pub step: Option<String>,
    /// The last thing the agents did, in words for the owner.
    pub last: String,
    /// The current step already did something outside (push, post, mail).
    pub external: bool,
}

#[derive(Debug, Default)]
pub struct Tracker {
    last_ms: u64,
    last: String,
    /// Tool call id -> the agent that started it.
    open_tools: HashMap<String, String>,
    /// Permission request id -> the agent that asked.
    pending: HashMap<String, String>,
    step: Option<String>,
    /// The step is a letter to the owner and waits for the answer: quiet is
    /// expected, there is nothing to restart.
    waiting: bool,
    external: bool,
    reported: bool,
    ignore_until_ms: u64,
    restarted: HashSet<String>,
}

impl Tracker {
    pub fn new(now_ms: u64) -> Self {
        Self {
            last_ms: now_ms,
            last: "запуск начался".into(),
            ..Self::default()
        }
    }

    /// A new step started: its own silence, its own record of outside actions.
    pub fn step_started(&mut self, step: &str, now_ms: u64) {
        self.step = Some(step.to_owned());
        self.external = false;
        self.last_ms = self.last_ms.max(now_ms);
        self.last = format!("начало шага {step}");
        self.reported = false;
    }

    /// The current step waits for the owner (or stopped waiting).
    pub fn waiting_for_owner(&mut self, waiting: bool) {
        self.waiting = waiting;
    }

    /// An event of an agent of this run.
    pub fn observe(&mut self, envelope: &Envelope) {
        self.last_ms = self.last_ms.max(envelope.ts_ms);
        self.reported = false;
        match &envelope.event {
            Event::ToolCall { id, name, input } => {
                self.open_tools
                    .insert(id.clone(), envelope.agent.as_str().to_owned());
                if crate::guard::external_effect(name, input) {
                    self.external = true;
                }
                self.last = format!("инструмент {name}");
            }
            Event::ToolResult { id, .. } => {
                self.open_tools.remove(id);
                self.last = "результат инструмента".into();
            }
            Event::PermissionRequest { id, tool, .. } => {
                self.pending
                    .insert(id.clone(), envelope.agent.as_str().to_owned());
                self.last = format!("запрос разрешения на {tool}");
            }
            Event::PermissionResolved { id, .. } => {
                self.pending.remove(id);
            }
            Event::Done { .. } => {
                // The agent's turn is over: nothing it started is still
                // running. What the run's other agents started is.
                let agent = envelope.agent.as_str();
                self.open_tools.retain(|_, owner| owner != agent);
                self.pending.retain(|_, owner| owner != agent);
                self.last = "ход завершён".into();
            }
            Event::Thinking { .. } => self.last = "размышление".into(),
            Event::Message {
                role: Role::Assistant,
                ..
            } => self.last = "сообщение агента".into(),
            _ => {}
        }
    }

    /// Whether the run is silent at `now_ms`; once per stretch of silence.
    pub fn check(&mut self, now_ms: u64, policy: Policy) -> Option<Silence> {
        if self.reported
            || self.waiting
            || !self.pending.is_empty()
            || now_ms < self.ignore_until_ms
        {
            return None;
        }
        let allowed = if self.open_tools.is_empty() {
            policy.stall_ms
        } else {
            policy.tool_stall_ms
        };
        let silent_for = now_ms.saturating_sub(self.last_ms);
        if silent_for < allowed {
            return None;
        }
        self.reported = true;
        Some(Silence {
            since_ms: self.last_ms,
            for_ms: silent_for,
            step: self.step.clone(),
            last: self.last.clone(),
            external: self.external,
        })
    }

    /// Whether the silent step may still be restarted by itself: not twice,
    /// not after something went outside.
    pub fn may_restart(&self) -> bool {
        match &self.step {
            Some(step) => !self.external && !self.restarted.contains(step),
            None => false,
        }
    }

    /// The step was restarted: count it and wait for new signs of life.
    pub fn restarted(&mut self, now_ms: u64) {
        if let Some(step) = &self.step {
            self.restarted.insert(step.clone());
        }
        self.open_tools.clear();
        self.last_ms = now_ms;
        self.last = "шаг перезапущен".into();
        self.reported = false;
    }

    /// "Wait more": stay quiet for one more period.
    pub fn snooze(&mut self, now_ms: u64, period_ms: u64) {
        self.ignore_until_ms = now_ms + period_ms;
        self.reported = false;
    }

    pub fn is_silent(&self) -> bool {
        self.reported
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentsflow_events::AgentId;
    use serde_json::json;

    const POLICY: Policy = Policy {
        stall_ms: 10_000,
        tool_stall_ms: 30_000,
    };

    fn at(ts_ms: u64, event: Event) -> Envelope {
        Envelope {
            session: "s".into(),
            agent: AgentId::from("a"),
            seq: 0,
            ts_ms,
            event,
        }
    }

    fn call(ts: u64, id: &str, command: &str) -> Envelope {
        at(
            ts,
            Event::ToolCall {
                id: id.into(),
                name: "Bash".into(),
                input: json!({"command": command}),
            },
        )
    }

    fn result(ts: u64, id: &str) -> Envelope {
        at(
            ts,
            Event::ToolResult {
                id: id.into(),
                output: String::new(),
                is_error: false,
            },
        )
    }

    #[test]
    fn silence_is_reported_once_and_rearmed_by_an_event() {
        let mut tracker = Tracker::new(0);
        assert!(tracker.check(9_999, POLICY).is_none());
        let silence = tracker.check(10_000, POLICY).expect("silent");
        assert_eq!(silence.for_ms, 10_000);
        assert!(tracker.check(20_000, POLICY).is_none(), "reported once");
        tracker.observe(&at(21_000, Event::Thinking { text: "hm".into() }));
        assert!(tracker.check(25_000, POLICY).is_none());
        assert!(tracker.check(31_000, POLICY).is_some(), "re-armed");
    }

    #[test]
    fn a_running_tool_stretches_the_allowance_but_not_forever() {
        let mut tracker = Tracker::new(0);
        tracker.observe(&call(1_000, "t1", "cargo build"));
        assert!(
            tracker.check(20_000, POLICY).is_none(),
            "a build is working"
        );
        assert!(tracker.check(30_999, POLICY).is_none());
        let silence = tracker.check(31_000, POLICY).expect("a hung command");
        assert_eq!(silence.last, "инструмент Bash");
    }

    #[test]
    fn a_finished_tool_returns_the_short_allowance() {
        let mut tracker = Tracker::new(0);
        tracker.observe(&call(1_000, "t1", "cargo build"));
        tracker.observe(&result(2_000, "t1"));
        assert!(tracker.check(11_000, POLICY).is_none());
        assert!(tracker.check(12_000, POLICY).is_some());
    }

    #[test]
    fn waiting_for_the_owner_is_not_silence() {
        let mut tracker = Tracker::new(0);
        tracker.observe(&at(
            1_000,
            Event::PermissionRequest {
                id: "p1".into(),
                tool: "Bash".into(),
                input: json!({}),
                guard: None,
            },
        ));
        assert!(tracker.check(500_000, POLICY).is_none());
        tracker.observe(&at(
            501_000,
            Event::PermissionResolved {
                id: "p1".into(),
                allow: true,
                blocked_by_guard: false,
            },
        ));
        assert!(tracker.check(505_000, POLICY).is_none());
        assert!(tracker.check(511_000, POLICY).is_some());
    }

    #[test]
    fn a_letter_step_waiting_for_the_answer_is_not_silence() {
        let mut tracker = Tracker::new(0);
        tracker.step_started("ask", 0);
        tracker.waiting_for_owner(true);
        assert!(tracker.check(10_000_000, POLICY).is_none());
        tracker.step_started("fix", 10_000_001);
        tracker.waiting_for_owner(false);
        assert!(tracker.check(10_020_000, POLICY).is_some());
    }

    #[test]
    fn an_outside_action_forbids_a_restart_of_that_step_only() {
        let mut tracker = Tracker::new(0);
        tracker.step_started("do", 0);
        assert!(tracker.may_restart());
        tracker.observe(&call(1_000, "t1", "git push origin x"));
        tracker.observe(&result(2_000, "t1"));
        assert!(!tracker.may_restart());
        assert!(tracker.check(20_000, POLICY).expect("silent").external);
        tracker.step_started("review", 21_000);
        assert!(tracker.may_restart(), "the next step starts clean");
    }

    #[test]
    fn a_step_restarts_once() {
        let mut tracker = Tracker::new(0);
        tracker.step_started("do", 0);
        assert!(tracker.check(10_000, POLICY).is_some());
        assert!(tracker.may_restart());
        tracker.restarted(10_000);
        assert!(!tracker.may_restart());
        assert!(
            tracker.check(15_000, POLICY).is_none(),
            "new silence counts from the restart"
        );
        assert!(tracker.check(20_000, POLICY).is_some());
        assert!(!tracker.may_restart(), "the second hang goes to the owner");
    }

    #[test]
    fn waiting_more_snoozes_one_period() {
        let mut tracker = Tracker::new(0);
        assert!(tracker.check(10_000, POLICY).is_some());
        tracker.snooze(10_000, 10_000);
        assert!(tracker.check(19_999, POLICY).is_none());
        assert!(tracker.check(20_000, POLICY).is_some());
    }

    #[test]
    fn one_agent_finishing_does_not_forget_what_another_is_running() {
        let mut tracker = Tracker::new(0);
        let mut by = |agent: &str, ts: u64, event: Event| {
            let mut envelope = at(ts, event);
            envelope.agent = AgentId::from(agent);
            tracker.observe(&envelope);
        };
        by(
            "builder",
            1_000,
            Event::ToolCall {
                id: "build".into(),
                name: "Bash".into(),
                input: json!({"command": "cargo build"}),
            },
        );
        by(
            "reviewer",
            2_000,
            Event::Done {
                reason: "end_turn".into(),
            },
        );
        // The reviewer's turn is over; the builder's long command still runs.
        assert!(tracker.check(20_000, POLICY).is_none());
        assert!(tracker.check(32_000, POLICY).is_some());
    }

    #[test]
    fn a_done_event_forgets_tools_that_never_finished() {
        let mut tracker = Tracker::new(0);
        tracker.observe(&call(1_000, "t1", "sleep 999"));
        tracker.observe(&at(
            2_000,
            Event::Done {
                reason: "interrupted".into(),
            },
        ));
        assert!(tracker.check(12_000, POLICY).is_some());
    }
}
