use crate::{
    hub::{Actor, Hub},
    team::{AccessMode, Member, MemberSandbox, Team},
};
use agentsflow_adapters::{
    claude::{ClaudeOptions, ClaudeSession},
    codex::{CodexOptions, CodexSession, SandboxMode},
};
use agentsflow_events::{AgentId, AgentKind};
use anyhow::Result;
use std::{collections::HashMap, path::PathBuf};
use tokio::task::JoinHandle;

pub type Agents = HashMap<AgentId, Actor>;
pub type MailAgentTokens = HashMap<AgentId, String>;

#[derive(Clone)]
pub struct RuntimeOptions {
    pub cwd: PathBuf,
    pub team: Team,
    pub claude_model: Option<String>,
    pub codex_model: Option<String>,
    pub sandbox: Option<SandboxMode>,
    pub codex_bin: Option<PathBuf>,
    pub yes: bool,
    pub allow_bypass: bool,
    pub log: Option<PathBuf>,
    pub mail: Option<agentsflow_adapters::MailConfig>,
    pub mail_agent_tokens: MailAgentTokens,
    pub mail_thread_limit: usize,
}
impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            team: Team::preset("pair").expect("pair preset is valid"),
            claude_model: None,
            codex_model: None,
            sandbox: None,
            codex_bin: None,
            yes: false,
            allow_bypass: false,
            log: None,
            mail: None,
            mail_agent_tokens: MailAgentTokens::new(),
            mail_thread_limit: crate::hub::MAX_THREAD_MAILS,
        }
    }
}
pub struct Runtime {
    pub hub: Hub,
    pub agents: Agents,
    pub team: Team,
    pub cwd: PathBuf,
    tasks: Vec<JoinHandle<()>>,
    mail_stop: Option<tokio::sync::watch::Sender<bool>>,
}

#[derive(Debug, PartialEq, Eq)]
struct EffectiveAccess {
    mode: AccessMode,
    sandbox: Option<MemberSandbox>,
}

fn effective_access(
    member: &Member,
    yes: bool,
    global_sandbox: Option<SandboxMode>,
) -> EffectiveAccess {
    let mode = member.mode.unwrap_or(if yes {
        AccessMode::Auto
    } else {
        AccessMode::Ask
    });
    let sandbox = if member.kind == AgentKind::Codex && mode != AccessMode::Bypass {
        Some(member.sandbox.unwrap_or(match global_sandbox {
            Some(SandboxMode::Workspace_write) => MemberSandbox::WorkspaceWrite,
            _ => MemberSandbox::ReadOnly,
        }))
    } else {
        None
    };
    EffectiveAccess { mode, sandbox }
}

impl Runtime {
    pub fn new(options: RuntimeOptions) -> Result<Self> {
        options.team.validate()?;
        options.team.check_bypass(options.allow_bypass)?;
        let mut team = options.team.clone();
        for member in &mut team.members {
            let access = effective_access(member, options.yes, options.sandbox.clone());
            member.mode = Some(access.mode);
            member.sandbox = access.sandbox;
        }
        let cwd = std::path::absolute(options.cwd)?;
        let hub = options.log.as_deref().map_or_else(Hub::new, Hub::with_log);
        let mail = options.mail.map(|mut mail| {
            mail.thread_limit = options.mail_thread_limit;
            mail
        });
        if mail.is_some()
            && options
                .team
                .ids()
                .any(|id| !options.mail_agent_tokens.contains_key(id))
        {
            anyhow::bail!("personal mail token is missing for an enabled agent");
        }
        hub.set_mail_enabled(mail.is_some());
        hub.set_mail_thread_limit(options.mail_thread_limit);
        let mut tasks = Vec::new();
        let mut agents = Agents::new();
        for member in &team.members {
            let cwd = cwd.clone();
            let model = member.model.clone().or_else(|| match member.kind {
                AgentKind::Claude => options.claude_model.clone(),
                AgentKind::Codex => options.codex_model.clone(),
            });
            let effort = member.effort.clone();
            let mail_config = mail.clone().map(|mut config| {
                config.id = member.id.to_string();
                config.token = options.mail_agent_tokens[&member.id].clone();
                config.peers = options
                    .team
                    .others(&member.id)
                    .map(|m| m.id.to_string())
                    .chain(std::iter::once("orchestrator".into()))
                    .collect();
                config
            });
            let yes = member.mode != Some(AccessMode::Ask);
            let bypass = member.mode == Some(AccessMode::Bypass);
            let (actor, task) = match member.kind {
                AgentKind::Claude => {
                    crate::hub::spawn(hub.clone(), member.id.clone(), yes, move || {
                        ClaudeSession::start(ClaudeOptions {
                            cwd: Some(cwd.clone()),
                            model: model.clone(),
                            mail: mail_config.clone(),
                            effort: effort.clone(),
                            bypass,
                        })
                    })
                }
                AgentKind::Codex => {
                    let sandbox = member.sandbox.map(|sandbox| match sandbox {
                        MemberSandbox::ReadOnly => SandboxMode::Read_only,
                        MemberSandbox::WorkspaceWrite => SandboxMode::Workspace_write,
                    });
                    let bin = options.codex_bin.clone();
                    crate::hub::spawn(hub.clone(), member.id.clone(), yes, move || {
                        CodexSession::start(CodexOptions {
                            cwd: Some(cwd.clone()),
                            model: model.clone(),
                            sandbox: sandbox.clone(),
                            bin: bin.clone(),
                            mail: mail_config.clone(),
                            effort: effort.clone(),
                            bypass,
                        })
                    })
                }
            };
            tasks.push(task);
            agents.insert(member.id.clone(), actor);
        }
        let mail_stop = if mail.is_some() {
            let (tx, rx) = tokio::sync::watch::channel(false);
            let wake_agents = options
                .team
                .ids()
                .map(|id| (id.clone(), agents[id].clone()))
                .collect();
            tasks.push(tokio::task::spawn_local(crate::mail::wake(
                hub.clone(),
                wake_agents,
                team.clone(),
                rx,
            )));
            Some(tx)
        } else {
            None
        };
        Ok(Self {
            hub,
            agents,
            team,
            cwd,
            tasks,
            mail_stop,
        })
    }
    pub async fn shutdown(self) {
        if let Some(stop) = self.mail_stop {
            let _ = stop.send(true);
        }
        for actor in self.agents.into_values() {
            actor.shutdown().await;
        }
        for task in self.tasks {
            let _ = task.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_inherits_flags_and_respects_member_overrides() {
        let team = Team::preset("pair").unwrap();
        let claude = &team.members[0];
        let codex = &team.members[1];
        assert_eq!(
            effective_access(claude, false, None),
            EffectiveAccess {
                mode: AccessMode::Ask,
                sandbox: None
            }
        );
        assert_eq!(
            effective_access(claude, true, None),
            EffectiveAccess {
                mode: AccessMode::Auto,
                sandbox: None
            }
        );
        assert_eq!(
            effective_access(codex, true, Some(SandboxMode::Workspace_write)),
            EffectiveAccess {
                mode: AccessMode::Auto,
                sandbox: Some(MemberSandbox::WorkspaceWrite)
            }
        );
        let mut overridden = codex.clone();
        overridden.mode = Some(AccessMode::Ask);
        overridden.sandbox = Some(MemberSandbox::ReadOnly);
        assert_eq!(
            effective_access(&overridden, true, Some(SandboxMode::Workspace_write)),
            EffectiveAccess {
                mode: AccessMode::Ask,
                sandbox: Some(MemberSandbox::ReadOnly)
            }
        );
        overridden.mode = Some(AccessMode::Bypass);
        overridden.sandbox = None;
        assert_eq!(
            effective_access(&overridden, false, None),
            EffectiveAccess {
                mode: AccessMode::Bypass,
                sandbox: None
            }
        );
        let trio = Team::preset("trio").unwrap();
        assert_eq!(
            effective_access(&trio.members[1], false, None),
            effective_access(&trio.members[2], false, None)
        );
        assert_eq!(
            effective_access(&trio.members[1], false, None).sandbox,
            Some(MemberSandbox::WorkspaceWrite)
        );
    }
}
