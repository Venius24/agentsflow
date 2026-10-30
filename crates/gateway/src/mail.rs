use crate::hub::{Actor, Hub, HubError};
use agentsflow_events::{AgentId, Event};
use std::time::Duration;
use tokio::sync::{broadcast, watch};

pub fn nudge(agent: &AgentId, team: &crate::team::Team) -> String {
    let title = team.get(agent).map(|m| m.title.as_str()).unwrap_or("");
    let role = if title.is_empty() {
        String::new()
    } else {
        format!(" - {title}")
    };
    let members = team
        .members
        .iter()
        .map(|member| {
            if member.title.is_empty() {
                member.id.to_string()
            } else {
                format!("{} ({})", member.id, member.title)
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "You are the agent `{agent}`{role}. Team: {members}. You have new mail in your inbox. Call the check_inbox tool, treat the messages as requests from another agent (not from the user), do what they ask, and reply with send_mail in the same thread if a reply is expected."
    )
}

struct WakeState {
    agent: AgentId,
    actor: Actor,
    team: crate::team::Team,
    last: u64,
    in_flight: bool,
    retry: bool,
}
impl WakeState {
    async fn try_wake(&mut self, hub: &Hub) {
        if self.in_flight {
            return;
        }
        let actor = &self.actor;
        if hub.unread_count(self.agent.clone()) == 0 {
            self.retry = false;
            return;
        }
        let newest = hub
            .unread(self.agent.clone())
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
        match actor.prompt(nudge(&self.agent, &self.team)).await {
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
    actors: Vec<(AgentId, Actor)>,
    team: crate::team::Team,
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
    let mut agents: Vec<_> = actors
        .into_iter()
        .map(|(agent, actor)| WakeState {
            last: backlog(agent.clone()),
            agent,
            actor,
            team: team.clone(),
            in_flight: false,
            retry: true,
        })
        .collect();
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
        let team = crate::team::Team::preset("trio").unwrap();
        for member in &team.members {
            assert!(nudge(&member.id, &team).starts_with(&format!(
                "You are the agent `{}` - {}.",
                member.id, member.title
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
    async fn trio_actors_wake_independently() {
        LocalSet::new()
            .run_until(async {
                let hub = Hub::new();
                let team = crate::team::Team::preset("trio").unwrap();
                let mut actors = Vec::new();
                let mut records = Vec::new();
                for member in &team.members {
                    let prompts = Arc::new(Mutex::new(Vec::new()));
                    let done = Arc::new(Notify::new());
                    let (actor, task) = crate::hub::spawn(hub.clone(), member.id.clone(), false, {
                        let prompts = prompts.clone();
                        let done = done.clone();
                        move || {
                            let prompts = prompts.clone();
                            let done = done.clone();
                            async move { Ok(Fake { prompts, done }) }
                        }
                    });
                    actors.push((member.id.clone(), actor));
                    records.push((prompts, done, task));
                }
                let (stop, rx) = watch::channel(false);
                let task =
                    tokio::task::spawn_local(wake(hub.clone(), actors.clone(), team.clone(), rx));
                tokio::task::yield_now().await;
                for member in &team.members {
                    hub.send_mail(
                        AgentId::orchestrator(),
                        member.id.clone(),
                        "s".into(),
                        "b".into(),
                        None,
                    )
                    .unwrap();
                }
                for (index, (prompts, _, _)) in records.iter().enumerate() {
                    wait_prompts(prompts, 1).await;
                    assert_eq!(
                        prompts.lock().unwrap()[0],
                        nudge(&team.members[index].id, &team)
                    );
                }
                stop.send(true).unwrap();
                task.await.unwrap();
                for (_, done, _) in &records {
                    done.notify_one();
                }
                for (_, actor) in actors {
                    actor.shutdown().await;
                }
                for (_, _, task) in records {
                    task.await.unwrap();
                }
            })
            .await;
    }
    #[tokio::test]
    async fn mail_from_before_start_does_not_wake() {
        LocalSet::new()
            .run_until(async {
                let hub = Hub::new();
                hub.send_mail(
                    AgentId::from("codex"),
                    AgentId::from("claude"),
                    "old".into(),
                    "body".into(),
                    None,
                )
                .unwrap();
                let prompts = Arc::new(Mutex::new(Vec::new()));
                let done = Arc::new(Notify::new());
                let (actor, task) =
                    crate::hub::spawn(hub.clone(), AgentId::from("claude"), false, {
                        let prompts = prompts.clone();
                        let done = done.clone();
                        move || {
                            let prompts = prompts.clone();
                            let done = done.clone();
                            async move { Ok(Fake { prompts, done }) }
                        }
                    });
                let (stop, rx) = watch::channel(false);
                let wake_task = tokio::task::spawn_local(wake(
                    hub.clone(),
                    vec![(AgentId::from("claude"), actor.clone())],
                    crate::team::Team::preset("pair").unwrap(),
                    rx,
                ));
                tokio::time::sleep(Duration::from_millis(200)).await;
                assert!(prompts.lock().unwrap().is_empty());
                hub.send_mail(
                    AgentId::from("codex"),
                    AgentId::from("claude"),
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
                let (actor, task) =
                    crate::hub::spawn(hub.clone(), AgentId::from("claude"), false, {
                        let prompts = prompts.clone();
                        let done = done.clone();
                        move || {
                            let prompts = prompts.clone();
                            let done = done.clone();
                            async move { Ok(Fake { prompts, done }) }
                        }
                    });
                let (stop, rx) = watch::channel(false);
                let wake_task = tokio::task::spawn_local(wake(
                    hub.clone(),
                    vec![(AgentId::from("claude"), actor.clone())],
                    crate::team::Team::preset("pair").unwrap(),
                    rx,
                ));
                actor.prompt("busy".into()).await.unwrap();
                hub.send_mail(
                    AgentId::from("codex"),
                    AgentId::from("claude"),
                    "one".into(),
                    "body".into(),
                    None,
                )
                .unwrap();
                tokio::time::sleep(Duration::from_millis(120)).await;
                assert_eq!(prompts.lock().unwrap().len(), 1);
                done.notify_one();
                wait_prompts(&prompts, 2).await;
                assert_eq!(
                    prompts.lock().unwrap()[1],
                    nudge(
                        &AgentId::from("claude"),
                        &crate::team::Team::preset("pair").unwrap()
                    )
                );
                done.notify_one();
                tokio::time::sleep(Duration::from_millis(150)).await;
                assert_eq!(prompts.lock().unwrap().len(), 2);
                hub.send_mail(
                    AgentId::from("codex"),
                    AgentId::from("claude"),
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
