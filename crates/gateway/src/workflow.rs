use crate::hub::{Actor, Hub};
use agentsflow_events::{AgentKind, Event, Role, WorkflowStatus};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::{Arc, Mutex},
};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default = "default_steps")]
    pub max_steps: usize,
    pub step: Vec<Step>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub id: String,
    pub agent: String,
    pub prompt: String,
    pub until: Option<String>,
    pub otherwise: Option<String>,
    #[serde(default = "default_loops")]
    pub max_loops: usize,
    pub next: Option<String>,
}
fn default_steps() -> usize {
    20
}
fn default_loops() -> usize {
    2
}

fn placeholders(prompt: &str) -> Result<Vec<&str>> {
    let mut result = Vec::new();
    let mut rest = prompt;
    while let Some(start) = rest.find(['{', '}']) {
        if rest.as_bytes()[start] == b'}' {
            bail!("unexpected closing brace");
        }
        rest = &rest[start + 1..];
        let end = rest.find('}').context("unclosed placeholder")?;
        let name = &rest[..end];
        if name.contains('{') || name.is_empty() {
            bail!("invalid placeholder {{{name}}}");
        }
        result.push(name);
        rest = &rest[end + 1..];
    }
    Ok(result)
}
impl Spec {
    pub fn parse(text: &str) -> Result<Self> {
        let spec: Self = toml::from_str(text).context("invalid workflow TOML")?;
        if spec.name.trim().is_empty() {
            bail!("workflow name is empty");
        }
        if spec.step.is_empty() {
            bail!("workflow has no steps");
        }
        let mut ids = HashSet::new();
        for step in &spec.step {
            if step.id.trim().is_empty() || !ids.insert(step.id.as_str()) {
                bail!("step id is empty or duplicated: {:?}", step.id);
            }
            if !matches!(step.agent.as_str(), "claude" | "codex") {
                bail!("invalid agent in step {}: {}", step.id, step.agent);
            }
            if step.otherwise.is_some() && step.until.is_none() {
                bail!("otherwise requires until in step {}", step.id);
            }
        }
        for step in &spec.step {
            for target in [
                step.next.as_deref().filter(|target| *target != "end"),
                step.otherwise.as_deref(),
            ]
            .into_iter()
            .flatten()
            {
                if !ids.contains(target) {
                    bail!("unknown step target {target} in {}", step.id);
                }
            }
            for key in placeholders(&step.prompt)? {
                if key != "task"
                    && key != "prev.output"
                    && !key
                        .strip_suffix(".output")
                        .is_some_and(|id| ids.contains(id))
                {
                    bail!("unknown placeholder {{{key}}} in {}", step.id);
                }
            }
        }
        Ok(spec)
    }
    pub fn load(path: &Path) -> Result<Self> {
        Self::parse(
            &std::fs::read_to_string(path)
                .with_context(|| format!("cannot read {}", path.display()))?,
        )
        .with_context(|| format!("invalid workflow {}", path.display()))
    }
}
#[derive(Serialize)]
pub struct Summary<'a> {
    name: &'a str,
    description: &'a Option<String>,
    steps: Vec<StepSummary<'a>>,
}
#[derive(Serialize)]
struct StepSummary<'a> {
    id: &'a str,
    agent: &'a str,
}
impl Spec {
    pub fn summary(&self) -> Summary<'_> {
        Summary {
            name: &self.name,
            description: &self.description,
            steps: self
                .step
                .iter()
                .map(|s| StepSummary {
                    id: &s.id,
                    agent: &s.agent,
                })
                .collect(),
        }
    }
}

fn kind(name: &str) -> AgentKind {
    if name == "claude" {
        AgentKind::Claude
    } else {
        AgentKind::Codex
    }
}
fn render(
    template: &str,
    task: &str,
    outputs: &HashMap<String, (String, AgentKind)>,
    previous: Option<&(String, AgentKind)>,
) -> Result<(String, Option<AgentKind>)> {
    let mut text = String::new();
    let mut rest = template;
    let mut from = None;
    while let Some(start) = rest.find('{') {
        text.push_str(&rest[..start]);
        rest = &rest[start + 1..];
        let end = rest.find('}').context("unclosed placeholder")?;
        let key = &rest[..end];
        if key == "task" {
            text.push_str(task);
        } else {
            let id = key.strip_suffix(".output").context("invalid placeholder")?;
            let (output, agent) = if id == "prev" {
                previous.context("no previous step has run for {prev.output}")?
            } else {
                outputs
                    .get(id)
                    .with_context(|| format!("step {id} has not run"))?
            };
            text.push_str(output);
            from = Some(agent.clone());
        }
        rest = &rest[end + 1..];
    }
    text.push_str(rest);
    Ok((text, from))
}

#[derive(Clone, Default, Serialize)]
pub struct WorkflowState {
    pub running: bool,
    pub name: Option<String>,
}
#[derive(Clone)]
pub struct Manager {
    shared: Arc<Mutex<Shared>>,
    tx: mpsc::UnboundedSender<Job>,
}
struct Shared {
    state: WorkflowState,
    stop: Option<watch::Sender<bool>>,
}
struct Job {
    spec: Spec,
    task: String,
    id: String,
    stop: watch::Receiver<bool>,
}
impl Manager {
    pub fn new(hub: Hub, claude: Option<Actor>, codex: Option<Actor>) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<Job>();
        let shared = Arc::new(Mutex::new(Shared {
            state: WorkflowState::default(),
            stop: None,
        }));
        let state = shared.clone();
        tokio::task::spawn_local(async move {
            while let Some(job) = rx.recv().await {
                let _ = execute(
                    &hub,
                    claude.as_ref(),
                    codex.as_ref(),
                    &job.spec,
                    &job.task,
                    &job.id,
                    job.stop,
                )
                .await;
                let mut lock = state.lock().unwrap();
                lock.state = WorkflowState::default();
                lock.stop = None;
            }
        });
        Self { shared, tx }
    }
    pub fn state(&self) -> WorkflowState {
        self.shared.lock().unwrap().state.clone()
    }
    pub fn start(&self, spec: Spec, task: String) -> std::result::Result<String, &'static str> {
        let mut shared = self.shared.lock().unwrap();
        if shared.state.running {
            return Err("busy");
        }
        let (stop, rx) = watch::channel(false);
        let id = Uuid::new_v4().to_string();
        shared.state = WorkflowState {
            running: true,
            name: Some(spec.name.clone()),
        };
        shared.stop = Some(stop);
        if self
            .tx
            .send(Job {
                spec,
                task,
                id: id.clone(),
                stop: rx,
            })
            .is_err()
        {
            shared.state = WorkflowState::default();
            shared.stop = None;
            return Err("closed");
        }
        Ok(id)
    }
    pub fn stop(&self) -> bool {
        let shared = self.shared.lock().unwrap();
        if let Some(stop) = &shared.stop {
            let _ = stop.send(true);
            true
        } else {
            false
        }
    }
}

fn publish(
    hub: &Hub,
    id: &str,
    spec: &Spec,
    step: Option<&str>,
    status: WorkflowStatus,
    detail: impl Into<String>,
) {
    hub.publish(
        AgentKind::Orchestrator,
        id,
        Event::Workflow {
            name: spec.name.clone(),
            step: step.map(str::to_owned),
            status,
            detail: detail.into(),
        },
    );
}
struct RunContext<'a> {
    hub: &'a Hub,
    claude: Option<&'a Actor>,
    codex: Option<&'a Actor>,
    spec: &'a Spec,
    task: &'a str,
    id: &'a str,
}

struct StepOutcome {
    output: String,
    agent: AgentKind,
}

enum Next {
    Goto(usize),
    Loop { index: usize, detail: String },
    End,
}

/// A verdict counts only when it is the whole last non-empty line of the
/// output, ignoring surrounding whitespace and markdown emphasis, so
/// "NOT APPROVED" or a verdict followed by remarks does not pass.
fn verdict_matches(output: &str, marker: &str) -> bool {
    let clean = |text: &str| {
        text.trim()
            .trim_matches(|c: char| {
                matches!(c, '*' | '_' | '`' | '~' | '.' | '!') || c.is_whitespace()
            })
            .to_owned()
    };
    let marker = clean(marker);
    !marker.is_empty()
        && output
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .is_some_and(|line| clean(line) == marker)
}

fn next_index(
    spec: &Spec,
    index: usize,
    output: &str,
    loops: &mut HashMap<String, usize>,
) -> Result<Next> {
    let step = &spec.step[index];
    if let Some(until) = &step.until {
        if !verdict_matches(output, until) {
            let used = loops.entry(step.id.clone()).or_default();
            if let Some(target) = &step.otherwise {
                if *used < step.max_loops {
                    *used += 1;
                    let index = spec
                        .step
                        .iter()
                        .position(|s| &s.id == target)
                        .context("workflow target disappeared")?;
                    return Ok(Next::Loop {
                        index,
                        detail: format!("loop {} -> {target}", *used),
                    });
                }
                bail!("until not satisfied after {} loops", *used);
            }
            bail!("until not satisfied");
        }
    }
    match step.next.as_deref() {
        Some("end") => Ok(Next::End),
        Some(target) => Ok(Next::Goto(
            spec.step
                .iter()
                .position(|s| s.id == target)
                .context("workflow target disappeared")?,
        )),
        None if index + 1 == spec.step.len() => Ok(Next::End),
        None => Ok(Next::Goto(index + 1)),
    }
}

async fn wait_turn(
    events: &mut tokio::sync::broadcast::Receiver<agentsflow_events::Envelope>,
    agent: &AgentKind,
    stop: &mut watch::Receiver<bool>,
) -> Result<String> {
    let mut output = String::new();
    let mut last_error = None;
    loop {
        let envelope = tokio::select! {
            biased;
            _ = stop.wait_for(|stopped| *stopped) => bail!("stopped"),
            event = events.recv() => event.map_err(|error| anyhow::anyhow!("event stream: {error}"))?,
        };
        if &envelope.agent != agent {
            continue;
        }
        match envelope.event {
            Event::Message {
                role: Role::Assistant,
                text,
                ..
            } => output = text,
            Event::Error { message } => last_error = Some(message),
            Event::Done { reason } => {
                if reason == "error" {
                    bail!("{}", last_error.unwrap_or(reason));
                }
                return Ok(output);
            }
            _ => {}
        }
    }
}

async fn wait_idle(actor: &Actor, stop: &mut watch::Receiver<bool>) -> Result<()> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while actor.state().busy {
        tokio::select! {
            biased;
            _ = stop.wait_for(|stopped| *stopped) => bail!("stopped"),
            _ = tokio::time::sleep_until(deadline) => bail!("agent did not become idle"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
        }
    }
    Ok(())
}

async fn run_step(
    context: &RunContext<'_>,
    step: &Step,
    outputs: &HashMap<String, (String, AgentKind)>,
    previous: Option<&(String, AgentKind)>,
    stop: &mut watch::Receiver<bool>,
) -> Result<StepOutcome> {
    let agent = kind(&step.agent);
    let (prompt, from) = render(&step.prompt, context.task, outputs, previous)?;
    if let Some(from) = from {
        context.hub.publish(
            AgentKind::Orchestrator,
            context.id,
            Event::Handoff {
                from,
                to: agent.clone(),
                step: step.id.clone(),
                text: prompt.clone(),
            },
        );
    }
    publish(
        context.hub,
        context.id,
        context.spec,
        Some(&step.id),
        WorkflowStatus::StepStarted,
        "",
    );
    let actor = match agent {
        AgentKind::Claude => context.claude,
        AgentKind::Codex => context.codex,
        AgentKind::Orchestrator => None,
    }
    .with_context(|| format!("agent {} is disabled", step.agent))?;
    let (_, mut events) = context.hub.subscribe();
    tokio::select! {
        biased;
        _ = stop.wait_for(|stopped| *stopped) => bail!("stopped"),
        result = actor.prompt(prompt) => {
            result.map_err(|error| anyhow::anyhow!("agent {}: {error:?}", step.agent))?;
        }
    }
    let output = wait_turn(&mut events, &agent, stop).await?;
    wait_idle(actor, stop).await?;
    Ok(StepOutcome { output, agent })
}

async fn execute_steps(context: &RunContext<'_>, stop: &mut watch::Receiver<bool>) -> Result<()> {
    let mut outputs = HashMap::new();
    let mut previous = None;
    let mut loops = HashMap::new();
    let mut index = 0;
    let mut count = 0;
    loop {
        if *stop.borrow() {
            bail!("stopped");
        }
        if count >= context.spec.max_steps {
            bail!("max_steps exceeded");
        }
        count += 1;
        let step = &context.spec.step[index];
        let outcome = run_step(context, step, &outputs, previous.as_ref(), stop).await?;
        let detail = outcome.output.chars().take(200).collect::<String>();
        previous = Some((outcome.output.clone(), outcome.agent.clone()));
        outputs.insert(step.id.clone(), (outcome.output.clone(), outcome.agent));
        publish(
            context.hub,
            context.id,
            context.spec,
            Some(&step.id),
            WorkflowStatus::StepDone,
            detail,
        );
        match next_index(context.spec, index, &outcome.output, &mut loops)? {
            Next::Goto(next) => index = next,
            Next::Loop {
                index: next,
                detail,
            } => {
                publish(
                    context.hub,
                    context.id,
                    context.spec,
                    Some(&step.id),
                    WorkflowStatus::Looped,
                    detail,
                );
                index = next;
            }
            Next::End => return Ok(()),
        }
    }
}

pub async fn execute(
    hub: &Hub,
    claude: Option<&Actor>,
    codex: Option<&Actor>,
    spec: &Spec,
    task: &str,
    id: &str,
    mut stop: watch::Receiver<bool>,
) -> bool {
    publish(hub, id, spec, None, WorkflowStatus::Started, "");
    let context = RunContext {
        hub,
        claude,
        codex,
        spec,
        task,
        id,
    };
    match execute_steps(&context, &mut stop).await {
        Ok(()) => {
            publish(hub, id, spec, None, WorkflowStatus::Finished, "");
            true
        }
        Err(error) => {
            publish(
                hub,
                id,
                spec,
                None,
                WorkflowStatus::Failed,
                format!("{error:#}"),
            );
            false
        }
    }
}

pub fn builtins() -> Result<Vec<Spec>> {
    Ok(vec![
        Spec::parse(include_str!("../../../workflows/implement-and-review.toml"))?,
        Spec::parse(include_str!("../../../workflows/cross-check.toml"))?,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentsflow_adapters::Agent;
    use std::collections::VecDeque;
    use tokio::task::LocalSet;

    const BASIC: &str = "name='test'\n[[step]]\nid='a'\nagent='claude'\nprompt='{task}'\n";

    #[test]
    fn validate_specs() {
        assert!(Spec::parse(BASIC).is_ok());
        for bad in [
            "name='x'\n[[step]]\nid=''\nagent='claude'\nprompt='x'",
            concat!(
                "name='x'\n[[step]]\nid='a'\nagent='claude'\nprompt='x'\n",
                "[[step]]\nid='a'\nagent='codex'\nprompt='x'"
            ),
            "name='x'\n[[step]]\nid='a'\nagent='bad'\nprompt='x'",
            "name='x'\n[[step]]\nid='a'\nagent='claude'\nprompt='x'\nnext='bad'",
            "name='x'\n[[step]]\nid='a'\nagent='claude'\nprompt='x'\nuntil='ok'\notherwise='bad'",
            "name='x'\n[[step]]\nid='a'\nagent='claude'\nprompt='{missing.output}'",
            "name='x'\n[[step]]\nid='a'\nagent='claude'\nprompt='x'\notherwise='a'",
            "name='x'\nunknown=1\n[[step]]\nid='a'\nagent='claude'\nprompt='x'",
            "name='x'\n[[step]]\nid='a'\nagent='claude'\nprompt='x'\nunknown=1",
        ] {
            assert!(Spec::parse(bad).is_err(), "{bad}");
        }
        assert!(
            Spec::parse(
                "name='x'\n[[step]]\nid='a'\nagent='claude'\nprompt='{prev.output}'\nnext='end'"
            )
            .is_ok()
        );
        let specs = builtins().unwrap();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, "implement-and-review");
        assert_eq!(specs[1].name, "cross-check");
    }

    #[test]
    fn rendering() {
        let outputs = HashMap::from([("a".into(), ("answer".into(), AgentKind::Codex))]);
        let previous = ("latest".into(), AgentKind::Claude);
        assert_eq!(
            render(
                "{task}: {a.output} / {prev.output}",
                "hi",
                &outputs,
                Some(&previous)
            )
            .unwrap(),
            ("hi: answer / latest".into(), Some(AgentKind::Claude))
        );
        assert!(
            render("{prev.output}", "", &outputs, None)
                .unwrap_err()
                .to_string()
                .contains("no previous step")
        );
        assert!(
            render("{b.output}", "", &outputs, None)
                .unwrap_err()
                .to_string()
                .contains("has not run")
        );
    }

    struct Fake {
        replies: Arc<Mutex<VecDeque<String>>>,
        prompts: Arc<Mutex<Vec<String>>>,
        never: bool,
    }
    impl Agent for Fake {
        async fn send(&mut self, prompt: &str) -> Result<()> {
            self.prompts.lock().unwrap().push(prompt.to_owned());
            Ok(())
        }
        async fn next_events(&mut self) -> Result<Vec<Event>> {
            if self.never {
                std::future::pending().await
            } else {
                let text = self.replies.lock().unwrap().pop_front().unwrap_or_default();
                if text == "FAIL" {
                    bail!("broken");
                }
                Ok(vec![
                    Event::Message {
                        role: Role::Assistant,
                        text,
                        delta: false,
                    },
                    Event::Done {
                        reason: "completed".into(),
                    },
                ])
            }
        }
        async fn respond(&mut self, _: &str, _: bool) -> Result<()> {
            Ok(())
        }
        async fn shutdown(self) -> Result<()> {
            Ok(())
        }
    }
    fn fake(
        hub: &Hub,
        kind: AgentKind,
        replies: &[&str],
        never: bool,
    ) -> (Actor, tokio::task::JoinHandle<()>, Arc<Mutex<Vec<String>>>) {
        let replies = Arc::new(Mutex::new(replies.iter().map(|s| s.to_string()).collect()));
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let saved = prompts.clone();
        let (actor, task) = crate::hub::spawn(hub.clone(), kind, true, move || {
            let replies = replies.clone();
            let prompts = prompts.clone();
            async move {
                Ok(Fake {
                    replies,
                    prompts,
                    never,
                })
            }
        });
        (actor, task, saved)
    }

    async fn looping_case(reviews: &[&str], success: bool) {
        let hub = Hub::new();
        let (claude, ctask, prompts) = fake(&hub, AgentKind::Claude, reviews, false);
        let (codex, dtask, _) = fake(&hub, AgentKind::Codex, &["implementation", "fixed"], false);
        let (_tx, stop) = watch::channel(false);
        let spec = builtins().unwrap().remove(0);
        assert_eq!(
            execute(&hub, Some(&claude), Some(&codex), &spec, "do", "id", stop).await,
            success
        );
        let (history, _) = hub.subscribe();
        let review_handoffs: Vec<_> = history
            .iter()
            .filter_map(|e| match &e.event {
                Event::Handoff {
                    from,
                    to,
                    step,
                    text,
                } if step == "review" => Some((from, to, text)),
                _ => None,
            })
            .collect();
        assert!(
            review_handoffs
                .iter()
                .all(|(from, to, _)| **from == AgentKind::Codex && **to == AgentKind::Claude)
        );
        assert!(review_handoffs[0].2.contains("implementation"));
        if reviews.len() > 1 {
            assert!(review_handoffs[1].2.contains("fixed"));
            assert!(prompts.lock().unwrap()[1].contains("fixed"));
        }
        if success {
            assert!(history.iter().any(|e| matches!(
                &e.event,
                Event::Workflow {
                    status: WorkflowStatus::Looped,
                    ..
                }
            )));
            assert_eq!(prompts.lock().unwrap().len(), 2);
        } else {
            assert!(history.iter().any(|e| matches!(
                &e.event,
                Event::Workflow {
                    status: WorkflowStatus::Failed,
                    ..
                }
            )));
        }
        claude.shutdown().await;
        codex.shutdown().await;
        ctask.await.unwrap();
        dtask.await.unwrap();
    }

    #[test]
    fn verdict_must_be_the_last_line() {
        let ok = "VERDICT: APPROVED";
        for output in [
            "VERDICT: APPROVED",
            "  VERDICT: APPROVED  \n\n",
            "All good.\nVERDICT: APPROVED",
            "**VERDICT: APPROVED**",
            "`VERDICT: APPROVED`.",
        ] {
            assert!(verdict_matches(output, ok), "{output:?}");
        }
        for output in [
            "",
            "   \n",
            "NOT VERDICT: APPROVED",
            "Not APPROVED: no error handling",
            "Almost VERDICT: APPROVED, but fix the tests",
            "VERDICT: APPROVED\nhowever the tests fail",
            "VERDICT: APPROVED?",
            "APPROVED",
        ] {
            assert!(!verdict_matches(output, ok), "{output:?}");
        }
        assert!(!verdict_matches("anything", ""));
    }

    #[tokio::test]
    async fn looping_handoff_and_failures() {
        let local = LocalSet::new();
        local
            .run_until(looping_case(&["needs changes", "VERDICT: APPROVED"], true))
            .await;
        local
            .run_until(looping_case(&["bad", "bad", "bad"], false))
            .await;
        local.run_until(looping_case(&["FAIL"], false)).await;
    }

    async fn previous_missing_case() {
        let hub = Hub::new();
        let spec =
            Spec::parse("name='x'\n[[step]]\nid='a'\nagent='claude'\nprompt='{prev.output}'")
                .unwrap();
        let (_tx, stop) = watch::channel(false);
        assert!(!execute(&hub, None, None, &spec, "task", "id", stop).await);
        let (history, _) = hub.subscribe();
        assert!(history.iter().any(|event| matches!(
            &event.event,
            Event::Workflow { status: WorkflowStatus::Failed, detail, .. }
                if detail.contains("no previous step")
        )));
    }

    #[tokio::test]
    async fn previous_missing_fails_workflow() {
        LocalSet::new().run_until(previous_missing_case()).await;
    }

    async fn success_ends_case() {
        let hub = Hub::new();
        let (claude, ctask, prompts) = fake(&hub, AgentKind::Claude, &["VERDICT: APPROVED"], false);
        let (codex, dtask, fixes) = fake(&hub, AgentKind::Codex, &["implementation"], false);
        let (_tx, stop) = watch::channel(false);
        let spec = builtins().unwrap().remove(0);
        assert!(execute(&hub, Some(&claude), Some(&codex), &spec, "task", "id", stop).await);
        assert_eq!(prompts.lock().unwrap().len(), 1);
        assert_eq!(fixes.lock().unwrap().len(), 1);
        claude.shutdown().await;
        codex.shutdown().await;
        ctask.await.unwrap();
        dtask.await.unwrap();
    }

    #[tokio::test]
    async fn successful_review_does_not_run_fix() {
        LocalSet::new().run_until(success_ends_case()).await;
    }

    async fn busy_stop_case() {
        let hub = Hub::new();
        let (actor, task, _) = fake(&hub, AgentKind::Claude, &[], true);
        let manager = Manager::new(hub.clone(), Some(actor.clone()), None);
        manager
            .start(Spec::parse(BASIC).unwrap(), "one".into())
            .unwrap();
        assert_eq!(
            manager.start(Spec::parse(BASIC).unwrap(), "two".into()),
            Err("busy")
        );
        while !actor.state().busy {
            tokio::task::yield_now().await;
        }
        assert!(manager.stop());
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while manager.state().running {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let (history, _) = hub.subscribe();
        assert!(history.iter().any(|event| matches!(
            &event.event,
            Event::Workflow { status: WorkflowStatus::Failed, detail, .. }
                if detail == "stopped"
        )));
        actor.shutdown().await;
        task.await.unwrap();
    }

    #[tokio::test]
    async fn manager_busy_and_stop() {
        LocalSet::new().run_until(busy_stop_case()).await;
    }
}
