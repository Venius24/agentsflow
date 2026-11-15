//! The mentor: one strong model that holds the idea of a task. It writes the
//! assignment for the cheaper doers and checks their work in the same session,
//! so the idea is not lost between the owner's words and the result.
//!
//! The owner switches it on and off and picks the model; the choice lives in
//! `.agentsflow/mentor.json` and applies to runs started after the change.

use crate::team::{AccessMode, Member, MemberSandbox, Team};
use crate::workflow::{Spec, Step, StepMode};
use agentsflow_events::{AgentId, AgentKind};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    pub id: &'static str,
    /// Shown to the owner.
    pub title: &'static str,
    pub model: &'static str,
    pub effort: &'static str,
}

pub const OFF: &str = "off";

pub const PRESETS: [Preset; 3] = [
    Preset {
        id: "fable-medium",
        title: "Fable · medium",
        model: "claude-fable-5-2",
        effort: "medium",
    },
    Preset {
        id: "fable-low",
        title: "Fable · low",
        model: "claude-fable-5-2",
        effort: "low",
    },
    Preset {
        id: "opus-high",
        title: "Opus · high",
        model: "claude-opus-5-5",
        effort: "high",
    },
];

pub const TITLE: &str = "наставник";
const BRIEF_STEP: &str = "mentor_brief";
const REVIEW_STEP: &str = "mentor_review";
const FIX_STEP: &str = "mentor_fix";
const APPROVED: &str = "VERDICT: APPROVED";

pub fn preset(id: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|preset| preset.id == id)
}

fn file(home: &Path) -> PathBuf {
    home.join(".agentsflow").join("mentor.json")
}

/// The owner's choice; `None` when the mentor is off.
pub fn load(home: &Path) -> Option<&'static Preset> {
    let text = std::fs::read_to_string(file(home)).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    preset(value.get("preset")?.as_str()?)
}

/// Saves the choice; `off` switches the mentor off.
pub fn save(home: &Path, id: &str) -> Result<()> {
    if id != OFF && preset(id).is_none() {
        bail!("нет такого наставника: {id}");
    }
    let path = file(home);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::json!({ "preset": id }).to_string())
        .with_context(|| format!("cannot write {}", path.display()))
}

fn has_role(member: &Member, role: &str) -> bool {
    member.roles.iter().any(|r| r == role)
}

fn resolve(team: &Team, address: &str) -> Option<AgentId> {
    match address.strip_prefix('@') {
        Some("lead") => team.lead().map(|m| m.id.clone()),
        Some(role) => team.by_role(role).map(|m| m.id.clone()),
        None => Some(AgentId::from(address)),
    }
}

fn step(id: &str, agent: &str, prompt: String, max_loops: usize) -> Step {
    Step {
        id: id.into(),
        agent: agent.into(),
        prompt,
        mode: StepMode::Prompt,
        cc: Vec::new(),
        subject: None,
        timeout_secs: None,
        until: None,
        otherwise: None,
        max_loops,
        next: None,
    }
}

/// Puts the mentor into one run, before member ids become run ids.
///
/// The department's reviewer becomes the mentor (its model and effort are
/// replaced); a department without a pure reviewer gets one more agent. Where
/// the workflow lacks them, the mentor's assignment goes before the first
/// step and its review after the last.
pub fn apply(team: &mut Team, spec: &mut Spec, preset: &Preset, max_loops: usize) {
    let existing = team
        .members
        .iter()
        .position(|m| has_role(m, "reviewer") && !has_role(m, "doer"))
        // `@reviewer` must still mean this agent.
        .filter(|&index| team.by_role("reviewer").map(|m| &m.id) == Some(&team.members[index].id));
    let mentor = match existing {
        Some(index) => {
            let member = &mut team.members[index];
            member.kind = AgentKind::Claude;
            member.command = None;
            member.model = Some(preset.model.into());
            member.effort = Some(preset.effort.into());
            member.title = TITLE.into();
            if member.sandbox.is_none() {
                member.sandbox = Some(MemberSandbox::ReadOnly);
            }
            member.id.clone()
        }
        None => {
            let mut id = "mentor".to_owned();
            while team.get(&AgentId::from(id.as_str())).is_some() {
                id.push('x');
            }
            team.members.push(Member {
                id: AgentId::from(id.as_str()),
                kind: AgentKind::Claude,
                command: None,
                model: Some(preset.model.into()),
                effort: Some(preset.effort.into()),
                mode: Some(AccessMode::Smart),
                sandbox: Some(MemberSandbox::ReadOnly),
                guard: true,
                title: TITLE.into(),
                roles: vec!["mentor".into()],
                lead: false,
                sites: Vec::new(),
                repos: Vec::new(),
                workdir: None,
            });
            AgentId::from(id)
        }
    };
    let mine = |team: &Team, step: &Step| resolve(team, &step.agent).as_ref() == Some(&mentor);
    let Some(first) = spec.step.iter().position(|s| !mine(team, s)) else {
        return;
    };
    let doer = spec.step[first].agent.clone();
    let reviews = spec.step.iter().any(|s| mine(team, s) && s.until.is_some());
    if !reviews {
        for step in &mut spec.step {
            if step.next.as_deref() == Some("end") {
                step.next = Some(REVIEW_STEP.into());
            }
        }
        let mut review = step(
            REVIEW_STEP,
            mentor.as_str(),
            format!(
                "Результат исполнителя:\n{{prev.output}}\n\n\
                 Ты наставник и проверяешь работу по своей постановке и по замыслу хозяина. \
                 Файлы не создавай и не меняй, замечания пиши в ответе. Посмотри result.md и \
                 то, что исполнитель изменил, проверки запусти сам. Возвращай только за ошибки \
                 по существу: замысел не выполнен, неверные факты или логика, не сделана часть \
                 задачи, нарушено правило отдела. Мелочи перечисли коротко. На повторной \
                 проверке смотри, исправлено ли сказанное раньше, и не добавляй новых \
                 требований без новой ошибки по существу. Ошибок по существу нет — последней \
                 строкой напиши ровно {APPROVED}. Иначе объясни исполнителю, что исправить и \
                 почему: он слабее тебя и замысла целиком не видит."
            ),
            max_loops,
        );
        review.until = Some(APPROVED.into());
        review.otherwise = Some(FIX_STEP.into());
        review.next = Some("end".into());
        let mut fix = step(
            FIX_STEP,
            &doer,
            format!(
                "Замечания наставника:\n{{{REVIEW_STEP}.output}}\n\nИсправь и обнови result.md."
            ),
            max_loops,
        );
        fix.next = Some(REVIEW_STEP.into());
        spec.step.push(review);
        spec.step.push(fix);
    }
    if first == 0 {
        spec.step[0].prompt.push_str(&format!(
            "\n\nПостановка наставника (он держит замысел задачи и потом проверит результат; \
             расходится с текстом выше — верь постановке):\n{{{BRIEF_STEP}.output}}"
        ));
        spec.step.insert(
            0,
            step(
                BRIEF_STEP,
                mentor.as_str(),
                "{brief}\n\n\
                 Ты наставник. Сам задачу не делай и файлы не меняй: делает исполнитель, модель \
                 слабее тебя, он увидит задачу выше и твой ответ. Твоё дело — понять замысел \
                 хозяина и передать его так, чтобы исполнитель не угадывал.\n\
                 1. Разберись: прочитай правила, память и нужные файлы репозитория, проверь \
                 факты, на которые опирается задача.\n\
                 2. Напиши постановку: чего хозяин хочет на самом деле и зачем, что именно \
                 сделать и где, чего не трогать, где легко ошибиться, по чему ты будешь \
                 проверять результат. Коротко и конкретно, без работы целиком.\n\
                 3. Без ответа хозяина придётся угадывать — перечисли вопросы и скажи \
                 исполнителю записать их в questions.md и остановиться."
                    .to_owned(),
                max_loops,
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::department::{Department, RunKind, tests::SAMPLE};

    const SOLO: &str = r#"
id = "shop"
title = "Магазин"
description = "Остатки"
[[agent]]
id = "doer"
kind = "claude"
model = "claude-sonnet-5-5"
roles = ["doer"]
[[step]]
id = "do"
agent = "@doer"
prompt = "{brief}"
next = "end"
"#;

    #[test]
    fn the_reviewer_becomes_the_mentor_and_briefs_the_doer_first() {
        let dept = Department::parse(SAMPLE, Path::new("pr")).unwrap();
        let fable = preset("fable-low").unwrap();
        let plan = dept
            .run_plan_with("t1", RunKind::Full, Some(fable))
            .unwrap();
        let mentor = plan.team.get(&AgentId::from("pr:critic:t1")).unwrap();
        assert_eq!(mentor.kind, AgentKind::Claude);
        assert_eq!(mentor.model.as_deref(), Some("claude-fable-5-2"));
        assert_eq!(mentor.effort.as_deref(), Some("low"));
        assert_eq!(mentor.sandbox, Some(MemberSandbox::ReadOnly));
        assert_eq!(plan.team.members.len(), 2);
        let ids: Vec<_> = plan.spec.step.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["mentor_brief", "do", "review", "fix"]);
        assert_eq!(plan.spec.step[0].agent, "pr:critic:t1");
        assert!(plan.spec.step[1].prompt.contains("{mentor_brief.output}"));
        // Off: the roster is as written.
        let plain = dept.run_plan("t1", RunKind::Full).unwrap();
        assert_eq!(plain.team.members[1].kind, AgentKind::Codex);
        assert_eq!(plain.spec.step.len(), 3);
    }

    #[test]
    fn a_solo_department_gets_a_mentor_with_brief_and_review() {
        let dept = Department::parse(SOLO, Path::new("shop")).unwrap();
        let opus = preset("opus-high").unwrap();
        let plan = dept.run_plan_with("t1", RunKind::Full, Some(opus)).unwrap();
        let mentor = plan.team.get(&AgentId::from("shop:mentor:t1")).unwrap();
        assert_eq!(mentor.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(mentor.effort.as_deref(), Some("high"));
        assert_eq!(mentor.sandbox, Some(MemberSandbox::ReadOnly));
        assert!(!mentor.lead);
        let steps = &plan.spec.step;
        let ids: Vec<_> = steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["mentor_brief", "do", "mentor_review", "mentor_fix"]);
        assert_eq!(steps[1].next.as_deref(), Some("mentor_review"));
        assert_eq!(steps[2].agent, "shop:mentor:t1");
        assert_eq!(steps[2].otherwise.as_deref(), Some("mentor_fix"));
        assert_eq!(steps[3].agent, "shop:doer:t1");
        assert_eq!(steps[3].next.as_deref(), Some("mentor_review"));
    }

    #[test]
    fn a_workflow_that_already_starts_with_the_reviewer_keeps_its_steps() {
        let lead = SAMPLE.replace(
            "[[step]]\nid = \"do\"",
            "[[step]]\nid = \"plan\"\nagent = \"@reviewer\"\nprompt = \"{brief}\"\n\n[[step]]\nid = \"do\"",
        );
        let dept = Department::parse(&lead, Path::new("pr")).unwrap();
        let plan = dept
            .run_plan_with("t1", RunKind::Full, preset("fable-medium"))
            .unwrap();
        let ids: Vec<_> = plan.spec.step.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["plan", "do", "review", "fix"]);
        assert_eq!(
            plan.team.members[1].model.as_deref(),
            Some("claude-fable-5-2")
        );
    }

    #[test]
    fn the_choice_is_saved_and_off_clears_it() {
        let dir = std::env::temp_dir().join(format!("agentsflow-mentor-{}", uuid::Uuid::new_v4()));
        assert!(load(&dir).is_none());
        save(&dir, "opus-high").unwrap();
        assert_eq!(load(&dir).unwrap().id, "opus-high");
        save(&dir, OFF).unwrap();
        assert!(load(&dir).is_none());
        assert!(save(&dir, "gpt").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
