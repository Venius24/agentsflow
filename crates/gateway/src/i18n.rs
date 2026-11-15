use std::sync::atomic::{AtomicU8, Ordering};

mod files_en;
mod screens_en;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Lang {
    Ru,
    En,
}

// Zero means no override; the environment remains the process default.
static OVERRIDE: AtomicU8 = AtomicU8::new(0);

pub(crate) fn default_lang() -> Lang {
    match OVERRIDE.load(Ordering::Relaxed) {
        1 => Lang::Ru,
        2 => Lang::En,
        _ if std::env::var("AGENTSFLOW_LANG").as_deref() == Ok("en") => Lang::En,
        _ => Lang::Ru,
    }
}

#[allow(dead_code)]
pub(crate) fn set_default(lang: Lang) {
    OVERRIDE.store(
        match lang {
            Lang::Ru => 1,
            Lang::En => 2,
        },
        Ordering::Relaxed,
    );
}

const ENGLISH: &[(&str, &str)] = &[
    ("Язык", "Language"),
    ("Язык: русский", "Language: English"),
    ("👥 Команда", "👥 Team"),
    ("🔐 Запросы", "🔐 Requests"),
    ("📋 Доска", "📋 Board"),
    ("✉ Почта", "✉ Mail"),
    ("⚙ Ещё", "⚙ More"),
    ("⛔ Отклонить", "⛔ Deny"),
    ("Разрешить всё равно…", "Allow anyway…"),
    ("✅ Разрешить", "✅ Allow"),
    ("Показать полностью", "Show in full"),
    ("<b>⚙ Ещё</b>", "<b>⚙ More</b>"),
    ("🔔 Звук", "🔔 Unmute"),
    ("🔕 Тишина", "🔕 Mute"),
    ("📊 Статистика", "📊 Statistics"),
    ("⚙ Запуск workflow", "⚙ Run workflow"),
    ("📜 Что делал агент", "📜 Agent activity"),
    ("⬇ Экспорт", "⬇ Export"),
    ("ℹ Помощь", "ℹ Help"),
    ("← Назад", "← Back"),
    ("Команда", "Team"),
    ("Запросы на разрешение", "Permission requests"),
    ("Доска задач", "Task board"),
    ("Новая задача", "New task"),
    ("Результаты и файлы задач", "Task results and files"),
    (
        "Наставник: сильная модель ведёт задачи",
        "Mentor: a strong model leads tasks",
    ),
    ("Почта", "Mail"),
    ("Написать агенту", "Message an agent"),
    ("Карточка агента", "Agent card"),
    ("Остановить агента", "Stop an agent"),
    ("Запустить workflow", "Run workflow"),
    ("Статистика", "Statistics"),
    ("Что делал агент", "Agent activity"),
    ("Экспорт переписки", "Export conversation"),
    ("Тишина", "Mute"),
    ("Включить звук", "Unmute"),
    ("Состояние команды", "Team status"),
    ("Другие действия", "More actions"),
    ("Помощь", "Help"),
    (
        "Отправка заданий из Telegram выключена владельцем",
        "The owner disabled tasks from Telegram",
    ),
    ("Задание устарело", "Task request expired"),
    ("Время ввода истекло", "Input timed out"),
    ("Нет такого агента", "Agent not found"),
    ("Не удалось остановить агента", "Could not stop agent"),
    ("Агент не остановился", "Agent did not stop"),
    ("Не удалось передать задание", "Could not send task"),
    ("Да, отправить", "Yes, send"),
    ("Отмена", "Cancel"),
    ("⏹ Остановить и отправить", "⏹ Stop and send"),
    ("нет доступа", "Access denied"),
    ("Отменено", "Cancelled"),
    (
        "Экран устарел - открыл заново",
        "Screen expired; opened again",
    ),
    ("Не удалось сохранить язык", "Could not save language"),
    ("Нет задач", "No tasks"),
    ("запрос недоступен", "Request unavailable"),
    ("уже решено", "Already resolved"),
    (
        "Точно разрешить опасную команду?",
        "Allow the dangerous command?",
    ),
    ("✅ Да, разрешить", "✅ Yes, allow"),
    ("← Нет", "← No"),
    ("Запрос остаётся открытым", "Request remains open"),
    (
        "Подтвердите опасную команду",
        "Confirm the dangerous command",
    ),
    ("разрешено", "allowed"),
    ("отклонено", "denied"),
    (
        "Введите текст задания длиной до 8000 символов",
        "Enter a task of up to 8000 characters",
    ),
    (
        "Сообщение длиннее 8000 символов",
        "Message exceeds 8000 characters",
    ),
    ("🧭 Принял, разбираю…", "🧭 Got it, working on it…"),
    (
        "Укажите имя агента и текст задания",
        "Enter the agent name and task",
    ),
    (
        "Укажите агента и задание: /ask <имя> <текст>",
        "Enter an agent and task: /ask <name> <text>",
    ),
    ("Тишина включена", "Muted"),
    ("Звук включён", "Unmuted"),
    (
        "Ничего не ждёт вашего решения ✔",
        "Nothing needs your decision ✔",
    ),
    ("остановлено", "stopped"),
    ("не занят", "not busy"),
    ("не удалось остановить", "could not stop"),
    ("нет такого агента", "agent not found"),
    ("Используйте /help", "Use /help"),
    (
        "⚠ У {agent} режим BYPASS: его команды выполняются без запросов и без песочницы. Отправить?",
        "⚠ {agent} uses BYPASS: commands run without permission requests or a sandbox. Send?",
    ),
    ("{agent} сейчас занят.", "{agent} is busy."),
    ("✔ Передано {agent}", "✔ Sent to {agent}"),
    (
        "Что передать {agent}? Следующее сообщение уйдёт ему как задание.",
        "What should {agent} do? Your next message will be sent as a task.",
    ),
    ("Не получилось: {error}", "Could not complete: {error}"),
    (
        "Ожидают вашего решения (карточки присланы выше):\n{list}",
        "Awaiting your decision (cards are above):\n{list}",
    ),
    ("🔴 ошибка", "🔴 error"),
    ("🟡 ждёт вас", "🟡 needs you"),
    ("🟢 работает", "🟢 working"),
    ("⚪ свободен", "⚪ idle"),
    ("⚡ авто", "⚡ auto"),
    ("🧠 умный", "🧠 smart"),
    (
        "<b>⚠ BYPASS</b> (без запросов и песочницы)",
        "<b>⚠ BYPASS</b> (no requests or sandbox)",
    ),
    ("✋ спрашивает", "✋ asks"),
    ("по умолчанию", "default"),
    ("включена", "on"),
    ("выключена", "off"),
    ("\n\nПоследнее:", "\n\nRecent:"),
    ("💬 Написать", "💬 Message"),
    ("📋 Его задачи", "📋 Agent tasks"),
    ("⏹ Стоп", "⏹ Stop"),
    ("🔄 Перезапустить", "🔄 Restart"),
    ("<b>👥 Команда</b>", "<b>👥 Team</b>"),
    ("▸ работает над запросом", "▸ working on a request"),
    ("🔄 Обновить", "🔄 Refresh"),
    ("<b>🔐 Запросы</b>", "<b>🔐 Requests</b>"),
    (
        "Ничего не ждёт вашего решения ✔",
        "Nothing needs your decision ✔",
    ),
    ("\n🔴 ошибка: {error}", "\n🔴 error: {error}"),
    (
        "\n🟡 ждёт вас: {count} запросов",
        "\n🟡 needs you: {count} requests",
    ),
    ("Показать: {agent}", "Show: {agent}"),
    (
        "⚠️ <b>Опасная команда</b> · {safe_agent}\nПравило: <code>{rule}</code> - {reason}\n<pre>{short}</pre>",
        "⚠️ <b>Dangerous command</b> · {safe_agent}\nRule: <code>{rule}</code> - {reason}\n<pre>{short}</pre>",
    ),
    (
        "🔐 <b>{safe_agent}</b> просит разрешение\nКоманда в терминале:\n<pre>{short}</pre>",
        "🔐 <b>{safe_agent}</b> requests permission\nTerminal command:\n<pre>{short}</pre>",
    ),
    (
        "🔐 <b>{safe_agent}</b> просит разрешение\n{tool}: <code>{short}</code>",
        "🔐 <b>{safe_agent}</b> requests permission\n{tool}: <code>{short}</code>",
    ),
    ("⏹ Ход остановлен", "⏹ Turn stopped"),
    ("🛡 Отклонено guard: {rule}", "🛡 Denied by guard: {rule}"),
    ("✅ Разрешено на странице", "✅ Allowed on the page"),
    ("⛔ Отклонено на странице", "⛔ Denied on the page"),
    ("Запрос закрыт", "Request closed"),
    ("✅ Разрешено вами", "✅ Allowed by you"),
    ("⛔ Отклонено вами", "⛔ Denied by you"),
    (
        "⚠️ guard остановил <b>{agent}</b>: <code>{command}</code> ({reason})",
        "⚠️ guard stopped <b>{agent}</b>: <code>{command}</code> ({reason})",
    ),
];

pub(crate) fn tr(lang: Lang, ru: &'static str) -> &'static str {
    if lang == Lang::En {
        ENGLISH
            .iter()
            .chain(screens_en::TABLE)
            .chain(files_en::TABLE)
            .find(|(key, _)| *key == ru)
            .map_or(ru, |(_, value)| *value)
    } else {
        ru
    }
}

/// Insert named values without interpreting placeholders in the supplied data.
pub(crate) fn render(lang: Lang, ru: &'static str, values: &[(&str, &str)]) -> String {
    let template = tr(lang, ru);
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start + 1..].find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let end = start + 1 + end;
        let name = &rest[start + 1..end];
        if let Some((_, value)) = values.iter().find(|(key, _)| *key == name) {
            out.push_str(value);
        } else {
            out.push_str(&rest[start..=end]);
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

#[allow(dead_code)]
pub(crate) fn plural<'a>(lang: Lang, n: u64, ru: [&'a str; 3], en: [&'a str; 2]) -> &'a str {
    match lang {
        Lang::En => en[usize::from(n != 1)],
        Lang::Ru if n % 10 == 1 && n % 100 != 11 => ru[0],
        Lang::Ru if (2..=4).contains(&(n % 10)) && !(12..=14).contains(&(n % 100)) => ru[1],
        Lang::Ru => ru[2],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn translations_and_fallback() {
        assert_eq!(tr(Lang::Ru, "Язык"), "Язык");
        assert_eq!(tr(Lang::En, "Язык"), "Language");
        assert_eq!(tr(Lang::En, "unknown"), "unknown");
    }
    #[test]
    fn plurals() {
        let ru = ["one-ru", "few-ru", "many-ru"];
        let en = ["one", "many"];
        for (n, expected) in [
            (1, "one-ru"),
            (2, "few-ru"),
            (5, "many-ru"),
            (11, "many-ru"),
            (21, "one-ru"),
            (112, "many-ru"),
        ] {
            assert_eq!(plural(Lang::Ru, n, ru, en), expected);
        }
        assert_eq!(plural(Lang::En, 1, ru, en), "one");
        assert_eq!(plural(Lang::En, 0, ru, en), "many");
        assert_eq!(plural(Lang::En, 2, ru, en), "many");
    }
}
