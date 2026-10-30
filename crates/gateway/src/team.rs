use agentsflow_events::{AgentId, AgentKind};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, path::Path};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Member {
    pub id: AgentId,
    pub kind: AgentKind,
    pub model: Option<String>,
    pub effort: Option<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default)]
    pub lead: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Team {
    #[serde(rename = "agent")]
    pub members: Vec<Member>,
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 24
        && id != "orchestrator"
        && id != "user"
        && id
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
}

impl Team {
    pub fn get(&self, id: &AgentId) -> Option<&Member> {
        self.members.iter().find(|m| &m.id == id)
    }
    pub fn lead(&self) -> Option<&Member> {
        self.members.iter().find(|m| m.lead)
    }
    #[allow(dead_code)]
    pub fn by_role(&self, tag: &str) -> Option<&Member> {
        self.members
            .iter()
            .find(|m| m.roles.iter().any(|r| r == tag))
    }
    #[allow(dead_code)]
    pub fn others(&self, than: &AgentId) -> impl Iterator<Item = &Member> {
        self.members.iter().filter(move |m| &m.id != than)
    }
    pub fn ids(&self) -> impl Iterator<Item = &AgentId> {
        self.members.iter().map(|m| &m.id)
    }
    pub fn filtered_pair(claude: bool, codex: bool) -> Result<Self> {
        let mut team = Self::preset("pair")?;
        team.members.retain(|member| match member.kind {
            AgentKind::Claude => claude,
            AgentKind::Codex => codex,
        });
        if !team.members.iter().any(|member| member.lead) {
            if let Some(first) = team.members.first_mut() {
                first.lead = true;
            }
        }
        team.validate()?;
        Ok(team)
    }
    pub fn validate(&self) -> Result<()> {
        if !(1..=6).contains(&self.members.len()) {
            bail!("team must contain 1..=6 agents");
        }
        let mut ids = HashSet::new();
        for member in &self.members {
            if !valid_id(member.id.as_str()) {
                bail!("agent {:?}: invalid id", member.id.as_str());
            }
            if !ids.insert(&member.id) {
                bail!("agent {}: duplicate id", member.id);
            }
            if let Some(effort) = &member.effort {
                let valid = match member.kind {
                    AgentKind::Claude => {
                        matches!(effort.as_str(), "low" | "medium" | "high" | "xhigh" | "max")
                    }
                    AgentKind::Codex => {
                        matches!(effort.as_str(), "low" | "medium" | "high" | "xhigh")
                    }
                };
                if !valid {
                    bail!("agent {}: invalid effort {effort}", member.id);
                }
            }
        }
        if self.members.iter().filter(|m| m.lead).count() != 1 {
            bail!("team must have exactly one agent with lead = true");
        }
        Ok(())
    }
    pub fn from_toml(text: &str) -> Result<Self> {
        let team: Self = toml::from_str(text).context("invalid team TOML")?;
        team.validate()?;
        Ok(team)
    }
    pub fn preset(name: &str) -> Result<Self> {
        match name {
            "trio" => Self::from_toml(TEMPLATE),
            "pair" => Self::from_toml(PAIR),
            _ => bail!("unknown team preset: {name}"),
        }
    }
    pub fn load(spec: &str) -> Result<Self> {
        match spec {
            "pair" | "trio" => Self::preset(spec),
            _ => Self::from_toml(
                &std::fs::read_to_string(Path::new(spec))
                    .with_context(|| format!("cannot read team file {spec}"))?,
            ),
        }
    }
}

pub fn format_compact(team: &Team) -> String {
    let members = team
        .members
        .iter()
        .map(|member| {
            let kind = match member.kind {
                AgentKind::Claude => "claude",
                AgentKind::Codex => "codex",
            };
            let mut details = kind.to_string();
            if let Some(model) = &member.model {
                details.push(' ');
                details.push_str(model);
            }
            if let Some(effort) = &member.effort {
                details.push(' ');
                details.push_str(effort);
            }
            if member.lead {
                details.push_str(", lead");
            }
            format!("{}({details})", member.id)
        })
        .collect::<Vec<_>>()
        .join(" ");
    format!("team: {members}")
}

pub fn run_cli(args: &[String]) -> Result<()> {
    match args {
        [command] if command == "show" => {
            for name in ["pair", "trio"] {
                println!("{name}:");
                print_team(&Team::preset(name)?);
            }
        }
        [command, spec] if command == "check" => {
            print_team(&Team::load(spec)?);
            println!("OK");
        }
        [command] if command == "init" => init(Path::new("team.toml"))?,
        [command, file] if command == "init" => init(Path::new(file))?,
        _ => bail!("Usage: agentsflow team init [FILE] | team check <pair|trio|FILE> | team show"),
    }
    Ok(())
}

fn print_team(team: &Team) {
    println!("id\tkind\tmodel\teffort\troles\tlead");
    for id in team.ids() {
        let member = team.get(id).expect("listed member exists");
        println!(
            "{}\t{:?}\t{}\t{}\t{}\t{}",
            id,
            member.kind,
            member.model.as_deref().unwrap_or("-"),
            member.effort.as_deref().unwrap_or("-"),
            member.roles.join(","),
            team.lead().is_some_and(|lead| lead.id == *id)
        );
    }
}

fn init(path: &Path) -> Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| {
            format!(
                "cannot create {}; choose a new path (existing files are not overwritten)",
                path.display()
            )
        })?;
    file.write_all(TEMPLATE.as_bytes())?;
    println!(
        "{}\nзапустите: agentsflow serve --team {}",
        path.display(),
        path.display()
    );
    Ok(())
}

const PAIR: &str = r#"
[[agent]]
id = "claude"
kind = "claude"
title = "ревью и ядро"
roles = ["lead", "reviewer"]
lead = true

[[agent]]
id = "codex"
kind = "codex"
title = "инженер"
roles = ["engineer"]
"#;

pub const TEMPLATE: &str = r#"# У каждого участника отдельный процесс и почтовый ящик.
# id: 1–24 символа из a-z, 0-9, _ и -, кроме user и orchestrator.
[[agent]]
id = "lead"
kind = "claude" # claude или codex
model = "claude-sonnet-5-5" # необязательно
# Claude: low, medium, high, xhigh, max; Codex: low, medium, high, xhigh.
effort = "high"
title = "лидер, ревью, ядро"
roles = ["lead", "reviewer", "core"] # теги для адресации в workflow
lead = true # ровно один лидер в команде

[[agent]]
id = "main"
kind = "codex"
model = "gpt-6.1-terra"
effort = "medium"
title = "главный инженер"
roles = ["engineer"]

[[agent]]
id = "junior"
kind = "codex"
model = "gpt-6.1-terra"
effort = "low"
title = "младший инженер"
roles = ["junior", "engineer"]

# Чтобы добавить участника, добавьте ещё блок [[agent]] с уникальным id.
# В workflow используйте теги ролей для адресации участников.
"#;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn presets_and_roles() {
        let team = Team::preset("trio").unwrap();
        assert_eq!(team, Team::from_toml(TEMPLATE).unwrap());
        assert_eq!(team.lead().unwrap().id.as_str(), "lead");
        assert_eq!(team.by_role("engineer").unwrap().id.as_str(), "main");
        assert_eq!(team.others(&AgentId::from("lead")).count(), 2);
        assert_eq!(Team::preset("pair").unwrap().ids().count(), 2);
    }
    #[test]
    fn compact_team_format() {
        assert_eq!(
            format_compact(&Team::preset("pair").unwrap()),
            "team: claude(claude, lead) codex(codex)"
        );
        assert_eq!(
            format_compact(&Team::preset("trio").unwrap()),
            "team: lead(claude claude-sonnet-5-5 high, lead) main(codex gpt-6.1-terra medium) junior(codex gpt-6.1-terra low)"
        );
    }
    #[test]
    fn check_valid_and_invalid_files() {
        let path =
            std::env::temp_dir().join(format!("agentsflow-check-{}.toml", uuid::Uuid::new_v4()));
        std::fs::write(&path, TEMPLATE).unwrap();
        let args = vec!["check".into(), path.to_string_lossy().into_owned()];
        assert!(run_cli(&args).is_ok());
        std::fs::write(&path, "[[agent]]\nid='a'\nkind='codex'\n").unwrap();
        assert!(run_cli(&args).unwrap_err().to_string().contains("lead"));
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn filtered_pair_promotes_remaining_member() {
        let team = Team::filtered_pair(false, true).unwrap();
        assert_eq!(team.lead().unwrap().id.as_str(), "codex");
        assert!(Team::filtered_pair(false, false).is_err());
    }
    #[test]
    fn rejects_invalid_teams() {
        let prefix = "[[agent]]\nid='a'\nkind='claude'\nlead=true\n";
        for bad in [
            "",
            "[[agent]]\nid='orchestrator'\nkind='codex'\nlead=true",
            "[[agent]]\nid='A'\nkind='codex'\nlead=true",
            "[[agent]]\nid='a'\nkind='codex'",
            "[[agent]]\nid='a'\nkind='codex'\nlead=true\neffort='max'",
            "[[agent]]\nid='a'\nkind='unknown'\nlead=true",
            "[[agent]]\nid='a'\nkind='codex'\nlead=true\nextra=1",
        ] {
            assert!(Team::from_toml(bad).is_err(), "{bad}");
        }
        assert!(Team::from_toml(&format!("{prefix}\n[[agent]]\nid='a'\nkind='codex'\n")).is_err());
        assert!(
            Team::from_toml(&format!(
                "{prefix}\n[[agent]]\nid='b'\nkind='codex'\nlead=true\n"
            ))
            .is_err()
        );
        assert!(!valid_id(&"a".repeat(25)));
    }

    #[test]
    fn init_does_not_overwrite() {
        let dir = std::env::temp_dir().join(format!("agentsflow-team-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("team.toml");
        init(&path).unwrap();
        let original = std::fs::read(&path).unwrap();
        assert!(init(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }
}
