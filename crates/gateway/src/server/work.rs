//! Routes of `serve --home`: departments, department tasks, runs, spending.

use super::*;
use crate::runs::{NewWork, Runs};
use agentsflow_events::Risk;

fn no_home() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error":"доступно только в режиме serve --home"})),
    )
        .into_response()
}

fn runs(state: &AppState) -> Result<&Runs, Box<Response>> {
    state.runs.as_ref().ok_or_else(|| Box::new(no_home()))
}

pub(super) async fn departments(State(state): State<AppState>) -> Response {
    let runs = match runs(&state) {
        Ok(runs) => runs,
        Err(response) => return *response,
    };
    let home = runs.home();
    let spend = runs.month_spend();
    let running = runs.running();
    let list: Vec<Value> = home
        .departments
        .values()
        .map(|dept| {
            let agents: Vec<Value> = dept
                .team
                .members
                .iter()
                .map(|m| json!({"id":m.id,"kind":m.kind,"model":m.model,"roles":m.roles,"title":m.title,"mode":m.mode}))
                .collect();
            json!({
                "id": dept.id,
                "title": dept.title,
                "description": dept.description,
                "default_risk": dept.default_risk,
                "max_parallel": dept.max_parallel,
                "schedules": dept.schedules.iter().map(|s| json!({"cron":s.cron,"title":s.title})).collect::<Vec<_>>(),
                "limits": {
                    "task_usd": dept.limits.task_usd,
                    "month_usd": dept.limits.month_usd,
                    "timeout_mins": dept.limits.timeout_mins,
                    "max_loops": dept.limits.max_loops,
                },
                "agents": agents,
                "running": running.iter().filter(|run| run.department == dept.id).count(),
                "month_spent_usd": spend.get(&dept.id).copied().unwrap_or(0.0),
            })
        })
        .collect();
    Json(json!({"departments": list, "max_parallel_runs": home.config.max_parallel_runs}))
        .into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateWork {
    #[serde(default)]
    title: Option<String>,
    text: String,
    department: String,
    #[serde(default)]
    risk: Option<Risk>,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    budget_usd: Option<f64>,
}

/// A short title from the first line of the text.
pub fn title_from(text: &str) -> String {
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    let mut title: String = line.trim().chars().take(80).collect();
    if line.trim().chars().count() > 80 {
        title.push('…');
    }
    title
}

pub(super) async fn create_work(
    State(state): State<AppState>,
    input: Result<Json<CreateWork>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let runs = match runs(&state) {
        Ok(runs) => runs,
        Err(response) => return *response,
    };
    let Ok(Json(input)) = input else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid task"})),
        )
            .into_response();
    };
    if input.text.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"пустая задача"})),
        )
            .into_response();
    }
    let title = input
        .title
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| title_from(&input.text));
    match runs.create(
        AgentId::orchestrator(),
        NewWork {
            title,
            text: input.text,
            department: input.department,
            risk: input.risk,
            depends_on: input.depends_on,
            budget_usd: input.budget_usd,
            source: "owner".into(),
            ..Default::default()
        },
    ) {
        Ok(task) => (StatusCode::CREATED, Json(json!({"task":task}))).into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({"error":error}))).into_response(),
    }
}

pub(super) async fn stop_task(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let runs = match runs(&state) {
        Ok(runs) => runs,
        Err(response) => return *response,
    };
    if runs.stop(&id) {
        Json(json!({"ok":true})).into_response()
    } else {
        (
            StatusCode::CONFLICT,
            Json(json!({"error":"задача не выполняется"})),
        )
            .into_response()
    }
}

pub(super) async fn retry_task(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    if let Err(response) = runs(&state) {
        return *response;
    }
    let patch = TaskPatch {
        status: Some(agentsflow_events::TaskStatus::Todo),
        ..Default::default()
    };
    match state.hub.update_task(AgentId::orchestrator(), &id, patch) {
        Ok(task) => Json(json!({"task":task})).into_response(),
        Err(error) => task_failure(error),
    }
}

pub(super) async fn task_result(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let runs = match runs(&state) {
        Ok(runs) => runs,
        Err(response) => return *response,
    };
    let Some(task) = state.hub.task(&id) else {
        return task_failure(TaskError::NotFound);
    };
    let Some(path) = task.work.and_then(|work| work.result_path) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error":"результата нет"})),
        )
            .into_response();
    };
    let root = runs.home().root.clone();
    let full = root.join(&path);
    // The path comes from the board file; never read outside the home.
    let inside = full
        .canonicalize()
        .ok()
        .zip(root.canonicalize().ok())
        .is_some_and(|(full, root)| full.starts_with(root));
    if !inside {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error":"результата нет"})),
        )
            .into_response();
    }
    match std::fs::read_to_string(&full) {
        Ok(text) => Json(json!({"path":path,"text":text})).into_response(),
        Err(_) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error":"результата нет"})),
        )
            .into_response(),
    }
}

pub(super) async fn list_runs(State(state): State<AppState>) -> Response {
    match runs(&state) {
        Ok(runs) => Json(json!({"runs":runs.running()})).into_response(),
        Err(response) => *response,
    }
}

pub(super) async fn reload_home(State(state): State<AppState>) -> Response {
    let runs = match runs(&state) {
        Ok(runs) => runs,
        Err(response) => return *response,
    };
    match runs.reload() {
        Ok(()) => Json(json!({"ok":true})).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":format!("{error:#}")})),
        )
            .into_response(),
    }
}

pub(super) async fn spend(State(state): State<AppState>) -> Response {
    let runs = match runs(&state) {
        Ok(runs) => runs,
        Err(response) => return *response,
    };
    let month = runs.month_spend();
    let total = month.values().fold(0.0, |sum, usd| sum + usd);
    Json(json!({"month": month, "total_usd": total})).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Text {
    text: String,
}

fn bad_request(error: String) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error":error}))).into_response()
}

/// A free-form owner message: the orchestrator turns it into tasks.
pub(super) async fn orchestrator(
    State(state): State<AppState>,
    input: Result<Json<Text>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let runs = match runs(&state) {
        Ok(runs) => runs,
        Err(response) => return *response,
    };
    let Ok(Json(input)) = input else {
        return bad_request("invalid message".into());
    };
    match runs.message(&input.text) {
        Ok(()) => (StatusCode::ACCEPTED, Json(json!({"ok":true}))).into_response(),
        Err(error) => bad_request(error),
    }
}

pub(super) async fn answer(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    input: Result<Json<Text>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let runs = match runs(&state) {
        Ok(runs) => runs,
        Err(response) => return *response,
    };
    let Ok(Json(input)) = input else {
        return bad_request("invalid answer".into());
    };
    match runs.answer(&id, &input.text) {
        Ok(task) => Json(json!({"task":task})).into_response(),
        Err(error) => bad_request(error),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Decision {
    create: bool,
}

/// The owner's decision on a proposed department.
pub(super) async fn proposal(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    input: Result<Json<Decision>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let runs = match runs(&state) {
        Ok(runs) => runs,
        Err(response) => return *response,
    };
    let Ok(Json(input)) = input else {
        return bad_request("invalid decision".into());
    };
    match runs.resolve_proposal(&id, input.create) {
        Ok(department) => Json(json!({"department":department})).into_response(),
        Err(error) => bad_request(error),
    }
}

/// Open rule proposals and the latest added rules.
pub(super) async fn rules(State(state): State<AppState>) -> Response {
    match runs(&state) {
        Ok(runs) => {
            Json(json!({"proposals": runs.rule_proposals(), "recent": runs.recent_rules(20)}))
                .into_response()
        }
        Err(response) => *response,
    }
}

pub(super) async fn approve_rule(
    State(state): State<AppState>,
    axum::extract::Path((target, id)): axum::extract::Path<(String, String)>,
) -> Response {
    let runs = match runs(&state) {
        Ok(runs) => runs,
        Err(response) => return *response,
    };
    match runs.approve_rule(&target, &id) {
        Ok(rule) => Json(json!({"rule": rule})).into_response(),
        Err(error) => bad_request(error),
    }
}

pub(super) async fn reject_rule(
    State(state): State<AppState>,
    axum::extract::Path((target, id)): axum::extract::Path<(String, String)>,
) -> Response {
    let runs = match runs(&state) {
        Ok(runs) => runs,
        Err(response) => return *response,
    };
    match runs.reject_rule(&target, &id) {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(error) => bad_request(error),
    }
}

pub(super) async fn revert_rule(
    State(state): State<AppState>,
    axum::extract::Path(commit): axum::extract::Path<String>,
) -> Response {
    let runs = match runs(&state) {
        Ok(runs) => runs,
        Err(response) => return *response,
    };
    match runs.revert_rule(&commit) {
        Ok(rule) => Json(json!({"rule": rule})).into_response(),
        Err(error) => bad_request(error),
    }
}

#[cfg(test)]
mod tests {
    use super::title_from;

    #[test]
    fn titles_come_from_the_first_line() {
        assert_eq!(
            title_from("\n  Отчёт по Байтаку \nподробности"),
            "Отчёт по Байтаку"
        );
        assert_eq!(title_from(&"я".repeat(90)).chars().count(), 81);
    }
}
