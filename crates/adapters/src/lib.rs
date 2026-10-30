//! Адаптеры агентов.

pub mod claude;
pub mod codex;

use agentsflow_events::Event;
use anyhow::Result;

#[derive(Clone, Debug)]
pub struct MailConfig {
    pub url: String,
    pub token: String,
    pub exe: std::path::PathBuf,
    pub thread_limit: usize,
    pub id: String,
    pub peers: Vec<String>,
}

#[allow(async_fn_in_trait)]
pub trait Agent {
    async fn send(&mut self, prompt: &str) -> Result<()>;
    async fn next_events(&mut self) -> Result<Vec<Event>>;
    async fn respond(&mut self, request_id: &str, allow: bool) -> Result<()>;
    async fn shutdown(self) -> Result<()>;
}
