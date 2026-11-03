//! Fake agent for tests that interrupt a turn.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use agentsflow_adapters::{Agent, Interrupter};
use agentsflow_events::{Event, Role};
use anyhow::Result;
use tokio::sync::mpsc;

/// The first `silent_turns` turns say nothing (after an optional permission
/// request with id "p") until they are interrupted; later turns answer at once.
pub struct Silent {
    silent_turns: usize,
    permission_first: bool,
    turns: usize,
    silent: bool,
    asked: bool,
    signal: mpsc::UnboundedSender<()>,
    interrupted: mpsc::UnboundedReceiver<()>,
    pub answers: Arc<Mutex<Vec<bool>>>,
    pub interrupts: Arc<AtomicUsize>,
}

impl Silent {
    pub fn new(silent_turns: usize, permission_first: bool) -> Self {
        let (signal, interrupted) = mpsc::unbounded_channel();
        Self {
            silent_turns,
            permission_first,
            turns: 0,
            silent: false,
            asked: false,
            signal,
            interrupted,
            answers: Arc::default(),
            interrupts: Arc::default(),
        }
    }
    /// Share the counters with the test, which cannot reach the agent once the
    /// actor owns it.
    pub fn probes(mut self, answers: Arc<Mutex<Vec<bool>>>, interrupts: Arc<AtomicUsize>) -> Self {
        self.answers = answers;
        self.interrupts = interrupts;
        self
    }
}

impl Agent for Silent {
    async fn send(&mut self, _: &str) -> Result<()> {
        while self.interrupted.try_recv().is_ok() {}
        self.silent = self.turns < self.silent_turns;
        self.asked = false;
        self.turns += 1;
        Ok(())
    }
    async fn next_events(&mut self) -> Result<Vec<Event>> {
        if !self.silent {
            return Ok(vec![
                Event::Message {
                    role: Role::Assistant,
                    text: "ok".into(),
                    delta: false,
                },
                Event::Done {
                    reason: "ok".into(),
                },
            ]);
        }
        if self.permission_first && !self.asked {
            self.asked = true;
            return Ok(vec![Event::PermissionRequest {
                id: "p".into(),
                tool: "shell".into(),
                input: serde_json::json!({}),
                guard: None,
            }]);
        }
        self.interrupted.recv().await;
        self.silent = false;
        Ok(vec![Event::Done {
            reason: "cancelled".into(),
        }])
    }
    async fn respond(&mut self, _: &str, allow: bool) -> Result<()> {
        self.answers.lock().unwrap().push(allow);
        Ok(())
    }
    fn interrupter(&self) -> Interrupter {
        let signal = self.signal.clone();
        let count = self.interrupts.clone();
        Interrupter::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
            signal.send(()).map_err(|_| anyhow::anyhow!("agent closed"))
        })
    }
    async fn shutdown(self) -> Result<()> {
        Ok(())
    }
}

/// Answers each prompt with the next scripted reply (the last one repeats),
/// reporting `cost` dollars per turn; a reply starting with `write:` also
/// writes the rest of it to `result.md` in `dir`.
pub struct Scripted {
    replies: std::collections::VecDeque<String>,
    last: String,
    pending: Option<String>,
    cost: Option<f64>,
    dir: std::path::PathBuf,
}

impl Scripted {
    pub fn new(replies: &[&str], cost: Option<f64>, dir: &std::path::Path) -> Self {
        Self {
            replies: replies.iter().map(|reply| reply.to_string()).collect(),
            last: replies.last().copied().unwrap_or("ok").to_owned(),
            pending: None,
            cost,
            dir: dir.to_path_buf(),
        }
    }
}

impl Agent for Scripted {
    async fn send(&mut self, _: &str) -> Result<()> {
        self.pending = Some(
            self.replies
                .pop_front()
                .unwrap_or_else(|| self.last.clone()),
        );
        Ok(())
    }
    async fn next_events(&mut self) -> Result<Vec<Event>> {
        let mut text = self.pending.take().unwrap_or_default();
        if let Some(message) = text.strip_prefix("failed:") {
            return Ok(vec![
                Event::Error {
                    message: message.to_owned(),
                },
                Event::Done {
                    reason: "failed".into(),
                },
            ]);
        }
        if let Some(result) = text.strip_prefix("write:") {
            std::fs::write(self.dir.join("result.md"), result)?;
            text = "записал result.md".into();
        }
        Ok(vec![
            Event::Usage {
                input_tokens: 10,
                output_tokens: 5,
                cost_usd: self.cost,
            },
            Event::Message {
                role: Role::Assistant,
                text,
                delta: false,
            },
            Event::Done {
                reason: "ok".into(),
            },
        ])
    }
    async fn respond(&mut self, _: &str, _: bool) -> Result<()> {
        Ok(())
    }
    async fn shutdown(self) -> Result<()> {
        Ok(())
    }
}
