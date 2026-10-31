use crate::{hub::Hub, runtime::Agents, workflow::Manager};
use serde::Serialize;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Serialize)]
pub struct Project {
    pub name: String,
    pub path: PathBuf,
    pub current: bool,
}

#[derive(Clone)]
pub struct Control {
    pub roots: Vec<PathBuf>,
    pub current: PathBuf,
    pub hub: Hub,
    pub agents: Agents,
    pub manager: Manager,
    pub tx: mpsc::UnboundedSender<PathBuf>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SwitchError {
    Unknown,
    Busy,
    Unavailable,
}
pub struct SwitchTicket {
    gate: std::sync::Arc<std::sync::Mutex<bool>>,
    tx: mpsc::UnboundedSender<PathBuf>,
    path: Option<PathBuf>,
}
impl SwitchTicket {
    pub fn commit(mut self) -> Result<(), SwitchError> {
        let path = self.path.take().expect("uncommitted switch");
        if self.tx.send(path).is_err() {
            *self.gate.lock().unwrap() = false;
            return Err(SwitchError::Unavailable);
        }
        Ok(())
    }
}
impl Drop for SwitchTicket {
    fn drop(&mut self) {
        if self.path.is_some() {
            *self.gate.lock().unwrap() = false;
        }
    }
}
impl Control {
    pub fn enabled(&self) -> bool {
        !self.roots.is_empty()
    }
    pub fn list(&self) -> std::io::Result<Vec<Project>> {
        list(&self.roots, &self.current)
    }
    pub fn switch(&self, requested: &Path) -> Result<bool, SwitchError> {
        match self.prepare(requested)? {
            Some(ticket) => {
                ticket.commit()?;
                Ok(true)
            }
            None => Ok(false),
        }
    }
    pub fn prepare(&self, requested: &Path) -> Result<Option<SwitchTicket>, SwitchError> {
        if !self.enabled() {
            return Err(SwitchError::Unknown);
        }
        let path = canonical(requested).map_err(|_| SwitchError::Unknown)?;
        let projects = self.list().map_err(|_| SwitchError::Unavailable)?;
        if !projects.iter().any(|p| p.path == path) {
            return Err(SwitchError::Unknown);
        }
        if path == canonical(&self.current).map_err(|_| SwitchError::Unavailable)? {
            return Ok(None);
        }
        let gate = self.hub.admission();
        let mut blocked = gate.lock().unwrap();
        if *blocked
            || self.manager.state().running
            || self.agents.values().any(|a| {
                let state = a.state();
                state.busy || !state.pending.is_empty()
            })
        {
            return Err(SwitchError::Busy);
        }
        *blocked = true;
        if self.tx.is_closed() {
            *blocked = false;
            return Err(SwitchError::Unavailable);
        }
        drop(blocked);
        Ok(Some(SwitchTicket {
            gate,
            tx: self.tx.clone(),
            path: Some(path),
        }))
    }
}

pub fn canonical(path: &Path) -> std::io::Result<PathBuf> {
    let path = std::fs::canonicalize(path)?;
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return Ok(PathBuf::from(format!(r"\\{rest}")));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return Ok(PathBuf::from(rest));
        }
    }
    Ok(path)
}

pub fn list(roots: &[PathBuf], current: &Path) -> std::io::Result<Vec<Project>> {
    let current = canonical(current)?;
    let mut paths = vec![current.clone()];
    for root in roots {
        let root = canonical(root)?;
        for entry in std::fs::read_dir(&root)? {
            let Ok(entry) = entry else { continue };
            let candidate = entry.path();
            if !candidate.is_dir() || !candidate.join(".git").exists() {
                continue;
            }
            let Ok(path) = canonical(&candidate) else {
                continue;
            };
            if path.parent() != Some(root.as_path()) || paths.contains(&path) {
                continue;
            }
            paths.push(path);
        }
    }
    let mut counts = HashMap::<String, usize>::new();
    for path in &paths {
        *counts.entry(name(path).to_lowercase()).or_default() += 1;
    }
    let mut result: Vec<_> = paths
        .into_iter()
        .map(|path| {
            let base = name(&path);
            let name = if counts[&base.to_lowercase()] > 1 {
                format!(
                    "{} ({})",
                    base,
                    path.parent()
                        .map_or_else(String::new, |p| p.display().to_string())
                )
            } else {
                base
            };
            Project {
                name,
                current: path == current,
                path,
            }
        })
        .collect();
    result.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.path.cmp(&b.path))
    });
    Ok(result)
}
fn name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |s| s.to_string_lossy().into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn finds_projects_and_disambiguates() {
        let root =
            std::env::temp_dir().join(format!("agentsflow-projects-{}", uuid::Uuid::new_v4()));
        let a = root.join("a");
        let b = root.join("b");
        let cwd = root.join("outside");
        for p in [a.join("repo"), b.join("repo"), a.join("other"), cwd.clone()] {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::create_dir(a.join("repo/.git")).unwrap();
        std::fs::write(b.join("repo/.git"), "gitdir: elsewhere").unwrap();
        let foreign = root.join("foreign");
        std::fs::create_dir_all(foreign.join(".git")).unwrap();
        #[cfg(windows)]
        let _ = std::os::windows::fs::symlink_dir(&foreign, a.join("link"));
        #[cfg(unix)]
        let _ = std::os::unix::fs::symlink(&foreign, a.join("link"));
        let list = list(&[a, b], &cwd).unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(
            list.iter().filter(|p| p.name.starts_with("repo (")).count(),
            2
        );
        assert_eq!(list.iter().filter(|p| p.current).count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn switching_rejects_disabled_unknown_and_busy() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let root = std::env::temp_dir().join(format!(
                    "agentsflow-project-control-{}",
                    uuid::Uuid::new_v4()
                ));
                let a = root.join("a");
                let b = root.join("b");
                let outside = root.join("outside");
                for path in [&a, &b, &outside] {
                    std::fs::create_dir_all(path).unwrap();
                }
                std::fs::create_dir(a.join(".git")).unwrap();
                std::fs::create_dir(b.join(".git")).unwrap();
                let hub = Hub::new();
                let (actor, task) = crate::hub::spawn(
                    hub.clone(),
                    agentsflow_events::AgentId::from("claude"),
                    false,
                    || async { Ok(crate::testing::Silent::new(1, false)) },
                );
                let agents =
                    Agents::from([(agentsflow_events::AgentId::from("claude"), actor.clone())]);
                let manager = Manager::new(
                    hub.clone(),
                    agents.clone(),
                    crate::team::Team::preset("pair").unwrap(),
                );
                let (tx, mut rx) = mpsc::unbounded_channel();
                let mut control = Control {
                    roots: vec![],
                    current: a.clone(),
                    hub,
                    agents,
                    manager,
                    tx,
                };
                assert_eq!(control.switch(&b), Err(SwitchError::Unknown));
                control.roots.push(root.clone());
                assert_eq!(control.switch(&outside), Err(SwitchError::Unknown));
                assert_eq!(control.switch(&a), Ok(false));
                actor.prompt("work".into()).await.unwrap();
                assert_eq!(control.switch(&b), Err(SwitchError::Busy));
                actor.interrupt().await.unwrap();
                while actor.state().busy {
                    tokio::task::yield_now().await;
                }
                assert_eq!(control.switch(&b), Ok(true));
                assert_eq!(rx.recv().await.unwrap(), canonical(&b).unwrap());
                assert_eq!(
                    actor.prompt("blocked".into()).await,
                    Err(crate::hub::HubError::Busy)
                );
                actor.shutdown().await;
                task.await.unwrap();
                std::fs::remove_dir_all(root).unwrap();
            })
            .await;
    }
}
