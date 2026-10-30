use std::{
    collections::VecDeque,
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use agentsflow_adapters::Agent;
use agentsflow_events::{AgentKind, Envelope, Event, Role};
use anyhow::Result;
use serde::Serialize;
use tokio::{
    sync::{broadcast, mpsc, oneshot, watch},
    task::JoinHandle,
};
use uuid::Uuid;

#[derive(Clone)]
pub struct Hub {
    inner: Arc<Inner>,
}
struct Inner {
    seq: AtomicU64,
    data: Mutex<Data>,
    tx: broadcast::Sender<Envelope>,
}
struct Data {
    history: VecDeque<Envelope>,
}

impl Hub {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self {
            inner: Arc::new(Inner {
                seq: AtomicU64::new(0),
                data: Mutex::new(Data {
                    history: VecDeque::new(),
                }),
                tx,
            }),
        }
    }
    pub fn publish(&self, agent: AgentKind, session: &str, event: Event) {
        let mut data = self.inner.data.lock().unwrap();
        let envelope = Envelope {
            session: session.into(),
            agent,
            seq: self.inner.seq.fetch_add(1, Ordering::Relaxed) + 1,
            ts_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(u64::MAX as u128) as u64,
            event,
        };
        if data.history.len() == 5000 {
            data.history.pop_front();
        }
        data.history.push_back(envelope.clone());
        let _ = self.inner.tx.send(envelope);
    }
    pub fn subscribe(&self) -> (Vec<Envelope>, broadcast::Receiver<Envelope>) {
        let data = self.inner.data.lock().unwrap();
        let rx = self.inner.tx.subscribe();
        (data.history.iter().cloned().collect(), rx)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HubError {
    Busy,
    NotFound,
    Closed,
}
#[derive(Clone, Default, Serialize)]
pub struct AgentState {
    pub enabled: bool,
    pub busy: bool,
    pub pending: Vec<String>,
}
#[derive(Clone)]
pub struct Actor {
    tx: mpsc::UnboundedSender<Command>,
    stop: Arc<watch::Sender<bool>>,
    state: Arc<Mutex<AgentState>>,
}
type Reply = oneshot::Sender<std::result::Result<(), HubError>>;
enum Command {
    Prompt(String, Reply),
    Respond(String, bool, Reply),
    Shutdown(Reply),
}

impl Actor {
    pub fn state(&self) -> AgentState {
        self.state.lock().unwrap().clone()
    }
    pub async fn prompt(&self, prompt: String) -> std::result::Result<(), HubError> {
        let (tx, rx) = oneshot::channel();
        {
            let mut state = self.state.lock().unwrap();
            if state.busy {
                return Err(HubError::Busy);
            }
            state.busy = true;
            if self.tx.send(Command::Prompt(prompt, tx)).is_err() {
                state.busy = false;
                return Err(HubError::Closed);
            }
        }
        rx.await.unwrap_or(Err(HubError::Closed))
    }
    pub async fn respond(&self, id: String, allow: bool) -> std::result::Result<(), HubError> {
        let (tx, rx) = oneshot::channel();
        {
            let mut state = self.state.lock().unwrap();
            let Some(index) = state.pending.iter().position(|pending| pending == &id) else {
                return Err(HubError::NotFound);
            };
            state.pending.remove(index);
            if self
                .tx
                .send(Command::Respond(id.clone(), allow, tx))
                .is_err()
            {
                state.pending.push(id);
                return Err(HubError::Closed);
            }
        }
        rx.await.unwrap_or(Err(HubError::Closed))
    }
    pub async fn shutdown(&self) {
        // Interrupts a turn that is blocked in `next_events`, which cannot
        // observe the command channel; the session is discarded anyway.
        let _ = self.stop.send(true);
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Command::Shutdown(tx)).is_ok() {
            let _ = rx.await;
        }
    }
}

pub fn spawn<A, F, Fut>(
    hub: Hub,
    agent: AgentKind,
    yes: bool,
    factory: F,
) -> (Actor, JoinHandle<()>)
where
    A: Agent + 'static,
    F: Fn() -> Fut + 'static,
    Fut: Future<Output = Result<A>> + 'static,
{
    let (tx, rx) = mpsc::unbounded_channel();
    let (stop_tx, stop_rx) = watch::channel(false);
    let state = Arc::new(Mutex::new(AgentState {
        enabled: true,
        ..Default::default()
    }));
    let actor = Actor {
        tx,
        stop: Arc::new(stop_tx),
        state: state.clone(),
    };
    let task = tokio::task::spawn_local(run_actor(hub, agent, yes, factory, state, rx, stop_rx));
    (actor, task)
}

async fn close<A: Agent>(session: &mut Option<A>) {
    if let Some(session) = session.take() {
        if let Err(error) = session.shutdown().await {
            eprintln!("agent shutdown: {error:#}");
        }
    }
}

async fn run_actor<A, F, Fut>(
    hub: Hub,
    agent: AgentKind,
    yes: bool,
    factory: F,
    state: Arc<Mutex<AgentState>>,
    mut rx: mpsc::UnboundedReceiver<Command>,
    mut stop: watch::Receiver<bool>,
) where
    A: Agent,
    F: Fn() -> Fut,
    Fut: Future<Output = Result<A>>,
{
    let mut session = None;
    let mut session_id = String::new();
    while let Some(command) = rx.recv().await {
        match command {
            Command::Prompt(prompt, reply) => {
                if session.is_none() {
                    session_id = Uuid::new_v4().to_string();
                }
                hub.publish(
                    agent.clone(),
                    &session_id,
                    Event::Message {
                        role: Role::User,
                        text: prompt.clone(),
                        delta: false,
                    },
                );
                let _ = reply.send(Ok(()));
                let result = async {
                    if session.is_none() {
                        session = Some(factory().await?);
                    }
                    let active = session.as_mut().expect("session was created");
                    active.send(&prompt).await?;
                    loop {
                        let events = tokio::select! {
                            biased;
                            _ = stop.wait_for(|stopped| *stopped) => return Ok(true),
                            events = active.next_events() => events?,
                        };
                        for event in events {
                            let done = matches!(event, Event::Done { .. });
                            let permission = match &event {
                                Event::PermissionRequest { id, .. } => Some(id.clone()),
                                _ => None,
                            };
                            if let Some(id) = &permission {
                                if !yes {
                                    state.lock().unwrap().pending.push(id.clone());
                                }
                            }
                            hub.publish(agent.clone(), &session_id, event);
                            if let Some(id) = permission {
                                let (allow, response) = if yes {
                                    (true, None)
                                } else {
                                    loop {
                                        match rx.recv().await {
                                            Some(Command::Respond(request_id, allow, reply))
                                                if request_id == id =>
                                            {
                                                break (allow, Some(reply));
                                            }
                                            Some(Command::Respond(_, _, reply)) => {
                                                let _ = reply.send(Err(HubError::NotFound));
                                            }
                                            Some(Command::Prompt(_, reply)) => {
                                                let _ = reply.send(Err(HubError::Busy));
                                            }
                                            Some(Command::Shutdown(reply)) => {
                                                let _ = reply.send(Ok(()));
                                                return Ok(true);
                                            }
                                            None => return Ok(true),
                                        }
                                    }
                                };
                                match active.respond(&id, allow).await {
                                    Ok(()) => {
                                        hub.publish(
                                            agent.clone(),
                                            &session_id,
                                            Event::PermissionResolved { id, allow },
                                        );
                                        if let Some(reply) = response {
                                            let _ = reply.send(Ok(()));
                                        }
                                    }
                                    Err(error) => {
                                        if let Some(reply) = response {
                                            let _ = reply.send(Err(HubError::Closed));
                                        }
                                        return Err(error);
                                    }
                                }
                            }
                            if done {
                                return Ok(false);
                            }
                        }
                        while let Ok(command) = rx.try_recv() {
                            match command {
                                Command::Shutdown(reply) => {
                                    let _ = reply.send(Ok(()));
                                    return Ok(true);
                                }
                                Command::Prompt(_, reply) => {
                                    let _ = reply.send(Err(HubError::Busy));
                                }
                                Command::Respond(_, _, reply) => {
                                    let _ = reply.send(Err(HubError::NotFound));
                                }
                            }
                        }
                    }
                }
                .await;
                state.lock().unwrap().pending.clear();
                match result {
                    Ok(true) => {
                        close(&mut session).await;
                        state.lock().unwrap().busy = false;
                        return;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        eprintln!("agent error: {error:#}");
                        hub.publish(
                            agent.clone(),
                            &session_id,
                            Event::Error {
                                message: format!("{error:#}"),
                            },
                        );
                        hub.publish(
                            agent.clone(),
                            &session_id,
                            Event::Done {
                                reason: "error".into(),
                            },
                        );
                        close(&mut session).await;
                    }
                }
                state.lock().unwrap().busy = false;
            }
            Command::Respond(_, _, reply) => {
                let _ = reply.send(Err(HubError::NotFound));
            }
            Command::Shutdown(reply) => {
                close(&mut session).await;
                let _ = reply.send(Ok(()));
                return;
            }
        }
    }
    close(&mut session).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use tokio::task::LocalSet;

    struct Fake {
        steps: VecDeque<anyhow::Result<Vec<Event>>>,
        answers: Arc<Mutex<Vec<bool>>>,
    }
    impl Agent for Fake {
        async fn send(&mut self, _: &str) -> Result<()> {
            Ok(())
        }
        async fn next_events(&mut self) -> Result<Vec<Event>> {
            self.steps.pop_front().unwrap_or_else(|| {
                Ok(vec![Event::Done {
                    reason: "ok".into(),
                }])
            })
        }
        async fn respond(&mut self, _: &str, allow: bool) -> Result<()> {
            self.answers.lock().unwrap().push(allow);
            Ok(())
        }
        async fn shutdown(self) -> Result<()> {
            Ok(())
        }
    }
    fn permission() -> Event {
        Event::PermissionRequest {
            id: "p".into(),
            tool: "shell".into(),
            input: serde_json::json!({}),
        }
    }
    async fn wait_event(
        rx: &mut broadcast::Receiver<Envelope>,
        predicate: impl Fn(&Event) -> bool,
    ) -> Event {
        loop {
            let envelope = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
            if predicate(&envelope.event) {
                return envelope.event;
            }
        }
    }
    #[test]
    fn snapshot_and_live_order() {
        let hub = Hub::new();
        hub.publish(
            AgentKind::Claude,
            "a",
            Event::Done {
                reason: "first".into(),
            },
        );
        let (snapshot, mut rx) = hub.subscribe();
        hub.publish(
            AgentKind::Codex,
            "b",
            Event::Done {
                reason: "second".into(),
            },
        );
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].seq, 1);
        assert_eq!(rx.try_recv().unwrap().seq, 2);
        for _ in 0..5000 {
            hub.publish(AgentKind::Claude, "a", Event::Done { reason: "x".into() });
        }
        let (history, _) = hub.subscribe();
        assert_eq!(history.len(), 5000);
        assert_eq!(history[0].seq, 3);
        assert_eq!(history.last().unwrap().seq, 5002);
    }
    struct Hangs(Arc<AtomicUsize>);
    impl Agent for Hangs {
        async fn send(&mut self, _: &str) -> Result<()> {
            Ok(())
        }
        async fn next_events(&mut self) -> Result<Vec<Event>> {
            std::future::pending().await
        }
        async fn respond(&mut self, _: &str, _: bool) -> Result<()> {
            Ok(())
        }
        async fn shutdown(self) -> Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    #[tokio::test]
    async fn shutdown_interrupts_a_silent_turn() {
        LocalSet::new()
            .run_until(async {
                let closed = Arc::new(AtomicUsize::new(0));
                let copy = closed.clone();
                let (actor, task) = spawn(Hub::new(), AgentKind::Claude, false, move || {
                    let copy = copy.clone();
                    async move { Ok(Hangs(copy)) }
                });
                actor.prompt("hi".into()).await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                tokio::time::timeout(std::time::Duration::from_secs(2), actor.shutdown())
                    .await
                    .expect("shutdown must not hang");
                task.await.unwrap();
                assert_eq!(closed.load(Ordering::SeqCst), 1);
            })
            .await;
    }
    #[tokio::test]
    async fn permission_busy_and_auto() {
        LocalSet::new()
            .run_until(async {
                for (yes, allow) in [(false, true), (false, false), (true, true)] {
                    let hub = Hub::new();
                    let (_, mut rx) = hub.subscribe();
                    let answers = Arc::new(Mutex::new(Vec::new()));
                    let copy = answers.clone();
                    let (actor, task) = spawn(hub, AgentKind::Claude, yes, move || {
                        let answers = copy.clone();
                        async move {
                            Ok(Fake {
                                steps: VecDeque::from([
                                    Ok(vec![permission()]),
                                    Ok(vec![Event::Done {
                                        reason: "ok".into(),
                                    }]),
                                ]),
                                answers,
                            })
                        }
                    });
                    actor.prompt("hello".into()).await.unwrap();
                    wait_event(&mut rx, |event| {
                        matches!(event, Event::PermissionRequest { .. })
                    })
                    .await;
                    if !yes {
                        assert_eq!(actor.state().pending, vec!["p"]);
                        assert_eq!(actor.prompt("busy".into()).await, Err(HubError::Busy));
                        assert_eq!(
                            actor.respond("wrong".into(), true).await,
                            Err(HubError::NotFound)
                        );
                        actor.respond("p".into(), allow).await.unwrap();
                    }
                    assert_eq!(
                        wait_event(&mut rx, |event| matches!(
                            event,
                            Event::PermissionResolved { .. }
                        ))
                        .await,
                        Event::PermissionResolved {
                            id: "p".into(),
                            allow
                        }
                    );
                    wait_event(&mut rx, |event| matches!(event, Event::Done { .. })).await;
                    assert_eq!(*answers.lock().unwrap(), vec![allow]);
                    actor.shutdown().await;
                    task.await.unwrap();
                }
            })
            .await;
    }
    #[tokio::test]
    async fn error_resets_session() {
        LocalSet::new()
            .run_until(async {
                let hub = Hub::new();
                let (_, mut rx) = hub.subscribe();
                let starts = Arc::new(AtomicUsize::new(0));
                let count = starts.clone();
                let (actor, task) = spawn(hub, AgentKind::Codex, false, move || {
                    let first = count.fetch_add(1, Ordering::SeqCst) == 0;
                    async move {
                        Ok(Fake {
                            steps: VecDeque::from([if first {
                                Err(anyhow::anyhow!("failed"))
                            } else {
                                Ok(vec![Event::Done {
                                    reason: "ok".into(),
                                }])
                            }]),
                            answers: Arc::new(Mutex::new(Vec::new())),
                        })
                    }
                });
                actor.prompt("one".into()).await.unwrap();
                wait_event(&mut rx, |e| matches!(e, Event::Error { .. })).await;
                wait_event(
                    &mut rx,
                    |e| matches!(e, Event::Done {reason} if reason == "error"),
                )
                .await;
                while actor.state().busy {
                    tokio::task::yield_now().await;
                }
                actor.prompt("two".into()).await.unwrap();
                wait_event(
                    &mut rx,
                    |e| matches!(e, Event::Done {reason} if reason == "ok"),
                )
                .await;
                assert_eq!(starts.load(Ordering::SeqCst), 2);
                actor.shutdown().await;
                task.await.unwrap();
            })
            .await;
    }
}
