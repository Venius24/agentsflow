//! Memory and rules. Agents leave `memory.md` and `rule.md` in the run
//! folder; after the run the gateway appends the notes to the department
//! memory and turns the rule into a proposal. A rule reaches `AGENTS.md` only
//! with the owner's approval, as its own commit, so it can be reverted.

use crate::home::{self, Home};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Notes an agent leaves for the next runs of the department.
pub const NOTE_FILE: &str = "memory.md";
/// A rule an agent proposes for the department.
pub const RULE_FILE: &str = "rule.md";
/// The orchestrator's own rules are a target like a department.
pub const ORCHESTRATOR: &str = "orchestrator";
const REJECTED: &str = "rejected.md";

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Proposal {
    pub target: String,
    pub id: String,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RuleCommit {
    pub target: String,
    pub commit: String,
    pub subject: String,
    pub date: String,
}

/// Where the rules of a target live.
fn target_dir(home: &Home, target: &str) -> Result<PathBuf> {
    if target == ORCHESTRATOR {
        return Ok(home.orchestrator_dir());
    }
    home.departments
        .get(target)
        .map(|dept| dept.dir.clone())
        .with_context(|| format!("нет отдела {target}"))
}

fn proposals_dir(dir: &Path) -> PathBuf {
    dir.join("memory").join("proposals")
}

/// Today's date in the home's time zone (minutes east of UTC).
pub fn today(offset_minutes: i32) -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default();
    date_of(secs + i64::from(offset_minutes) * 60)
}

fn date_of(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    // Civil date from days since the epoch (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Agents often start their note with their own "## date · task" heading;
/// the gateway writes that heading itself, so a leading heading is dropped.
fn strip_heading(notes: &str) -> &str {
    let mut rest = notes;
    while let Some(line) = rest.lines().next() {
        if !line.trim_start().starts_with('#') {
            break;
        }
        rest = rest[line.len()..].trim_start();
    }
    if rest.is_empty() { notes } else { rest }
}

fn take(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let _ = std::fs::remove_file(path);
    let text = text.trim().to_owned();
    (!text.is_empty()).then_some(text)
}

fn first_line(text: &str, limit: usize) -> String {
    let line = text
        .lines()
        .map(|line| line.trim().trim_start_matches(['#', '-', '*', ' ']))
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let mut out: String = line.chars().take(limit).collect();
    if line.chars().count() > limit {
        out.push('…');
    }
    out
}

/// Takes what the agents left in the run folder. Returns whether notes were
/// added and the new rule proposal, if any.
pub fn harvest(
    dept_id: &str,
    dept_dir: &Path,
    task: &str,
    run_dir: &Path,
    offset_minutes: i32,
) -> Result<(bool, Option<Proposal>)> {
    let mut noted = false;
    // Agents sometimes write the note next to the department's rules
    // (`../../memory.md`) instead of the run folder: nobody reads it there.
    let notes = [run_dir.join(NOTE_FILE), dept_dir.join(NOTE_FILE)]
        .iter()
        .filter_map(|path| take(path))
        .map(|text| strip_heading(&text).to_owned())
        .collect::<Vec<_>>()
        .join("\n");
    if !notes.is_empty() {
        let memory = dept_dir.join("memory");
        std::fs::create_dir_all(&memory)?;
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(memory.join("notes.md"))?;
        let date = today(offset_minutes);
        write!(file, "\n## {date} · задача {task}\n\n{notes}\n")?;
        noted = true;
    }
    let proposal = match take(&run_dir.join(RULE_FILE)) {
        Some(text) => Some(write_proposal(dept_id, dept_dir, task, &text)?),
        None => None,
    };
    Ok((noted, proposal))
}

/// A fact an agent of a department with repositories wants in the owner's
/// own memory of the project: kept as a proposal with the `share-` prefix,
/// it reaches that memory only on the owner's approval.
pub fn harvest_share(dept_id: &str, dept_dir: &Path, task: &str, run_dir: &Path) -> Result<Option<Proposal>> {
    match take(&run_dir.join(crate::projectmem::SHARE_FILE)) {
        Some(text) => {
            let base = format!("{}{task}", crate::projectmem::SHARE_PREFIX);
            Ok(Some(write_proposal(dept_id, dept_dir, &base, &text)?))
        }
        None => Ok(None),
    }
}

/// Whether a proposal is a fact for the owner's memory, not a rule.
pub fn is_share(id: &str) -> bool {
    id.starts_with(crate::projectmem::SHARE_PREFIX)
}

fn write_proposal(target: &str, dir: &Path, base: &str, text: &str) -> Result<Proposal> {
    let proposals = proposals_dir(dir);
    std::fs::create_dir_all(&proposals)?;
    let mut id = base.to_owned();
    let mut n = 1;
    while proposals.join(format!("{id}.md")).exists() {
        n += 1;
        id = format!("{base}-{n}");
    }
    std::fs::write(proposals.join(format!("{id}.md")), text)?;
    Ok(Proposal {
        target: target.to_owned(),
        id,
        text: text.to_owned(),
    })
}

/// A rule proposed outside a run (by the orchestrator).
pub fn propose(home: &Home, target: &str, text: &str) -> Result<Proposal> {
    let text = text.trim();
    if text.is_empty() {
        bail!("пустое правило");
    }
    let dir = target_dir(home, target)?;
    let base = uuid::Uuid::new_v4().simple().to_string()[..8].to_owned();
    write_proposal(target, &dir, &base, text)
}

fn valid_proposal_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 24
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn proposal_path(home: &Home, target: &str, id: &str) -> Result<PathBuf> {
    if !valid_proposal_id(id) {
        bail!("недопустимый id предложения");
    }
    let path = proposals_dir(&target_dir(home, target)?).join(format!("{id}.md"));
    if !path.is_file() {
        bail!("предложения уже нет");
    }
    Ok(path)
}

/// Every open proposal, orchestrator first.
pub fn proposals(home: &Home) -> Vec<Proposal> {
    let targets = std::iter::once(ORCHESTRATOR.to_owned()).chain(home.departments.keys().cloned());
    let mut out = Vec::new();
    for target in targets {
        let Ok(dir) = target_dir(home, &target) else {
            continue;
        };
        let Ok(entries) = std::fs::read_dir(proposals_dir(&dir)) else {
            continue;
        };
        let mut found: Vec<_> = entries
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                let id = path.file_stem()?.to_str()?.to_owned();
                (path.extension()? == "md").then_some(())?;
                let text = std::fs::read_to_string(&path).ok()?;
                Some(Proposal {
                    target: target.clone(),
                    id,
                    text,
                })
            })
            .collect();
        found.sort_by(|a, b| a.id.cmp(&b.id));
        out.extend(found);
    }
    out
}

/// Adds the rule to `AGENTS.md` as its own commit. Returns the commit.
pub fn approve(home: &Home, target: &str, id: &str) -> Result<RuleCommit> {
    let path = proposal_path(home, target, id)?;
    let text = std::fs::read_to_string(&path)?;
    if is_share(id) {
        let repo = home
            .departments
            .get(target)
            .and_then(|dept| dept.repos.first())
            .with_context(|| format!("у отдела {target} нет репозитория"))?;
        let file = crate::projectmem::share(repo, id, &text)?;
        std::fs::remove_file(&path)?;
        return Ok(RuleCommit {
            target: target.to_owned(),
            commit: String::new(),
            subject: format!("в память проекта: {}", file.display()),
            date: today(home.config.offset_minutes()),
        });
    }
    let dir = target_dir(home, target)?;
    let rules = dir.join(crate::department::RULES);
    let mut current = std::fs::read_to_string(&rules).unwrap_or_default();
    if !current.ends_with('\n') && !current.is_empty() {
        current.push('\n');
    }
    let date = today(home.config.offset_minutes());
    current.push_str(&format!("\n## Правило от {date}\n\n{}\n", text.trim()));
    std::fs::write(&rules, current)?;
    let subject = format!("rule({target}): {}", first_line(&text, 60));
    if let Err(error) = home::commit(&home.root, &[&rules], &subject) {
        crate::server_diag!("rule commit failed: {error:#}");
    }
    std::fs::remove_file(&path)?;
    Ok(RuleCommit {
        target: target.to_owned(),
        commit: head(&home.root).unwrap_or_default(),
        subject,
        date: today(home.config.offset_minutes()),
    })
}

/// Drops the proposal and remembers it, so agents do not propose it again.
pub fn reject(home: &Home, target: &str, id: &str) -> Result<()> {
    let path = proposal_path(home, target, id)?;
    let text = std::fs::read_to_string(&path)?;
    let memory = target_dir(home, target)?.join("memory");
    std::fs::create_dir_all(&memory)?;
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(memory.join(REJECTED))?;
    writeln!(file, "- {}: {}", today(home.config.offset_minutes()), first_line(&text, 300))?;
    std::fs::remove_file(path)?;
    Ok(())
}

/// The tail of rejected rules, for the brief.
pub fn rejected(dir: &Path, limit: usize) -> String {
    let text = std::fs::read_to_string(dir.join("memory").join(REJECTED)).unwrap_or_default();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let mut out = Vec::new();
    let mut size = 0;
    for line in lines.iter().rev() {
        size += line.len();
        if size > limit {
            break;
        }
        out.push(*line);
    }
    out.reverse();
    out.join("\n")
}

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "user.name=agentsflow",
            "-c",
            "user.email=agentsflow@localhost",
        ])
        .args(args)
        .output()
        .context("git is not available")?;
    if !output.status.success() {
        bail!(
            "git {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn head(root: &Path) -> Result<String> {
    Ok(git(root, &["rev-parse", "--short", "HEAD"])?
        .trim()
        .to_owned())
}

/// The latest rule commits that are not reverted yet, newest first.
pub fn recent(home: &Home, limit: usize) -> Vec<RuleCommit> {
    let Ok(log) = git(
        &home.root,
        &["log", "-n", "200", "--format=%h%x09%s%x09%cs%x09%b%x1e"],
    ) else {
        return Vec::new();
    };
    let mut reverted = std::collections::HashSet::new();
    let mut out = Vec::new();
    for record in log.split('\u{1e}') {
        let mut fields = record.trim().splitn(4, '\t');
        let (Some(commit), Some(subject), Some(date)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let body = fields.next().unwrap_or("");
        if subject.starts_with("Revert \"rule(") {
            // git revert notes the reverted commit in the body.
            if let Some(hash) = body
                .split("This reverts commit ")
                .nth(1)
                .map(|rest| rest.trim_end_matches(['.', '\n', ' ']))
            {
                reverted.insert(hash.chars().take(7).collect::<String>());
            }
            continue;
        }
        let Some(rest) = subject.strip_prefix("rule(") else {
            continue;
        };
        let Some((target, _)) = rest.split_once("):") else {
            continue;
        };
        if reverted
            .iter()
            .any(|hash| commit.starts_with(hash.as_str()) || hash.starts_with(commit))
        {
            continue;
        }
        out.push(RuleCommit {
            target: target.to_owned(),
            commit: commit.to_owned(),
            subject: subject.to_owned(),
            date: date.to_owned(),
        });
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// Reverts a rule commit. Only commits made by [`approve`] qualify.
pub fn revert(home: &Home, commit: &str) -> Result<RuleCommit> {
    if commit.len() < 7 || commit.len() > 40 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("недопустимый коммит");
    }
    let found = recent(home, 200)
        .into_iter()
        .find(|rule| rule.commit.starts_with(commit) || commit.starts_with(rule.commit.as_str()))
        .context("это не действующее правило")?;
    if let Err(error) = git(&home.root, &["revert", "--no-edit", &found.commit]) {
        let _ = git(&home.root, &["revert", "--abort"]);
        return Err(error.context("правило не отменилось: AGENTS.md меняли руками"));
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> (Home, PathBuf) {
        let dir = std::env::temp_dir().join(format!("agentsflow-rules-{}", uuid::Uuid::new_v4()));
        crate::home::init(&dir).unwrap();
        (Home::load(&dir).unwrap(), dir)
    }

    #[test]
    fn harvest_appends_notes_and_proposes_rules() {
        let (home, dir) = home();
        let dept = &home.departments["general"];
        let run = dept.run_dir("t1");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(run.join(NOTE_FILE), "Остатки лежат в 1С → Склад.\n").unwrap();
        std::fs::write(run.join(RULE_FILE), "Отчёты всегда в xlsx.").unwrap();
        let (noted, proposal) = harvest("general", &dept.dir, "t1", &run, 0).unwrap();
        assert!(noted);
        let proposal = proposal.unwrap();
        assert_eq!(proposal.id, "t1");
        assert!(!run.join(NOTE_FILE).exists() && !run.join(RULE_FILE).exists());
        let notes = std::fs::read_to_string(dept.memory_dir().join("notes.md")).unwrap();
        assert!(notes.contains("задача t1") && notes.contains("1С → Склад"));
        assert_eq!(notes.matches("## ").count(), 1, "{notes}");
        // A note left next to the rules is taken too.
        std::fs::write(dept.dir.join(NOTE_FILE), "Отчёт 1С по счёту 1330 быстрее.").unwrap();
        harvest("general", &dept.dir, "t2", &run, 0).unwrap();
        let notes = std::fs::read_to_string(dept.memory_dir().join("notes.md")).unwrap();
        assert!(notes.contains("задача t2") && notes.contains("счёту 1330"));
        assert!(!dept.dir.join(NOTE_FILE).exists());
        // A second run of the same task does not overwrite the proposal.
        std::fs::write(run.join(RULE_FILE), "Ещё правило").unwrap();
        let (_, second) = harvest("general", &dept.dir, "t1", &run, 0).unwrap();
        assert_eq!(second.unwrap().id, "t1-2");
        assert_eq!(proposals(&home).len(), 2);
        // Empty files are ignored.
        std::fs::write(run.join(RULE_FILE), "  \n").unwrap();
        assert_eq!(
            harvest("general", &dept.dir, "t1", &run, 0).unwrap(),
            (false, None)
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn dates_follow_the_offset() {
        assert_eq!(date_of(0), "1970-01-01");
        // 2026-11-05 23:30 UTC is already the 6th at +05:00.
        let late = 1_793_921_400;
        assert_eq!(date_of(late), "2026-11-05");
        assert_eq!(date_of(late + 5 * 3600), "2026-11-06");
    }

    #[test]
    fn approve_commits_reject_remembers_and_revert_undoes() {
        let (home, dir) = home();
        let dept = home.departments["general"].clone();
        let first = propose(&home, "general", "Отчёты всегда в xlsx.").unwrap();
        let rule = approve(&home, "general", &first.id).unwrap();
        assert_eq!(rule.subject, "rule(general): Отчёты всегда в xlsx.");
        assert!(dept.rules().contains("Отчёты всегда в xlsx."));
        assert!(approve(&home, "general", &first.id).is_err());
        let recent_rules = recent(&home, 5);
        assert_eq!(recent_rules.len(), 1);
        assert_eq!(recent_rules[0].commit, rule.commit);

        let second = propose(&home, "general", "Пиши стихами").unwrap();
        reject(&home, "general", &second.id).unwrap();
        assert!(rejected(&dept.dir, 1000).contains("Пиши стихами"));
        assert!(proposals(&home).is_empty());

        revert(&home, &rule.commit).unwrap();
        assert!(!dept.rules().contains("Отчёты всегда в xlsx."));
        assert!(recent(&home, 5).is_empty());
        assert!(revert(&home, &rule.commit).is_err());
        assert!(revert(&home, "zzzz").is_err());

        let own = propose(&home, ORCHESTRATOR, "Спрашивай про сроки").unwrap();
        approve(&home, ORCHESTRATOR, &own.id).unwrap();
        let orchestrator_rules = home.orchestrator_rules();
        assert!(orchestrator_rules.contains("Спрашивай про сроки"));
        assert!(propose(&home, "nope", "x").is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
