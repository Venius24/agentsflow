use crate::hub::{Actor, Hub};
use agentsflow_events::{AgentId, Event, Role, WorkflowStatus};
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
    #[serde(skip)]
    bindings: HashMap<String, String>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub id: String,
    pub agent: String,
    pub prompt: String,
    #[serde(default)]
    pub mode: StepMode,
    #[serde(default)]
    pub cc: Vec<String>,
    pub subject: Option<String>,
    pub timeout_secs: Option<u64>,
    pub until: Option<String>,
    pub otherwise: Option<String>,
    #[serde(default = "default_loops")]
    pub max_loops: usize,
    pub next: Option<String>,
}
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StepMode {
    #[default]
    Prompt,
    Mail,
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
fn valid_role(role: &str) -> bool {
    !role.is_empty()
        && role.len() <= 24
        && role
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
}

fn resolve_address(team: &crate::team::Team, step: &str, address: &str) -> Result<AgentId> {
    if let Some(role) = address.strip_prefix('@') {
        let member = if role == "lead" {
            team.lead()
        } else {
            team.by_role(role)
        };
        return member.map(|member| member.id.clone()).with_context(|| {
            let mut roles = vec!["lead".to_string()];
            for member in &team.members {
                for role in &member.roles {
                    if !roles.contains(role) {
                        roles.push(role.clone());
                    }
                }
            }
            format!(
                "step {step}: no team member has role {role} (roles: {})",
                roles.join(", ")
            )
        });
    }
    let id = AgentId::from(address);
    if team.get(&id).is_none() {
        bail!(
            "step {step}: agent {address} is not in the team (members: {})",
            team.ids()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(id)
}

fn roster(team: &crate::team::Team) -> String {
    team.members
        .iter()
        .map(|member| {
            let kind = match member.kind {
                agentsflow_events::AgentKind::Claude => "claude",
                agentsflow_events::AgentKind::Codex => "codex",
            };
            format!(
                "- {} ({kind}, {} {}): {}; roles: {}{}",
                member.id,
                member.model.as_deref().unwrap_or("default"),
                member.effort.as_deref().unwrap_or("default"),
                member.title,
                member.roles.join(", "),
                if member.lead { "; lead" } else { "" }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}
impl Spec {
    pub fn resolve_team(&self, team: &crate::team::Team) -> Result<Self> {
        let mut resolved = self.clone();
        resolved.bindings.insert("team".into(), roster(team));
        for step in &mut resolved.step {
            let agent = resolve_address(team, &step.id, &step.agent)?;
            let mut recipients = Vec::new();
            for address in &step.cc {
                if address == "@others" {
                    for id in team.ids() {
                        if id != &agent && !recipients.contains(id) {
                            recipients.push(id.clone());
                        }
                    }
                } else {
                    let id = resolve_address(team, &step.id, address)?;
                    if id == agent || recipients.contains(&id) {
                        bail!("step {}: duplicate recipient {}", step.id, id);
                    }
                    recipients.push(id);
                }
            }
            for key in placeholders(&step.prompt)? {
                if key.starts_with('@') {
                    let id = resolve_address(team, &step.id, key)?;
                    resolved.bindings.insert(key.to_owned(), id.to_string());
                }
            }
            step.agent = agent.to_string();
            step.cc = recipients.into_iter().map(|id| id.to_string()).collect();
        }
        Ok(resolved)
    }
    pub fn parse(text: &str) -> Result<Self> {
        let raw: toml::Value = toml::from_str(text).context("invalid workflow TOML")?;
        let spec: Self = raw.clone().try_into().context("invalid workflow TOML")?;
        if spec.name.trim().is_empty() {
            bail!("workflow name is empty");
        }
        if spec.step.is_empty() {
            bail!("workflow has no steps");
        }
        let mut ids = HashSet::new();
        for (index, step) in spec.step.iter().enumerate() {
            if step.id.trim().is_empty() || !ids.insert(step.id.as_str()) {
                bail!("step id is empty or duplicated: {:?}", step.id);
            }
            if !(crate::team::valid_id(&step.agent)
                || step
                    .agent
                    .strip_prefix('@')
                    .is_some_and(|role| role != "others" && valid_role(role)))
            {
                bail!("invalid agent in step {}: {}", step.id, step.agent);
            }
            if step.otherwise.is_some() && step.until.is_none() {
                bail!("otherwise requires until in step {}", step.id);
            }
            let cc_present = raw
                .get("step")
                .and_then(toml::Value::as_array)
                .and_then(|steps| steps.get(index))
                .and_then(toml::Value::as_table)
                .is_some_and(|table| table.contains_key("cc"));
            if cc_present && step.mode != StepMode::Mail {
                bail!("cc requires mode='mail' in step {}", step.id);
            }
            for recipient in &step.cc {
                if !(crate::team::valid_id(recipient)
                    || recipient.strip_prefix('@').is_some_and(valid_role))
                {
                    bail!("invalid cc recipient {recipient:?} in step {}", step.id);
                }
            }
            match step.mode {
                StepMode::Prompt if step.subject.is_some() || step.timeout_secs.is_some() => {
                    bail!(
                        "subject and timeout_secs require mode='mail' in step {}",
                        step.id
                    );
                }
                StepMode::Mail => {
                    if !matches!(step.timeout_secs.unwrap_or(900), 1..=7200) {
                        bail!("timeout_secs must be 1..=7200 in step {}", step.id);
                    }
                    let subject = step.subject.as_deref().unwrap_or(&step.id);
                    if subject.is_empty() || subject.chars().count() > 200 {
                        bail!(
                            "mail subject must contain 1..=200 characters in step {}",
                            step.id
                        );
                    }
                }
                _ => {}
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
                    && key != "team"
                    && key != "prev.output"
                    && !key
                        .strip_prefix('@')
                        .is_some_and(|role| role != "others" && valid_role(role))
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
    agents: Vec<&'a str>,
}
#[derive(Serialize)]
struct StepSummary<'a> {
    id: &'a str,
    agent: &'a str,
    mode: StepMode,
}
impl Spec {
    pub fn summary(&self) -> Summary<'_> {
        Summary {
            name: &self.name,
            description: &self.description,
            agents: {
                let mut agents = Vec::new();
                for step in &self.step {
                    for id in std::iter::once(&step.agent).chain(&step.cc) {
                        if !agents.contains(&id.as_str()) {
                            agents.push(id.as_str());
                        }
                    }
                    for role in placeholders(&step.prompt).expect("validated prompt") {
                        if role.starts_with('@') && !agents.contains(&role) {
                            agents.push(role);
                        }
                    }
                }
                agents
            },
            steps: self
                .step
                .iter()
                .map(|s| StepSummary {
                    id: &s.id,
                    agent: &s.agent,
                    mode: s.mode,
                })
                .collect(),
        }
    }
}

fn kind(name: &str) -> AgentId {
    AgentId::from(name)
}
fn render(
    template: &str,
    task: &str,
    outputs: &HashMap<String, (String, AgentId)>,
    previous: Option<&(String, AgentId)>,
    bindings: &HashMap<String, String>,
) -> Result<(String, Option<AgentId>)> {
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
        } else if let Some(value) = bindings.get(key) {
            text.push_str(value);
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
    admission: Arc<Mutex<bool>>,
    shared: Arc<Mutex<Shared>>,
    tx: mpsc::UnboundedSender<Job>,
    team: crate::team::Team,
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
#[derive(Debug, PartialEq, Eq)]
pub enum StartError {
    Busy,
    Closed,
    InvalidTeam(String),
}

impl Manager {
    pub fn new(hub: Hub, agents: crate::runtime::Agents, team: crate::team::Team) -> Self {
        let admission = hub.admission();
        let (tx, mut rx) = mpsc::unbounded_channel::<Job>();
        let shared = Arc::new(Mutex::new(Shared {
            state: WorkflowState::default(),
            stop: None,
        }));
        let state = shared.clone();
        tokio::task::spawn_local(async move {
            while let Some(job) = rx.recv().await {
                let _ = execute(&hub, &agents, &job.spec, &job.task, &job.id, job.stop).await;
                let mut lock = state.lock().unwrap();
                lock.state = WorkflowState::default();
                lock.stop = None;
            }
        });
        Self {
            shared,
            tx,
            team,
            admission,
        }
    }
    pub fn state(&self) -> WorkflowState {
        self.shared.lock().unwrap().state.clone()
    }
    pub fn start(&self, spec: Spec, task: String) -> std::result::Result<String, StartError> {
        let admission = self.admission.lock().unwrap();
        if *admission {
            return Err(StartError::Busy);
        }
        let spec = spec
            .resolve_team(&self.team)
            .map_err(|error| StartError::InvalidTeam(error.to_string()))?;
        let mut shared = self.shared.lock().unwrap();
        if shared.state.running {
            return Err(StartError::Busy);
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
            return Err(StartError::Closed);
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
        AgentId::orchestrator(),
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
    agents: &'a crate::runtime::Agents,
    spec: &'a Spec,
    task: &'a str,
    id: &'a str,
}

struct StepOutcome {
    output: String,
    agent: AgentId,
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
    agent: &AgentId,
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
                if reason == "interrupted" {
                    bail!("interrupted");
                }
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

async fn run_mail_step(
    context: &RunContext<'_>,
    step: &Step,
    agent: AgentId,
    prompt: String,
    stop: &mut watch::Receiver<bool>,
) -> Result<StepOutcome> {
    if *stop.borrow() {
        bail!("stopped");
    }
    let (_, mut events) = context.hub.subscribe();
    let sent = context
        .hub
        .send_mail(
            AgentId::orchestrator(),
            agent,
            step.subject.as_deref().unwrap_or(&step.id).to_owned(),
            prompt.clone(),
            None,
        )
        .map_err(|error| anyhow::anyhow!("mail send failed: {error}"))?;
    for recipient in &step.cc {
        context
            .hub
            .send_mail(
                AgentId::orchestrator(),
                kind(recipient),
                step.subject.as_deref().unwrap_or(&step.id).to_owned(),
                prompt.clone(),
                Some(sent.thread.clone()),
            )
            .map_err(|error| anyhow::anyhow!("mail send to cc {recipient} failed: {error}"))?;
    }
    let timeout_secs = step.timeout_secs.unwrap_or(900);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        if *stop.borrow() {
            bail!("stopped");
        }
        if let Some(reply) = context
            .hub
            .take_thread_for(AgentId::orchestrator(), &sent.thread)
        {
            return Ok(StepOutcome {
                output: reply.body,
                agent: reply.from,
            });
        }
        tokio::select! {
            biased;
            _ = stop.wait_for(|stopped| *stopped) => bail!("stopped"),
            _ = tokio::time::sleep_until(deadline) => {
                bail!("mail timeout: no reply in thread {} within {}s", sent.thread, timeout_secs);
            }
            event = events.recv() => match event {
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(error) => bail!("event stream: {error}"),
            }
        }
    }
}

async fn run_step(
    context: &RunContext<'_>,
    step: &Step,
    outputs: &HashMap<String, (String, AgentId)>,
    previous: Option<&(String, AgentId)>,
    stop: &mut watch::Receiver<bool>,
) -> Result<StepOutcome> {
    let agent = kind(&step.agent);
    let (prompt, from) = render(
        &step.prompt,
        context.task,
        outputs,
        previous,
        &context.spec.bindings,
    )?;
    if step.mode == StepMode::Prompt
        && let Some(from) = from
    {
        context.hub.publish(
            AgentId::orchestrator(),
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
    if step.mode == StepMode::Mail {
        return run_mail_step(context, step, agent, prompt, stop).await;
    }
    let actor = context
        .agents
        .get(&agent)
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
    if context
        .spec
        .step
        .iter()
        .any(|step| step.mode == StepMode::Mail)
        && !context.hub.mail_enabled()
    {
        bail!("mail steps require `agentsflow serve`");
    }
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
    agents: &crate::runtime::Agents,
    spec: &Spec,
    task: &str,
    id: &str,
    mut stop: watch::Receiver<bool>,
) -> bool {
    publish(hub, id, spec, None, WorkflowStatus::Started, "");
    let context = RunContext {
        hub,
        agents,
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
        Spec::parse(include_str!("../../../workflows/mail-review.toml"))?,
        Spec::parse(include_str!("../../../workflows/mail-team.toml"))?,
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
            "name='x'\n[[step]]\nid='a'\nagent=''\nprompt='x'",
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
        assert_eq!(specs.len(), 4);
        assert_eq!(specs[0].name, "implement-and-review");
        assert_eq!(specs[1].name, "cross-check");
        assert_eq!(specs[2].name, "mail-review");
        assert_eq!(specs[3].name, "mail-team");
    }

    #[test]
    fn mail_spec_validation() {
        let prefix = "name='x'\n[[step]]\nid='work'\nagent='codex'\nprompt='{task}'\n";
        let default = Spec::parse(prefix).unwrap();
        assert_eq!(default.step[0].mode, StepMode::Prompt);
        let mail = Spec::parse(&format!(
            "{prefix}mode='mail'\nsubject='review'\ntimeout_secs=7\n"
        ))
        .unwrap();
        assert_eq!(mail.step[0].mode, StepMode::Mail);
        assert_eq!(mail.step[0].subject.as_deref(), Some("review"));
        assert_eq!(mail.step[0].timeout_secs, Some(7));
        let defaults = Spec::parse(&format!("{prefix}mode='mail'\n")).unwrap();
        assert_eq!(
            defaults.step[0]
                .subject
                .as_deref()
                .unwrap_or(&defaults.step[0].id),
            "work"
        );
        assert_eq!(defaults.step[0].timeout_secs.unwrap_or(900), 900);
        assert!(Spec::parse(&format!("{prefix}mode='mail'\ntimeout_secs=7200\n")).is_ok());
        for suffix in [
            "mode='unknown'",
            "subject='x'",
            "timeout_secs=1",
            "mode='mail'\ntimeout_secs=0",
            "mode='mail'\ntimeout_secs=7201",
            "mode='mail'\nsubject=''",
            "mode='mail'\nextra='x'",
        ] {
            assert!(
                Spec::parse(&format!("{prefix}{suffix}\n")).is_err(),
                "{suffix}"
            );
        }
    }

    #[test]
    fn validates_team_members_and_agent_format() {
        let team = crate::team::Team::preset("trio").unwrap();
        let spec = Spec::parse("name='x'\n[[step]]\nid='one'\nagent='main'\nprompt='x'\n[[step]]\nid='two'\nagent='junior'\nmode='mail'\ncc=['missing']\nprompt='x'").unwrap();
        assert_eq!(
            spec.resolve_team(&team).unwrap_err().to_string(),
            "step two: agent missing is not in the team (members: lead, main, junior)"
        );
        let spec = Spec::parse("name='x'\n[[step]]\nid='one'\nagent='other'\nprompt='x'").unwrap();
        assert_eq!(
            spec.resolve_team(&team).unwrap_err().to_string(),
            "step one: agent other is not in the team (members: lead, main, junior)"
        );
        for id in ["@others", "UPPER", "a b", "", "abcdefghijklmnopqrstuvwxyz"] {
            let text = format!("name='x'\n[[step]]\nid='s'\nagent='{id}'\nprompt='x'");
            assert!(Spec::parse(&text).is_err(), "{id}");
        }
    }
    #[test]
    fn named_agents_are_valid_workflow_targets() {
        let spec = Spec::parse("name='team'\n[[step]]\nid='work'\nagent='main'\nmode='mail'\ncc=['junior']\nprompt='x'").unwrap();
        assert_eq!(spec.step[0].agent, "main");
        assert_eq!(spec.step[0].cc, ["junior"]);
    }

    #[test]
    fn roles_others_and_bindings() {
        let spec = Spec::parse("name='x'\n[[step]]\nid='work'\nagent='@lead'\nmode='mail'\ncc=['@others']\nprompt='{team} {@lead} {@engineer}'").unwrap();
        for (preset, lead, others, engineer) in [
            ("pair", "claude", vec!["codex"], "codex"),
            ("trio", "lead", vec!["main", "junior"], "main"),
        ] {
            let team = crate::team::Team::preset(preset).unwrap();
            let resolved = spec.resolve_team(&team).unwrap();
            assert_eq!(resolved.step[0].agent, lead);
            assert_eq!(resolved.step[0].cc, others);
            let (text, from) = render(
                &resolved.step[0].prompt,
                "",
                &HashMap::new(),
                None,
                &resolved.bindings,
            )
            .unwrap();
            assert!(text.contains(&format!("- {lead} (claude,")));
            assert!(text.contains(&format!("{lead} {engineer}")));
            assert!(text.contains("; lead"));
            assert!(from.is_none());
        }
        let trio = crate::team::Team::preset("trio").unwrap();
        let spec = Spec::parse("name='x'\n[[step]]\nid='work'\nagent='@lead'\nmode='mail'\ncc=['main','@others']\nprompt='x'").unwrap();
        assert_eq!(
            spec.resolve_team(&trio).unwrap().step[0].cc,
            ["main", "junior"]
        );
    }

    #[test]
    fn role_errors_and_builtins_for_each_team() {
        for team_name in ["pair", "trio"] {
            let team = crate::team::Team::preset(team_name).unwrap();
            for spec in builtins().unwrap() {
                spec.resolve_team(&team).unwrap();
            }
        }
        let team = crate::team::Team::preset("trio").unwrap();
        for spec in [
            "name='x'\n[[step]]\nid='s'\nagent='@missing'\nprompt='x'",
            "name='x'\n[[step]]\nid='s'\nagent='main'\nprompt='{@missing}'",
        ] {
            assert!(
                Spec::parse(spec)
                    .unwrap()
                    .resolve_team(&team)
                    .unwrap_err()
                    .to_string()
                    .contains("no team member has role missing")
            );
        }
        for cc in [
            "['main']",
            "['@engineer']",
            "['junior','junior']",
            "['@others','junior']",
        ] {
            let spec = format!(
                "name='x'\n[[step]]\nid='s'\nagent='main'\nmode='mail'\ncc={cc}\nprompt='x'"
            );
            assert!(
                Spec::parse(&spec)
                    .unwrap()
                    .resolve_team(&team)
                    .unwrap_err()
                    .to_string()
                    .contains("duplicate recipient")
            );
        }
        assert!(Spec::parse("name='x'\n[[step]]\nid='s'\nagent='@others'\nprompt='x'").is_err());
        assert!(
            Spec::parse("name='x'\n[[step]]\nid='s'\nagent='main'\nprompt='{@others}'").is_err()
        );
        let spec = builtins().unwrap().remove(0).resolve_team(&team).unwrap();
        assert_eq!(
            spec.step
                .iter()
                .map(|s| s.agent.as_str())
                .collect::<Vec<_>>(),
            ["main", "lead", "main"]
        );
    }

    #[tokio::test]
    async fn trio_mail_team_delivers_to_all_in_one_thread() {
        let hub = Hub::new();
        hub.set_mail_enabled(true);
        let (_, mut events) = hub.subscribe();
        let sender = hub.clone();
        let reply = tokio::spawn(async move {
            let mut messages = Vec::new();
            while messages.len() < 3 {
                if let Event::Mail {
                    from,
                    to,
                    thread,
                    body,
                    ..
                } = events.recv().await.unwrap().event
                    && from == AgentId::orchestrator()
                {
                    messages.push((to, thread, body));
                }
            }
            assert_eq!(
                messages.iter().map(|m| m.0.as_str()).collect::<Vec<_>>(),
                ["lead", "main", "junior"]
            );
            assert!(
                messages
                    .iter()
                    .all(|m| m.1 == messages[0].1 && m.2 == messages[0].2)
            );
            assert!(messages[0].2.contains("- junior (codex,"));
            sender
                .send_mail(
                    AgentId::from("lead"),
                    AgentId::orchestrator(),
                    "reply".into(),
                    "VERDICT: APPROVED".into(),
                    Some(messages[0].1.clone()),
                )
                .unwrap();
        });
        let spec = builtins()
            .unwrap()
            .remove(3)
            .resolve_team(&crate::team::Team::preset("trio").unwrap())
            .unwrap();
        let (_tx, stop) = watch::channel(false);
        assert!(execute(&hub, &Default::default(), &spec, "task", "run", stop).await);
        reply.await.unwrap();
    }

    #[test]
    fn cc_validation() {
        let prefix = "name='x'\n[[step]]\nid='work'\nagent='codex'\nprompt='{task}'\n";
        assert!(Spec::parse(&format!("{prefix}mode='mail'\ncc=['claude']\n")).is_ok());
        for suffix in ["cc=['claude']", "cc=[]", "mode='mail'\ncc=['']"] {
            assert!(
                Spec::parse(&format!("{prefix}{suffix}\n")).is_err(),
                "{suffix}"
            );
        }
    }

    #[tokio::test]
    async fn cc_delivers_same_mail_and_reply_completes() {
        let hub = Hub::new();
        hub.set_mail_enabled(true);
        let (_, mut events) = hub.subscribe();
        let sender = hub.clone();
        let reply = tokio::spawn(async move {
            let mut received = Vec::new();
            while received.len() < 2 {
                if let Event::Mail {
                    from,
                    to,
                    thread,
                    subject,
                    body,
                    ..
                } = events.recv().await.unwrap().event
                    && from == AgentId::orchestrator()
                {
                    received.push((to, thread, subject, body));
                }
            }
            assert_eq!(received[0].0, AgentId::from("claude"));
            assert_eq!(received[1].0, AgentId::from("codex"));
            assert_eq!(received[0].1, received[1].1);
            assert_eq!(received[0].2, received[1].2);
            assert_eq!(received[0].3, received[1].3);
            sender
                .send_mail(
                    AgentId::from("codex"),
                    AgentId::orchestrator(),
                    "reply".into(),
                    "VERDICT: APPROVED".into(),
                    Some(received[0].1.clone()),
                )
                .unwrap();
        });
        let (_tx, stop) = watch::channel(false);
        let spec = builtins()
            .unwrap()
            .remove(3)
            .resolve_team(&crate::team::Team::preset("pair").unwrap())
            .unwrap();
        assert!(execute(&hub, &Default::default(), &spec, "do work", "run", stop).await);
        reply.await.unwrap();
        let codex = hub.unread(AgentId::from("codex"));
        let claude = hub.unread(AgentId::from("claude"));
        assert_eq!(codex.len(), 1);
        assert_eq!(claude.len(), 1);
        assert_eq!(codex[0].thread, claude[0].thread);
        assert_eq!(codex[0].body, claude[0].body);
        assert_eq!(codex[0].subject, claude[0].subject);
    }

    #[tokio::test]
    async fn cc_limit_fails_with_reason() {
        let hub = Hub::new();
        hub.set_mail_enabled(true);
        hub.set_mail_thread_limit(1);
        let (_tx, stop) = watch::channel(false);
        let spec = builtins()
            .unwrap()
            .remove(3)
            .resolve_team(&crate::team::Team::preset("pair").unwrap())
            .unwrap();
        assert!(!execute(&hub, &Default::default(), &spec, "work", "run", stop).await);
        assert_eq!(hub.unread(AgentId::from("claude")).len(), 1);
        assert!(hub.unread(AgentId::from("codex")).is_empty());
        assert!(
            hub.subscribe()
                .0
                .iter()
                .any(|envelope| matches!(&envelope.event,
                    Event::Workflow { status: WorkflowStatus::Failed, detail, .. }
                    if detail.contains("cc codex") && detail.contains("thread limit")
                ))
        );
    }

    const MAIL_SPEC: &str = "name='mail'\n[[step]]\nid='work'\nagent='codex'\nmode='mail'\nprompt='{task}'\nuntil='VERDICT: APPROVED'\ntimeout_secs=1\nnext='end'\n";

    async fn mail_reply_case(body: &'static str, expected: bool) {
        let hub = Hub::new();
        hub.set_mail_enabled(true);
        let (_, mut events) = hub.subscribe();
        let sender = hub.clone();
        let reply = tokio::spawn(async move {
            loop {
                if let Event::Mail {
                    from,
                    to,
                    thread,
                    subject,
                    ..
                } = events.recv().await.unwrap().event
                    && from == AgentId::orchestrator()
                    && to == AgentId::from("codex")
                {
                    assert_eq!(subject, "work");
                    sender
                        .send_mail(
                            AgentId::from("claude"),
                            AgentId::orchestrator(),
                            "other".into(),
                            "unrelated".into(),
                            None,
                        )
                        .unwrap();
                    sender
                        .send_mail(
                            AgentId::from("codex"),
                            AgentId::orchestrator(),
                            "reply".into(),
                            body.into(),
                            Some(thread),
                        )
                        .unwrap();
                    break;
                }
            }
        });
        let (_tx, stop) = watch::channel(false);
        let spec = Spec::parse(MAIL_SPEC).unwrap();
        assert_eq!(
            execute(&hub, &Default::default(), &spec, "task", "run", stop).await,
            expected
        );
        reply.await.unwrap();
        let unread = hub.unread(AgentId::orchestrator());
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].body, "unrelated");
        let (history, _) = hub.subscribe();
        assert!(
            !history
                .iter()
                .any(|e| matches!(e.event, Event::Handoff { .. }))
        );
        if expected {
            assert!(history.iter().any(|e| matches!(&e.event, Event::Workflow { status:WorkflowStatus::StepDone, detail, .. } if detail == body)));
            assert!(history.iter().any(|e| matches!(
                e.event,
                Event::MailRead {
                    ref agent,
                    ..
                } if *agent == AgentId::orchestrator()
            )));
        } else {
            assert!(history.iter().any(|e| matches!(&e.event, Event::Workflow { status:WorkflowStatus::Failed, detail, .. } if detail.contains("until not satisfied"))));
        }
    }

    #[tokio::test]
    async fn mail_reply_verdict_and_other_thread() {
        mail_reply_case("finished\nVERDICT: APPROVED", true).await;
        mail_reply_case("unable to finish", false).await;
    }

    #[tokio::test]
    async fn mail_timeout() {
        let hub = Hub::new();
        hub.set_mail_enabled(true);
        let (_tx, stop) = watch::channel(false);
        assert!(
            !execute(
                &hub,
                &Default::default(),
                &Spec::parse(MAIL_SPEC).unwrap(),
                "task",
                "run",
                stop
            )
            .await
        );
        let (history, _) = hub.subscribe();
        let thread = history
            .iter()
            .find_map(|event| match &event.event {
                Event::Mail { thread, .. } => Some(thread),
                _ => None,
            })
            .unwrap();
        let expected = format!("mail timeout: no reply in thread {thread} within 1s");
        assert!(history.iter().any(|e| matches!(&e.event, Event::Workflow { status:WorkflowStatus::Failed, detail, .. } if detail == &expected)));
    }

    #[tokio::test]
    async fn mail_stop_while_waiting() {
        let hub = Hub::new();
        hub.set_mail_enabled(true);
        let (_, mut events) = hub.subscribe();
        let (tx, stop) = watch::channel(false);
        let stopper = tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                if matches!(
                    event.event,
                    Event::Mail {
                        ref from,
                        ..
                    } if *from == AgentId::orchestrator()
                ) {
                    tx.send(true).unwrap();
                    break;
                }
            }
        });
        assert!(
            !execute(
                &hub,
                &Default::default(),
                &Spec::parse(MAIL_SPEC).unwrap(),
                "task",
                "run",
                stop
            )
            .await
        );
        stopper.await.unwrap();
        assert!(hub.subscribe().0.iter().any(|e| matches!(&e.event, Event::Workflow { status:WorkflowStatus::Failed, detail, .. } if detail == "stopped")));
    }

    #[tokio::test]
    async fn disabled_mail_fails_before_first_step() {
        let hub = Hub::new();
        let (_tx, stop) = watch::channel(false);
        let spec = Spec::parse("name='mixed'\n[[step]]\nid='first'\nagent='claude'\nprompt='first'\n[[step]]\nid='mail'\nagent='codex'\nmode='mail'\nprompt='second'\n").unwrap();
        assert!(!execute(&hub, &Default::default(), &spec, "task", "run", stop).await);
        let (history, _) = hub.subscribe();
        assert!(history.iter().any(|e| matches!(&e.event, Event::Workflow { status:WorkflowStatus::Failed, detail, .. } if detail == "mail steps require `agentsflow serve`")));
        assert!(!history.iter().any(|e| matches!(
            e.event,
            Event::Workflow {
                status: WorkflowStatus::StepStarted,
                ..
            } | Event::Mail { .. }
        )));
    }

    #[test]
    fn rendering() {
        let outputs = HashMap::from([("a".into(), ("answer".into(), AgentId::from("codex")))]);
        let previous = ("latest".into(), AgentId::from("claude"));
        assert_eq!(
            render(
                "{task}: {a.output} / {prev.output}",
                "hi",
                &outputs,
                Some(&previous),
                &HashMap::new()
            )
            .unwrap(),
            ("hi: answer / latest".into(), Some(AgentId::from("claude")))
        );
        assert!(
            render("{prev.output}", "", &outputs, None, &HashMap::new())
                .unwrap_err()
                .to_string()
                .contains("no previous step")
        );
        assert!(
            render("{b.output}", "", &outputs, None, &HashMap::new())
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
        kind: AgentId,
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

    #[tokio::test]
    async fn interrupted_turn_fails_workflow() {
        LocalSet::new().run_until(async {
            let hub = Hub::new();
            let (actor, task) = crate::hub::spawn(hub.clone(), AgentId::from("main"), true, || async {
                Ok(crate::testing::Silent::new(1, false))
            });
            let agents = HashMap::from([(AgentId::from("main"), actor.clone())]);
            let spec = Spec::parse("name='named'\n[[step]]\nid='work'\nagent='main'\nprompt='{task}'").unwrap();
            let (_tx, stop) = watch::channel(false);
            let interrupter = actor.clone();
            let interrupt_task = tokio::task::spawn_local(async move {
                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    while !interrupter.state().busy { tokio::task::yield_now().await; }
                    interrupter.interrupt().await.unwrap();
                }).await.unwrap();
            });
            assert!(!execute(&hub, &agents, &spec, "hello", "id", stop).await);
            interrupt_task.await.unwrap();
            assert!(hub.subscribe().0.iter().any(|e| matches!(&e.event,
                Event::Workflow { status: WorkflowStatus::Failed, detail, .. } if detail == "interrupted"
            )));
            actor.shutdown().await;
            task.await.unwrap();
        }).await;
    }

    #[tokio::test]
    async fn workflow_uses_named_actor() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let hub = Hub::new();
                let (main, task, prompts) = fake(&hub, AgentId::from("main"), &["done"], false);
                let agents = HashMap::from([(AgentId::from("main"), main.clone())]);
                let spec =
                    Spec::parse("name='named'\n[[step]]\nid='work'\nagent='main'\nprompt='{task}'")
                        .unwrap();
                let (_tx, stop) = watch::channel(false);
                assert!(execute(&hub, &agents, &spec, "hello", "id", stop).await);
                assert_eq!(prompts.lock().unwrap().as_slice(), ["hello"]);
                main.shutdown().await;
                task.await.unwrap();
            })
            .await;
    }

    #[tokio::test]
    async fn implement_review_runs_on_trio_roles() {
        LocalSet::new()
            .run_until(async {
                let hub = Hub::new();
                let (lead, lead_task, lead_prompts) =
                    fake(&hub, AgentId::from("lead"), &["VERDICT: APPROVED"], false);
                let (main, main_task, main_prompts) =
                    fake(&hub, AgentId::from("main"), &["implementation"], false);
                let agents = HashMap::from([
                    (AgentId::from("lead"), lead.clone()),
                    (AgentId::from("main"), main.clone()),
                ]);
                let spec = builtins()
                    .unwrap()
                    .remove(0)
                    .resolve_team(&crate::team::Team::preset("trio").unwrap())
                    .unwrap();
                let (_tx, stop) = watch::channel(false);
                assert!(execute(&hub, &agents, &spec, "task", "run", stop).await);
                assert_eq!(main_prompts.lock().unwrap().as_slice(), ["task"]);
                assert!(lead_prompts.lock().unwrap()[0].contains("implementation"));
                lead.shutdown().await;
                main.shutdown().await;
                lead_task.await.unwrap();
                main_task.await.unwrap();
            })
            .await;
    }

    async fn looping_case(reviews: &[&str], success: bool) {
        let hub = Hub::new();
        let (claude, ctask, prompts) = fake(&hub, AgentId::from("claude"), reviews, false);
        let (codex, dtask, _) = fake(
            &hub,
            AgentId::from("codex"),
            &["implementation", "fixed"],
            false,
        );
        let (_tx, stop) = watch::channel(false);
        let spec = builtins()
            .unwrap()
            .remove(0)
            .resolve_team(&crate::team::Team::preset("pair").unwrap())
            .unwrap();
        assert_eq!(
            execute(
                &hub,
                &HashMap::from([
                    (AgentId::from("claude"), claude.clone()),
                    (AgentId::from("codex"), codex.clone())
                ]),
                &spec,
                "do",
                "id",
                stop
            )
            .await,
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
        assert!(review_handoffs.iter().all(
            |(from, to, _)| **from == AgentId::from("codex") && **to == AgentId::from("claude")
        ));
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
        assert!(!execute(&hub, &Default::default(), &spec, "task", "id", stop).await);
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
        let (claude, ctask, prompts) =
            fake(&hub, AgentId::from("claude"), &["VERDICT: APPROVED"], false);
        let (codex, dtask, fixes) = fake(&hub, AgentId::from("codex"), &["implementation"], false);
        let (_tx, stop) = watch::channel(false);
        let spec = builtins()
            .unwrap()
            .remove(0)
            .resolve_team(&crate::team::Team::preset("pair").unwrap())
            .unwrap();
        assert!(
            execute(
                &hub,
                &HashMap::from([
                    (AgentId::from("claude"), claude.clone()),
                    (AgentId::from("codex"), codex.clone())
                ]),
                &spec,
                "task",
                "id",
                stop
            )
            .await
        );
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
        let (actor, task, _) = fake(&hub, AgentId::from("claude"), &[], true);
        let manager = Manager::new(
            hub.clone(),
            HashMap::from([(AgentId::from("claude"), actor.clone())]),
            crate::team::Team::filtered_pair(true, false).unwrap(),
        );
        manager
            .start(Spec::parse(BASIC).unwrap(), "one".into())
            .unwrap();
        assert_eq!(
            manager.start(Spec::parse(BASIC).unwrap(), "two".into()),
            Err(StartError::Busy)
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
