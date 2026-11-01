//! The brief: everything an agent of a department gets at the start of a run.
//! It is built by the gateway, not discovered by the CLI, so Claude and Codex
//! see the same thing and the owner can read it in `runs/<task>/brief.md`.

use crate::department::Department;
use agentsflow_events::{Risk, TaskSnapshot};
use std::path::Path;

/// The last `limit` bytes of the department notes, cut on a line boundary.
pub fn memory_digest(dept: &Department, limit: usize) -> String {
    let text = std::fs::read_to_string(dept.memory_dir().join("notes.md")).unwrap_or_default();
    if text.len() <= limit {
        return text.trim().to_owned();
    }
    let mut start = text.len() - limit;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let tail = &text[start..];
    let tail = tail.find('\n').map_or(tail, |line| &tail[line + 1..]);
    format!("…(начало памяти не поместилось)\n{}", tail.trim())
}

pub struct Input {
    pub title: String,
    pub result: std::path::PathBuf,
}

pub fn build(
    dept: &Department,
    task: &TaskSnapshot,
    inputs: &[Input],
    digest_kb: usize,
    run_dir: &Path,
) -> String {
    let work = task.work.clone().unwrap_or_default();
    let mut out = String::new();
    out.push_str(&format!(
        "Ты работаешь в отделе «{}» ({}).\n\n",
        dept.title, dept.id
    ));
    out.push_str("# Правила отдела\n\n");
    let rules = dept.rules();
    out.push_str(if rules.trim().is_empty() {
        "Правил пока нет.\n"
    } else {
        rules.trim()
    });
    out.push_str("\n\n# Память отдела\n\n");
    let memory = memory_digest(dept, digest_kb * 1024);
    out.push_str(if memory.is_empty() {
        "Память пуста."
    } else {
        &memory
    });
    out.push_str(&format!("\n\n# Задача {}: {}\n\n", task.id, task.title));
    out.push_str(task.description.trim());
    out.push('\n');
    if !work.review_notes.is_empty() {
        out.push_str(
            "\n# Замечания приёмки\n\nПрошлый результат вернули с замечаниями; исправь их:\n",
        );
        for note in &work.review_notes {
            out.push_str(&format!("- {note}\n"));
        }
    }
    let answered: Vec<_> = work
        .questions
        .iter()
        .filter_map(|q| q.answer.as_ref().map(|a| (q, a)))
        .collect();
    if !answered.is_empty() {
        out.push_str("\n# Уточнения хозяина\n\n");
        for (question, answer) in answered {
            out.push_str(&format!(
                "- Вопрос: {}\n  Ответ: {}\n",
                question.text, answer
            ));
        }
    }
    if !inputs.is_empty() {
        out.push_str("\n# Входные данные\n\nРезультаты задач, от которых зависит эта:\n");
        for input in inputs {
            out.push_str(&format!("- {}: {}\n", input.title, input.result.display()));
        }
    }
    if let Some(reason) = &work.stuck_reason {
        out.push_str(&format!(
            "\n# Прошлый запуск\n\nПрошлый запуск остановлен: {reason}. Его файлы остались в папке; продолжай с них, а не с нуля.\n"
        ));
    } else if run_dir.join("result.md").is_file() {
        out.push_str("\n# Прошлый запуск\n\nВ папке есть result.md прошлого запуска; учти его.\n");
    }
    out.push_str("\n# Как сдать работу\n\n");
    out.push_str(&format!(
        "- Рабочая папка: {}. Работай только в ней.\n",
        run_dir.display()
    ));
    out.push_str(
        "- Итог положи в result.md: сначала вывод в двух-трёх предложениях, потом подробности.\n",
    );
    out.push_str("- Файлы результата клади в artifacts/.\n");
    if work.risk == Risk::External {
        out.push_str("- Результат уйдёт наружу только после одобрения хозяина: ничего не отправляй и не публикуй сам, готовь черновик.\n");
    }
    out.push_str("- Если не знаешь и не можешь проверить, так и напиши; не выдумывай.\n");
    out.push_str("\n# Память и правила\n\n");
    out.push_str(&format!(
        "- Узнал то, что пригодится следующим задачам отдела (где лежат данные, что не сработало, договорённости) — допиши коротко в {} в рабочей папке. Это память отдела, её прочитают следующие запуски.\n",
        crate::rules::NOTE_FILE
    ));
    out.push_str(&format!(
        "- Считаешь, что отделу нужно постоянное правило, — опиши одно правило коротко в {}. Его добавит хозяин, если согласится; не предлагай то, что уже есть в правилах.\n",
        crate::rules::RULE_FILE
    ));
    let rejected = crate::rules::rejected(&dept.dir, 1024);
    if !rejected.is_empty() {
        out.push_str("- Эти правила хозяин уже отклонил, не предлагай их снова:\n");
        for line in rejected.lines() {
            out.push_str(&format!("  {line}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentsflow_events::{Question, TaskStatus, WorkInfo};

    #[test]
    fn brief_contains_rules_memory_task_and_answers() {
        let dir = std::env::temp_dir().join(format!("agentsflow-brief-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("memory")).unwrap();
        std::fs::write(dir.join("AGENTS.md"), "Пиши коротко.").unwrap();
        std::fs::write(
            dir.join("memory").join("notes.md"),
            format!("{}\nпоследняя заметка\n", "старое\n".repeat(400)),
        )
        .unwrap();
        let dept = Department::parse(crate::department::tests::SAMPLE, &dir).unwrap();
        let task = TaskSnapshot {
            id: "t1".into(),
            title: "Пост".into(),
            description: "Анонс релиза".into(),
            status: TaskStatus::Todo,
            assignee: None,
            progress: 0,
            labels: vec![],
            note: String::new(),
            thread: None,
            created_ms: 0,
            updated_ms: 0,
            work: Some(WorkInfo {
                department: "pr".into(),
                questions: vec![Question {
                    text: "Где?".into(),
                    answer: Some("В Telegram".into()),
                    from: "orchestrator".into(),
                }],
                ..Default::default()
            }),
        };
        let brief = build(&dept, &task, &[], 1, &dir.join("runs").join("t1"));
        assert!(brief.contains("Пиши коротко."));
        assert!(brief.contains("последняя заметка"));
        assert!(brief.contains("не поместилось"));
        assert!(brief.contains("Анонс релиза"));
        assert!(brief.contains("Ответ: В Telegram"));
        assert!(brief.contains("ничего не отправляй"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
