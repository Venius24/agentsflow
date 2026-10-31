use super::*;
use crate::{runtime::Runtime, team::Team};
use agentsflow_events::Envelope;
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    routing::post,
};
use std::sync::{Arc, Mutex};

type Calls = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

async fn fake(hub: Hub) -> (Bridge, Calls, tokio::task::JoinHandle<()>) {
    let calls: Calls = Arc::default();
    let app = Router::new()
        .route(
            "/{path}/{method}",
            post(
                |Path((_, method)): Path<(String, String)>,
                 State(calls): State<Calls>,
                 body: Bytes| async move {
                    calls.lock().unwrap().push((method, body.to_vec()));
                    Json(json!({"ok":true,"result":{"message_id":42}}))
                },
            ),
        )
        .with_state(calls.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let config = Config::parse("secret".into(), "11", format!("http://{address}")).unwrap();
    let bridge = Bridge::with_hub(config, Agents::new(), Team::preset("trio").unwrap(), hub);
    (bridge, calls, server)
}
fn msg(text: &str) -> Value {
    json!({"message":{"from":{"id":11},"chat":{"id":11,"type":"private"},"text":text}})
}
fn cb(data: &str) -> Value {
    json!({"callback_query":{"id":"cb","from":{"id":11},"data":data,"message":{"message_id":42,"chat":{"id":11,"type":"private"}}}})
}
fn last_text(calls: &Calls) -> String {
    calls
        .lock()
        .unwrap()
        .iter()
        .rev()
        .filter(|(method, _)| method != "answerCallbackQuery")
        .find_map(|(_, body)| {
            serde_json::from_slice::<Value>(body)
                .ok()?
                .get("text")?
                .as_str()
                .map(str::to_owned)
        })
        .unwrap_or_default()
}
fn last_data(calls: &Calls) -> Vec<String> {
    calls
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find_map(|(_, body)| {
            let value = serde_json::from_slice::<Value>(body).ok()?;
            let rows = value.pointer("/reply_markup/inline_keyboard")?.as_array()?;
            Some(
                rows.iter()
                    .flat_map(|row| row.as_array().unwrap().iter())
                    .filter_map(|b| b["callback_data"].as_str().map(str::to_owned))
                    .collect(),
            )
        })
        .unwrap_or_default()
}
fn add_task(hub: &Hub, title: &str, status: TaskStatus, assignee: Option<&str>) -> String {
    hub.create_task(
        AgentId::orchestrator(),
        NewTask {
            title: title.into(),
            description: "описание".into(),
            assignee: assignee.map(AgentId::from),
            status: Some(status),
            labels: Vec::new(),
        },
    )
    .unwrap()
    .id
}

#[tokio::test]
async fn board_filters_pages_and_card_actions() {
    let hub = Hub::new();
    hub.set_mail_enabled(true);
    let review = add_task(&hub, "Проверить меня", TaskStatus::Review, Some("lead"));
    for n in 0..10 {
        add_task(
            &hub,
            &format!("Работа {n}"),
            TaskStatus::InProgress,
            Some("main"),
        );
    }
    add_task(&hub, "Идея", TaskStatus::Backlog, None);
    let (mut bridge, calls, server) = fake(hub.clone()).await;
    bridge.update(&msg("/board")).await;
    let text = last_text(&calls);
    assert!(
        text.contains("Доска") && text.contains("В работе"),
        "{text}"
    );
    assert!(text.contains("Работа"));
    assert!(
        !text.contains("Проверить меня"),
        "default column is in progress"
    );
    let data = last_data(&calls);
    assert!(data.contains(&"s:b:3:0".to_owned()) && data.contains(&"s:b:2:1".to_owned()));
    bridge.update(&cb("s:b:2:1")).await;
    assert!(last_text(&calls).contains("9."));
    bridge.update(&cb("s:b:3:0")).await;
    assert!(last_text(&calls).contains("Проверить меня"));
    bridge.update(&cb(&format!("s:t:{review}"))).await;
    let text = last_text(&calls);
    assert!(text.contains("На проверке") && text.contains("lead"));
    let data = last_data(&calls);
    assert!(data.contains(&format!("s:ta:{review}:acc")));
    assert!(data.contains(&format!("s:ta:{review}:ret")));
    bridge.update(&cb(&format!("s:ta:{review}:acc"))).await;
    assert!(last_text(&calls).contains("Принять задачу"));
    assert_eq!(hub.task(&review).unwrap().status, TaskStatus::Review);
    bridge.update(&cb(&format!("s:ta:{review}:accy"))).await;
    assert_eq!(hub.task(&review).unwrap().status, TaskStatus::Done);
    assert_eq!(hub.task(&review).unwrap().progress, 100);
    let idea = hub
        .tasks(false)
        .into_iter()
        .find(|t| t.title == "Идея")
        .unwrap()
        .id;
    bridge.update(&cb(&format!("s:ta:{idea}:sm"))).await;
    assert!(last_data(&calls).contains(&format!("s:ts:{idea}:5")));
    bridge.update(&cb(&format!("s:ts:{idea}:5"))).await;
    assert_eq!(hub.task(&idea).unwrap().status, TaskStatus::Blocked);
    bridge.update(&cb(&format!("s:tu:{idea}:1"))).await;
    assert_eq!(
        hub.task(&idea).unwrap().assignee,
        Some(AgentId::from("main"))
    );
    let todo = add_task(&hub, "К запуску", TaskStatus::Todo, Some("junior"));
    bridge.update(&cb(&format!("s:t:{todo}"))).await;
    assert!(last_data(&calls).contains(&format!("s:ta:{todo}:run")));
    bridge.update(&cb(&format!("s:ta:{todo}:run"))).await;
    assert_eq!(hub.task(&todo).unwrap().status, TaskStatus::InProgress);
    assert!(hub.task(&todo).unwrap().thread.is_some());
    bridge.update(&cb(&format!("s:ta:{todo}:ar"))).await;
    assert_eq!(hub.task(&todo).unwrap().status, TaskStatus::InProgress);
    bridge.update(&cb(&format!("s:ta:{todo}:ary"))).await;
    assert_eq!(hub.task(&todo).unwrap().status, TaskStatus::Archived);
    let loose = add_task(&hub, "Без исполнителя", TaskStatus::Backlog, None);
    bridge.update(&cb(&format!("s:ta:{loose}:run"))).await;
    assert!(last_text(&calls).contains("Нужен исполнитель"));
    let before = calls.lock().unwrap().len();
    let mut stranger = cb("s:ta:x:ary");
    stranger["callback_query"]["from"]["id"] = json!(99);
    bridge.update(&stranger).await;
    assert_eq!(calls.lock().unwrap().len(), before);
    server.abort();
}

#[tokio::test]
async fn three_ways_to_create_a_task_and_edits() {
    let hub = Hub::new();
    let (mut bridge, calls, server) = fake(hub.clone()).await;
    bridge.update(&msg("/task Первая | с описанием")).await;
    let first = hub.tasks(false).remove(0);
    assert_eq!(
        (first.title.as_str(), first.description.as_str()),
        ("Первая", "с описанием")
    );
    assert!(last_text(&calls).contains("Задача создана"));
    bridge.update(&cb("s:nt")).await;
    assert!(bridge.inputs.contains_key(&11));
    bridge.update(&msg("Вторая")).await;
    assert_eq!(hub.tasks(false).len(), 2);
    assert!(!bridge.inputs.contains_key(&11));
    bridge.update(&msg("/task")).await;
    bridge.update(&msg("Третья")).await;
    assert_eq!(hub.tasks(false).len(), 3);
    bridge.update(&msg("/task")).await;
    bridge.update(&msg(&"я".repeat(300))).await;
    assert_eq!(hub.tasks(false).len(), 3);
    assert_eq!(
        bridge.queues[&11].back().unwrap().text,
        "Название: от 1 до 200 символов"
    );
    bridge.update(&cb(&format!("s:ta:{}:et", first.id))).await;
    bridge.update(&msg("Новое название")).await;
    assert_eq!(hub.task(&first.id).unwrap().title, "Новое название");
    bridge.update(&cb(&format!("s:ta:{}:ed", first.id))).await;
    bridge.update(&msg("Новое описание")).await;
    assert_eq!(hub.task(&first.id).unwrap().description, "Новое описание");
    bridge.update(&cb("s:nt")).await;
    bridge.update(&msg("👥 Команда")).await;
    assert!(bridge.inputs.is_empty());
    server.abort();
}

#[tokio::test]
async fn review_and_blocked_notifications_have_buttons() {
    let hub = Hub::new();
    let id = add_task(&hub, "Сдать", TaskStatus::InProgress, Some("lead"));
    let (mut bridge, calls, server) = fake(hub.clone()).await;
    let patch = |status| TaskPatch {
        status: Some(status),
        ..TaskPatch::default()
    };
    let task = hub
        .update_task(AgentId::from("lead"), &id, patch(TaskStatus::Review))
        .unwrap();
    bridge.handle_event(Envelope {
        session: "s".into(),
        agent: AgentId::from("lead"),
        seq: 1,
        ts_ms: 1,
        event: Event::Task { task },
    });
    bridge.flush().await;
    assert!(last_text(&calls).contains("сдал задачу «Сдать» на проверку"));
    assert_eq!(
        last_data(&calls),
        vec![format!("s:t:{id}"), format!("s:ta:{id}:acc")]
    );
    let task = hub
        .update_task(AgentId::from("lead"), &id, patch(TaskStatus::Blocked))
        .unwrap();
    bridge.handle_event(Envelope {
        session: "s".into(),
        agent: AgentId::from("lead"),
        seq: 2,
        ts_ms: 2,
        event: Event::Task { task },
    });
    bridge.last_sent.clear();
    bridge.flush().await;
    assert!(last_text(&calls).contains("застряла"));
    server.abort();
}

#[tokio::test]
async fn mail_list_thread_reply_and_new() {
    let hub = Hub::new();
    hub.set_mail_enabled(true);
    hub.set_mail_thread_limit(3);
    let first = hub
        .send_mail(
            AgentId::from("lead"),
            AgentId::orchestrator(),
            "План".into(),
            "Предлагаю такой план".into(),
            None,
        )
        .unwrap();
    let (mut bridge, calls, server) = fake(hub.clone()).await;
    bridge.update(&msg("/mail")).await;
    let text = last_text(&calls);
    assert!(text.contains("План") && text.contains('●'), "{text}");
    bridge
        .update(&cb(&format!("s:mt:{}:0", first.thread)))
        .await;
    assert!(last_text(&calls).contains("Предлагаю такой план"));
    bridge.update(&cb("s:m:0")).await;
    assert!(
        !last_text(&calls).contains('●'),
        "opened thread is marked seen"
    );
    bridge.update(&cb(&format!("s:mr:{}", first.thread))).await;
    bridge.update(&msg("Согласен")).await;
    let sent = hub.mail_all().pop().unwrap();
    assert_eq!(
        (sent.from.clone(), sent.to.clone(), sent.thread.clone()),
        (
            AgentId::orchestrator(),
            AgentId::from("lead"),
            first.thread.clone()
        )
    );
    assert_eq!(sent.subject, "Re: План");
    bridge.update(&cb(&format!("s:mr:{}", first.thread))).await;
    bridge.update(&msg("Ещё одно")).await;
    bridge.update(&cb(&format!("s:mr:{}", first.thread))).await;
    bridge.update(&msg("И ещё")).await;
    assert_eq!(
        bridge.queues[&11].back().unwrap().text,
        "В этой переписке исчерпан лимит писем"
    );
    bridge.update(&msg("/mail main Вопрос | Как дела?")).await;
    let sent = hub.mail_all().pop().unwrap();
    assert_eq!(
        (sent.to, sent.subject.as_str(), sent.body.as_str()),
        (AgentId::from("main"), "Вопрос", "Как дела?")
    );
    bridge.update(&cb("s:mn")).await;
    assert!(last_data(&calls).contains(&"s:mn:2".to_owned()));
    bridge.update(&cb("s:mn:2")).await;
    bridge.update(&msg("Без разделителя")).await;
    assert_eq!(
        bridge.queues[&11].back().unwrap().text,
        "Формат: тема | текст письма"
    );
    bridge.update(&msg("Тема | Текст")).await;
    assert_eq!(hub.mail_all().pop().unwrap().to, AgentId::from("junior"));
    bridge.update(&msg("/mail nobody тема | текст")).await;
    assert!(
        bridge.queues[&11]
            .back()
            .unwrap()
            .text
            .starts_with("Формат: /mail")
    );
    server.abort();
}

#[tokio::test]
async fn workflow_screens_without_running_anything() {
    use tokio::task::LocalSet;
    LocalSet::new()
        .run_until(async {
            let runtime = Runtime::new(crate::runtime::RuntimeOptions {
                team: Team::preset("trio").unwrap(),
                ..Default::default()
            })
            .unwrap();
            let manager = crate::workflow::Manager::new(
                runtime.hub.clone(),
                runtime.agents.clone(),
                runtime.team.clone(),
            );
            let ok = crate::workflow::Spec::parse(
                "name='ревью'\ndescription='Проверка кода'\n[[step]]\nid='a'\nagent='lead'\nprompt='x'\n[[step]]\nid='b'\nagent='main'\nprompt='y'",
            )
            .unwrap();
            let bad = crate::workflow::Spec::parse(
                "name='чужой'\n[[step]]\nid='s'\nagent='missing'\nprompt='x'",
            )
            .unwrap();
            let (mut bridge, calls, server) = fake(runtime.hub.clone()).await;
            bridge.control = Some(Control {
                specs: vec![ok, bad],
                manager,
                projects: None,
                announce: None,
            });
            bridge.update(&msg("/run")).await;
            let text = last_text(&calls);
            assert!(text.contains("ревью") && text.contains("Проверка кода"));
            bridge.update(&cb("s:ws:0")).await;
            assert!(last_text(&calls).contains("Что поручить"));
            bridge.update(&msg("Починить сборку")).await;
            let text = last_text(&calls);
            assert!(text.contains("Запустить «ревью» с задачей"), "{text}");
            assert!(text.contains("Починить сборку") && text.contains("lead"));
            assert_eq!(last_data(&calls), vec!["s:wy", "s:w"]);
            bridge.update(&cb("s:ws:1")).await;
            bridge.update(&msg("задача")).await;
            bridge.update(&cb("s:wy")).await;
            assert!(!bridge.control.as_ref().unwrap().manager.state().running);
            bridge.update(&cb("s:wy")).await;
            let before = calls.lock().unwrap().len();
            bridge.update(&cb("s:wx")).await;
            let answer = calls.lock().unwrap()[before..]
                .iter()
                .find(|(method, _)| method == "answerCallbackQuery")
                .map(|(_, body)| String::from_utf8_lossy(body).into_owned())
                .unwrap();
            assert!(answer.contains("ничего не запущено"), "{answer}");
            for (step, status) in [
                (None, WorkflowStatus::Started),
                (Some("a"), WorkflowStatus::StepStarted),
                (Some("b"), WorkflowStatus::StepStarted),
            ] {
                bridge.track_workflow(&Event::Workflow {
                    name: "ревью".into(),
                    step: step.map(str::to_owned),
                    status,
                    detail: String::new(),
                });
            }
            let (text, rows) = bridge.workflow_text().unwrap();
            assert_eq!(text, "⚙ <b>ревью</b>: шаг 2 из 2: b · main");
            assert_eq!(rows[0][0]["callback_data"], "s:wx");
            bridge.track_workflow(&Event::Workflow {
                name: "ревью".into(),
                step: None,
                status: WorkflowStatus::Failed,
                detail: "stopped".into(),
            });
            assert_eq!(
                bridge.workflow_text().unwrap().0,
                "⏹ Workflow «ревью»: остановлен"
            );
            server.abort();
            runtime.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn project_screens_validate_selection_and_busy() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let root = std::env::temp_dir().join(format!(
                "agentsflow-telegram-projects-{}",
                uuid::Uuid::new_v4()
            ));
            let a = root.join("a");
            let b = root.join("b");
            for path in [&a, &b] {
                std::fs::create_dir_all(path.join(".git")).unwrap();
            }
            let hub = Hub::new();
            let (actor, task) =
                crate::hub::spawn(hub.clone(), AgentId::from("claude"), false, || async {
                    Ok(crate::testing::Silent::new(1, false))
                });
            let agents = Agents::from([(AgentId::from("claude"), actor.clone())]);
            let manager = crate::workflow::Manager::new(
                hub.clone(),
                agents.clone(),
                Team::preset("pair").unwrap(),
            );
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let projects = crate::projects::Control {
                roots: vec![root.clone()],
                current: a.clone(),
                hub: hub.clone(),
                agents,
                manager: manager.clone(),
                tx,
            };
            let (mut bridge, calls, server) = fake(hub).await;
            bridge.control = Some(Control {
                specs: vec![],
                manager,
                projects: Some(projects.clone()),
                announce: None,
            });
            bridge.update(&msg("/more")).await;
            assert!(last_data(&calls).contains(&"s:proj".into()));
            bridge.update(&cb("s:proj")).await;
            assert!(last_text(&calls).contains("Проекты"));
            let index = projects
                .list()
                .unwrap()
                .iter()
                .position(|p| p.path == crate::projects::canonical(&b).unwrap())
                .unwrap();
            bridge.update(&cb(&format!("s:proj:{index}"))).await;
            assert!(last_text(&calls).contains("Перезапустить команду"));
            actor.prompt("work".into()).await.unwrap();
            bridge.update(&cb(&format!("s:projgo:{index}"))).await;
            assert!(
                calls
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(method, body)| method == "answerCallbackQuery"
                        && String::from_utf8_lossy(body).contains("Сначала остановите"))
            );
            assert!(rx.try_recv().is_err());
            actor.interrupt().await.unwrap();
            while actor.state().busy {
                tokio::task::yield_now().await;
            }
            bridge.update(&cb(&format!("s:projgo:{index}"))).await;
            assert_eq!(
                rx.recv().await.unwrap(),
                crate::projects::canonical(&b).unwrap()
            );
            actor.shutdown().await;
            task.await.unwrap();
            server.abort();
            std::fs::remove_dir_all(root).unwrap();
        })
        .await;
}

#[tokio::test]
async fn stats_log_export_more_and_help() {
    let hub = Hub::new();
    hub.publish(
        AgentId::from("lead"),
        "s",
        Event::Usage {
            input_tokens: 123,
            output_tokens: 45,
            cost_usd: Some(0.12),
        },
    );
    add_task(&hub, "Одна", TaskStatus::Done, Some("lead"));
    for n in 0..14 {
        hub.publish(
            AgentId::from("lead"),
            "s",
            Event::Message {
                role: Role::User,
                text: format!("вопрос {n}"),
                delta: false,
            },
        );
    }
    hub.publish(
        AgentId::from("lead"),
        "s",
        Event::Done {
            reason: "success".into(),
        },
    );
    let (mut bridge, calls, server) = fake(hub.clone()).await;
    bridge.update(&msg("/stats")).await;
    let text = last_text(&calls);
    assert!(
        text.contains("Статистика") && text.contains("Готово 1") && text.contains("lead"),
        "{text}"
    );
    assert!(text.contains("ходов 1"));
    assert!(
        text.contains("токенов 123/45")
            && text.contains("$0.12")
            && text.contains("Всего токенов: 123/45"),
        "{text}"
    );
    bridge.update(&msg("/log lead 5")).await;
    let text = last_text(&calls);
    assert_eq!(text.matches("вопрос").count(), 4, "{text}");
    assert!(last_data(&calls).contains(&"s:l:0:1:5".to_owned()));
    bridge.update(&cb("s:l:0:1:5")).await;
    assert!(last_text(&calls).contains("вопрос 7"));
    bridge.update(&msg("/log nobody")).await;
    assert_eq!(bridge.queues[&11].back().unwrap().text, "Нет такого агента");
    bridge.update(&msg("/export md")).await;
    assert!(last_text(&calls).contains("пройдёт через серверы Telegram"));
    assert!(
        !calls
            .lock()
            .unwrap()
            .iter()
            .any(|(m, _)| m == "sendDocument")
    );
    bridge.update(&cb("s:exy:md")).await;
    let documents: Vec<_> = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(m, _)| m == "sendDocument")
        .map(|(_, body)| String::from_utf8_lossy(body).into_owned())
        .collect();
    assert_eq!(documents.len(), 1);
    assert!(documents[0].contains("вопрос 3") && !documents[0].contains("secret"));
    bridge.update(&msg("/export jsonl")).await;
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _)| m == "sendDocument")
            .count(),
        2,
        "no second confirmation"
    );
    bridge.update(&msg("/export pdf")).await;
    assert_eq!(
        bridge.queues[&11].back().unwrap().text,
        "Формат: md или jsonl"
    );
    bridge.update(&msg("⚙ Ещё")).await;
    let data = last_data(&calls);
    for expected in ["s:st", "s:w", "s:lg", "s:ex", "s:help"] {
        assert!(data.contains(&expected.to_owned()), "{expected}");
    }
    bridge.update(&msg("/help")).await;
    let help = &bridge.queues[&11].back().unwrap().text;
    for command in [
        "/team", "/board", "/mail", "/run", "/stats", "/log", "/export", "/mute", "/more",
    ] {
        assert!(help.contains(command), "{command}");
    }
    let version_line = format!("Версия: {}", env!("CARGO_PKG_VERSION"));
    assert_eq!(help.lines().last(), Some(version_line.as_str()));
    bridge.update(&cb("s:help")).await;
    let help = &bridge.queues[&11].back().unwrap().text;
    assert_eq!(help.lines().last(), Some(version_line.as_str()));
    server.abort();
}

#[tokio::test]
async fn unknown_user_gets_silence_and_every_command_is_handled() {
    let hub = Hub::new();
    hub.set_mail_enabled(true);
    let (mut bridge, calls, server) = fake(hub.clone()).await;
    let mut stranger = msg("/board");
    stranger["message"]["from"]["id"] = json!(99);
    stranger["message"]["chat"]["id"] = json!(99);
    bridge.update(&stranger).await;
    let mut stranger = cb("s:st");
    stranger["callback_query"]["from"]["id"] = json!(99);
    bridge.update(&stranger).await;
    assert!(calls.lock().unwrap().is_empty() && bridge.queues.is_empty());
    for command in bot_commands()["commands"].as_array().unwrap() {
        let name = command["command"].as_str().unwrap();
        bridge.queues.clear();
        bridge.update(&msg(&format!("/{name}"))).await;
        assert!(
            bridge
                .queues
                .values()
                .flatten()
                .all(|item| item.text != "Используйте /help"),
            "/{name} is registered but not handled"
        );
    }
    server.abort();
}

#[tokio::test]
async fn persistent_keyboard_buttons_open_their_screens() {
    let hub = Hub::new();
    hub.set_mail_enabled(true);
    let (mut bridge, calls, server) = fake(hub).await;
    bridge.update(&msg("📋 Доска")).await;
    assert!(last_text(&calls).contains("Доска"));
    bridge.update(&msg("✉ Почта")).await;
    assert!(last_text(&calls).contains("Почта"));
    bridge.update(&msg("⚙ Ещё")).await;
    assert!(last_text(&calls).contains("Ещё"));
    assert!(
        bridge
            .queues
            .values()
            .flatten()
            .all(|item| item.text != "Используйте /help")
    );
    server.abort();
}
