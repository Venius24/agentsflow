//! Адаптеры агентов.

pub mod claude;
pub mod codex;

use agentsflow_events::Event;
use anyhow::Result;
use std::rc::Rc;

#[derive(Clone, Debug)]
pub struct MailConfig {
    pub url: String,
    pub token: String,
    pub exe: std::path::PathBuf,
    pub thread_limit: usize,
    pub id: String,
    pub peers: Vec<String>,
}

/// Cloneable handle that asks an agent to stop its current turn. `next_events`
/// is not cancel-safe and keeps `&mut` on the agent while it waits, so the
/// request has to travel through a handle taken before the turn starts.
#[derive(Clone)]
pub struct Interrupter(Rc<dyn Fn() -> Result<()>>);

impl Interrupter {
    pub fn new(request: impl Fn() -> Result<()> + 'static) -> Self {
        Self(Rc::new(request))
    }
    pub fn unsupported() -> Self {
        Self::new(|| anyhow::bail!("interrupt is not supported by this agent"))
    }
    pub fn interrupt(&self) -> Result<()> {
        (self.0)()
    }
}

#[allow(async_fn_in_trait)]
pub trait Agent {
    async fn send(&mut self, prompt: &str) -> Result<()>;
    async fn next_events(&mut self) -> Result<Vec<Event>>;
    async fn respond(&mut self, request_id: &str, allow: bool) -> Result<()>;
    async fn respond_with_reason(
        &mut self,
        request_id: &str,
        allow: bool,
        _reason: Option<&str>,
    ) -> Result<()> {
        self.respond(request_id, allow).await
    }
    /// Handle for interrupting the active turn while `next_events` is pending.
    fn interrupter(&self) -> Interrupter {
        Interrupter::unsupported()
    }
    fn interrupt(&self) -> Result<()> {
        self.interrupter().interrupt()
    }
    async fn shutdown(self) -> Result<()>;
}
