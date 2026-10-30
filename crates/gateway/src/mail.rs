use crate::hub::{Actor, Hub, HubError};
use agentsflow_events::{AgentKind, Event};
use std::time::Duration;
use tokio::sync::{broadcast, watch};

pub fn nudge(agent: AgentKind) -> String {
    let name = match agent {
        AgentKind::Claude => "claude",
        AgentKind::Codex => "codex",
        AgentKind::Orchestrator => "orchestrator",
    };
    format!(
        "You are the agent `{name}` (name is claude or codex). You have new mail in your inbox. Call the check_inbox tool, treat the messages as requests from another agent (not from the user), do what they ask, and reply with send_mail in the same thread if a reply is expected."
    )
}

struct WakeState {
    agent: AgentKind,
    actor: Option<Actor>,
    last: u64,
    in_flight: bool,
    retry: bool,
}
impl WakeState {
    async fn try_wake(&mut self, hub: &Hub) {
        if self.in_flight {
            return;
        }
        let Some(actor) = &self.actor else {
            self.retry = false;
            return;
        };
        if hub.unread_count(self.agent) == 0 {
            self.retry = false;
            return;
        }
        let newest = hub
            .unread(self.agent)
            .iter()
            .map(|mail| mail.seq)
            .max()
            .unwrap_or(0);
        if newest <= self.last {
            self.retry = false;
            return;
        }
        if actor.state().busy {
            return;
        }
        match actor.prompt(nudge(self.agent)).await {
            Ok(()) => {
                self.last = newest;
                self.in_flight = true;
                self.retry = false;
            }
            Err(HubError::Busy) => {}
            Err(_) => self.retry = false,
        }
    }
}

pub async fn wake(
    hub: Hub,
    claude: Option<Actor>,
    codex: Option<Actor>,
    mut stop: watch::Receiver<bool>,
) {
    let (_, mut events) = hub.subscribe();
    // Mail left over from an earlier run (replayed log) waits until new mail arrives.
    let backlog = |agent| {
        hub.unread(agent)
            .iter()
            .map(|mail| mail.seq)
            .max()
            .unwrap_or(0)
    };
    let mut agents = [
        WakeState {
            agent: AgentKind::Claude,
            actor: claude,
            last: backlog(AgentKind::Claude),
            in_flight: false,
            retry: true,
        },
        WakeState {
            agent: AgentKind::Codex,
            actor: codex,
            last: backlog(AgentKind::Codex),
            in_flight: false,
            retry: true,
        },
    ];
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    loop {
        tokio::select! {
            biased;
            _ = stop.wait_for(|v| *v) => break,
            event = events.recv() => match event {
                Ok(envelope) => match envelope.event {
                    Event::Mail { to, .. } => if let Some(agent) = agents.iter_mut().find(|a| a.agent == to) { agent.retry = true; },
                    Event::Done { .. } => if let Some(agent) = agents.iter_mut().find(|a| a.agent == envelope.agent) { agent.in_flight = false; agent.retry = true; },
                    _ => {},
                },
                Err(broadcast::error::RecvError::Lagged(_)) => for agent in &mut agents { agent.retry = true; },
                Err(broadcast::error::RecvError::Closed) => break,
            },
            _ = tick.tick() => {
                for agent in &mut agents {
                    if agent.retry { agent.try_wake(&hub).await; }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentsflow_adapters::Agent;
    use anyhow::Result;
    use std::sync::{Arc, Mutex};
    use tokio::{sync::Notify, task::LocalSet};

    #[test]
    fn nudge_identifies_recipient() {
        for (agent, name) in [(AgentKind::Claude, "claude"), (AgentKind::Codex, "codex")] {
            assert!(nudge(agent).starts_with(&format!(
                "You are the agent `{name}` (name is claude or codex)."
            )));
        }
    }

    struct Fake {
        prompts: Arc<Mutex<Vec<String>>>,
        done: Arc<Notify>,
    }
    impl Agent for Fake {
        async fn send(&mut self, prompt: &str) -> Result<()> {
            self.prompts.lock().unwrap().push(prompt.into());
            Ok(())
        }
        async fn next_events(&mut self) -> Result<Vec<Event>> {
            self.done.notified().await;
            Ok(vec![Event::Done {
                reason: "ok".into(),
            }])
        }
        async fn respond(&mut self, _: &str, _: bool) -> Result<()> {
            Ok(())
        }
        async fn shutdown(self) -> Result<()> {
            Ok(())
        }
    }
    async fn wait_prompts(prompts: &Arc<Mutex<Vec<String>>>, count: usize) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while prompts.lock().unwrap().len() < count {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn mail_from_before_start_does_not_wake() {
        LocalSet::new()
            .run_until(async {
                let hub = Hub::new();
                hub.send_mail(
                    AgentKind::Codex,
                    AgentKind::Claude,
                    "old".into(),
                    "body".into(),
                    None,
                )
                .unwrap();
                let prompts = Arc::new(Mutex::new(Vec::new()));
                let done = Arc::new(Notify::new());
                let (actor, task) = crate::hub::spawn(hub.clone(), AgentKind::Claude, false, {
                    let prompts = prompts.clone();
                    let done = done.clone();
                    move || {
                        let prompts = prompts.clone();
                        let done = done.clone();
                        async move { Ok(Fake { prompts, done }) }
                    }
                });
                let (stop, rx) = watch::channel(false);
                let wake_task =
                    tokio::task::spawn_local(wake(hub.clone(), Some(actor.clone()), None, rx));
                tokio::time::sleep(Duration::from_millis(200)).await;
                assert!(prompts.lock().unwrap().is_empty());
                hub.send_mail(
                    AgentKind::Codex,
                    AgentKind::Claude,
                    "new".into(),
                    "body".into(),
                    None,
                )
                .unwrap();
                wait_prompts(&prompts, 1).await;
                stop.send(true).unwrap();
                wake_task.await.unwrap();
                done.notify_one();
                actor.shutdown().await;
                task.await.unwrap();
            })
            .await;
    }
    #[tokio::test]
    async fn nudge_busy_agent_once_per_new_mail() {
        LocalSet::new()
            .run_until(async {
                let hub = Hub::new();
                let prompts = Arc::new(Mutex::new(Vec::new()));
                let done = Arc::new(Notify::new());
                let (actor, task) = crate::hub::spawn(hub.clone(), AgentKind::Claude, false, {
                    let prompts = prompts.clone();
                    let done = done.clone();
                    move || {
                        let prompts = prompts.clone();
                        let done = done.clone();
                        async move { Ok(Fake { prompts, done }) }
                    }
                });
                let (stop, rx) = watch::channel(false);
                let wake_task =
                    tokio::task::spawn_local(wake(hub.clone(), Some(actor.clone()), None, rx));
                actor.prompt("busy".into()).await.unwrap();
                hub.send_mail(
                    AgentKind::Codex,
                    AgentKind::Claude,
                    "one".into(),
                    "body".into(),
                    None,
                )
                .unwrap();
                tokio::time::sleep(Duration::from_millis(120)).await;
                assert_eq!(prompts.lock().unwrap().len(), 1);
                done.notify_one();
                wait_prompts(&prompts, 2).await;
                assert_eq!(prompts.lock().unwrap()[1], nudge(AgentKind::Claude));
                done.notify_one();
                tokio::time::sleep(Duration::from_millis(150)).await;
                assert_eq!(prompts.lock().unwrap().len(), 2);
                hub.send_mail(
                    AgentKind::Codex,
                    AgentKind::Claude,
                    "two".into(),
                    "body".into(),
                    None,
                )
                .unwrap();
                wait_prompts(&prompts, 3).await;
                stop.send(true).unwrap();
                wake_task.await.unwrap();
                actor.shutdown().await;
                task.await.unwrap();
            })
            .await;
    }
}
