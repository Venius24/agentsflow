//! English texts of telegram/files.rs, keyed by the Russian text.

pub(crate) const TABLE: &[(&str, &str)] = &[
    (
        "В ветке задачи пока нет коммитов.",
        "The task branch has no commits yet.",
    ),
    (
        "Копия репозитория задачи — ссылка или без своей папки .git, её не читаю.",
        "The task repository copy is a link or has no .git directory of its own; it is not read.",
    ),
    ("git не показал правки: ", "git could not show changes: "),
    (
        "Ветка {branch} против {base}, только закоммиченное.",
        "Branch {branch} against {base}, committed changes only.",
    ),
    ("задачи", "task"),
    ("базы", "base"),
    (
        "Правки из ленты агентов, каждая отдельно и по порядку.",
        "Changes from the agent feed, separately and in order.",
    ),
    ("Правок нет.", "No changes."),
    (
        "Показано не всё: правки больше 2 МБ.",
        "Not all changes are shown: they exceed 2 MB.",
    ),
    (
        "файл больше 20 МБ, бот не может его скачать",
        "file exceeds 20 MB; the bot cannot download it",
    ),
    (
        "Telegram не отдал файл",
        "Telegram did not provide the file",
    ),
    ("файл не скачался", "file download failed"),
    ("неверный путь", "invalid path"),
    ("не записать файл: {error}", "could not write file: {error}"),
    ("файл скачался не до конца", "file download was incomplete"),
    (
        "<b>🧾 Правки</b> «{title}»\n{source}\n",
        "<b>🧾 Changes</b> «{title}»\n{source}\n",
    ),
    (" · двоичный", " · binary"),
    (" · новый", " · new"),
    (" · удалён", " · deleted"),
    ("\n… и ещё {count}", "\n… and {count} more"),
    ("{bytes} Б", "{bytes} B"),
    ("{size} КБ", "{size} KB"),
    ("{size} МБ", "{size} MB"),
    (
        "Файлы принимаются в режиме отделов (serve --home)",
        "Files are accepted in department mode (serve --home)",
    ),
    (
        "Отправка заданий из Telegram выключена владельцем",
        "The owner disabled tasks from Telegram",
    ),
    (
        "Агенту можно написать только текстом",
        "Only text can be sent to an agent",
    ),
    (
        "Здесь нужен текст, файл сюда не приложить",
        "Text is required here; a file cannot be attached",
    ),
    (
        "За один раз не больше {MAX_BATCH} файлов, этот не принят",
        "At most {MAX_BATCH} files at a time; this file was not accepted",
    ),
    (
        "Не принял «{name}»: {error}",
        "Could not accept «{name}»: {error}",
    ),
    (
        "Сообщение длиннее 8000 символов",
        "Message exceeds 8000 characters",
    ),
    (
        "Файлы не приняты, сообщение не отправлено: {text}",
        "Files were not accepted; message not sent: {text}",
    ),
    (
        "📎 Приложил к задаче: {names}. Теперь напишите текст.",
        "📎 Attached to the task: {names}. Now write a message.",
    ),
    (
        "Не приложил файлы: {error}",
        "Could not attach files: {error}",
    ),
    (
        "🧭 Принял вместе с файлами ({names}), разбираю…",
        "🧭 Received with files ({names}), working on it…",
    ),
    ("Не получилось: {error}", "Could not complete: {error}"),
    (
        "📎 Файлы ({names}) убраны: задачу к ним не написали",
        "📎 Files ({names}) removed: no task was written for them",
    ),
    (
        "📎 Получил: {names}\n\nНапишите, что с этим сделать, и всё уйдёт оркестратору.",
        "📎 Received: {names}\n\nWrite what to do with these files and everything will be sent to the orchestrator.",
    ),
    ("Отмена", "Cancel"),
    ("Файлы убраны", "Files removed"),
    ("📖 Читать итог", "📖 Read result"),
    ("📎 Файлы · {artifacts}", "📎 Files · {artifacts}"),
    ("🧾 Правки", "🧾 Changes"),
    (
        "Правки есть только в режиме отделов (serve --home)",
        "Changes are available only in department mode (serve --home)",
    ),
    ("Задачи нет", "Task not found"),
    ("Не удалось собрать правки", "Could not collect changes"),
    ("Правки {title}.diff", "Changes {title}.diff"),
    ("Не удалось подготовить файл", "Could not prepare file"),
    ("🧾 Правки отправлены", "🧾 Changes sent"),
    ("Не удалось прислать правки", "Could not send changes"),
    ("Файлов нет", "No files"),
    (
        "<b>📎 Файлы задачи</b>\n«{title}»",
        "<b>📎 Task files</b>\n«{title}»",
    ),
    (
        "\n\nОтдел файлов не оставил.",
        "\n\nThe department left no files.",
    ),
    (
        "\n\n📖 откроется здесь, ⬇ придёт файлом.",
        "\n\n📖 opens here, ⬇ sends a file.",
    ),
    ("← К задаче", "← Back to task"),
    ("Файла нет", "File not found"),
    (
        "Файла уже нет, откройте список заново",
        "File no longer exists; reopen the list",
    ),
    (
        "Telegram не показал страницу",
        "Telegram could not show the page",
    ),
    ("⬇ Скачать", "⬇ Download"),
    ("📎 Все файлы", "📎 All files"),
    (
        "Файл больше 50 МБ, Telegram его не примет. Он лежит здесь:\n<code>{path}</code>",
        "File exceeds 50 MB; Telegram cannot accept it. Find it here:\n<code>{path}</code>",
    ),
    ("Не удалось прислать файл", "Could not send file"),
    ("⬇ Отправляю файл", "⬇ Sending file"),
    (
        "Результаты задач есть только в режиме отделов (serve --home)",
        "Task results are available only in department mode (serve --home)",
    ),
    (
        "<b>📂 Результаты</b>\n\nПока пусто: ни одна задача не оставила итога.",
        "<b>📂 Results</b>\n\nNo task results yet.",
    ),
    (
        "<b>📂 Результаты</b>\n\nНедавние задачи, новые сверху. Нажмите, чтобы читать.",
        "<b>📂 Results</b>\n\nRecent tasks, newest first. Tap to read.",
    ),
];
