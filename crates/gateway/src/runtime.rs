use crate::hub::{Actor, Hub};
use agentsflow_adapters::{
    claude::{ClaudeOptions, ClaudeSession},
    codex::{CodexOptions, CodexSession, SandboxMode},
};
use agentsflow_events::AgentKind;
use anyhow::Result;
use std::path::PathBuf;
use tokio::task::JoinHandle;

#[derive(Clone)]
pub struct RuntimeOptions {
    pub cwd: PathBuf,
    pub claude: bool,
    pub codex: bool,
    pub claude_model: Option<String>,
    pub codex_model: Option<String>,
    pub sandbox: Option<SandboxMode>,
    pub codex_bin: Option<PathBuf>,
    pub yes: bool,
    pub log: Option<PathBuf>,
    pub mail: Option<agentsflow_adapters::MailConfig>,
    pub mail_thread_limit: usize,
}
impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            claude: true,
            codex: true,
            claude_model: None,
            codex_model: None,
            sandbox: None,
            codex_bin: None,
            yes: false,
            log: None,
            mail: None,
            mail_thread_limit: crate::hub::MAX_THREAD_MAILS,
        }
    }
}
pub struct Runtime {
    pub hub: Hub,
    pub claude: Option<Actor>,
    pub codex: Option<Actor>,
    pub cwd: PathBuf,
    tasks: Vec<JoinHandle<()>>,
    mail_stop: Option<tokio::sync::watch::Sender<bool>>,
}
impl Runtime {
    pub fn new(options: RuntimeOptions) -> Result<Self> {
        let cwd = std::path::absolute(options.cwd)?;
        let hub = options.log.as_deref().map_or_else(Hub::new, Hub::with_log);
        let mail = options.mail.map(|mut mail| {
            mail.thread_limit = options.mail_thread_limit;
            mail
        });
        hub.set_mail_enabled(mail.is_some());
        hub.set_mail_thread_limit(
            mail.as_ref()
                .map_or(options.mail_thread_limit, |mail| mail.thread_limit),
        );
        let mut tasks = Vec::new();
        let claude = if options.claude {
            let cwd = cwd.clone();
            let model = options.claude_model;
            let mail = mail.clone();
            let (actor, task) =
                crate::hub::spawn(hub.clone(), AgentKind::Claude, options.yes, move || {
                    ClaudeSession::start(ClaudeOptions {
                        cwd: Some(cwd.clone()),
                        model: model.clone(),
                        mail: mail.clone(),
                    })
                });
            tasks.push(task);
            Some(actor)
        } else {
            None
        };
        let codex = if options.codex {
            let cwd = cwd.clone();
            let model = options.codex_model;
            let sandbox = options.sandbox;
            let bin = options.codex_bin;
            let mail = mail.clone();
            let (actor, task) =
                crate::hub::spawn(hub.clone(), AgentKind::Codex, options.yes, move || {
                    CodexSession::start(CodexOptions {
                        cwd: Some(cwd.clone()),
                        model: model.clone(),
                        sandbox: sandbox.clone(),
                        bin: bin.clone(),
                        mail: mail.clone(),
                    })
                });
            tasks.push(task);
            Some(actor)
        } else {
            None
        };
        let mail_stop = if mail.is_some() {
            let (tx, rx) = tokio::sync::watch::channel(false);
            tasks.push(tokio::task::spawn_local(crate::mail::wake(
                hub.clone(),
                claude.clone(),
                codex.clone(),
                rx,
            )));
            Some(tx)
        } else {
            None
        };
        Ok(Self {
            hub,
            claude,
            codex,
            cwd,
            tasks,
            mail_stop,
        })
    }
    pub async fn shutdown(self) {
        if let Some(stop) = self.mail_stop {
            let _ = stop.send(true);
        }
        if let Some(actor) = self.claude {
            actor.shutdown().await;
        }
        if let Some(actor) = self.codex {
            actor.shutdown().await;
        }
        for task in self.tasks {
            let _ = task.await;
        }
    }
}
