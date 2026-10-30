# agentsflow

Лёгкий шлюз для агентов Claude Code и Codex: оба запускаются как обычные CLI-процессы, их события приводятся к одному формату и показываются на локальной веб-странице. Поверх этого работает небольшой оркестратор: агенты передают друг другу результаты по сценарию (workflow), а их переписка видна в интерфейсе.

```
 claude CLI ──(claude-codes)──┐                         ┌── SSE ──► web: [Claude | Codex | Оркестратор]
                              ├─► Hub (seq, история) ───┤
 codex CLI  ──(codex-codes)───┘        ▲                └◄─ POST /api/prompt, /api/permission, /api/workflow/*
                                       │
                               Workflow-движок (шаги, handoff, циклы ревью)
```

Весь код на Rust, страница — один HTML-файл без сборки и внешних зависимостей.

## Быстрый старт

Нужны Rust (stable), установленные и авторизованные CLI `claude` и `codex`.

```sh
cargo run -p agentsflow-gateway -- serve --cwd path/to/project
```

Сервер печатает одну строку вида `agentsflow: http://127.0.0.1:7878/?token=…`. Откройте её в браузере.

- Слева Claude, посередине Codex: у каждого своя лента и поле промпта.
- Запросы разрешений (запуск команды, запись файла) приходят карточкой с кнопками «Разрешить» / «Отклонить». Диффы показываются до решения.
- Справа оркестратор: выберите workflow, задайте задачу и запустите.

По умолчанию Codex работает в песочнице `read-only` и спрашивает разрешение на каждую запись. Для автономной работы: `--sandbox workspace-write`; чтобы не отвечать вручную — `--yes` (автоматически разрешает всё, используйте осознанно).

## Режимы запуска

| Команда | Что делает |
|---|---|
| `agentsflow serve [--port N] [--cwd DIR] [--agents claude,codex] [--workflow FILE]…` | Сервер и веб-страница. `--port 0` — свободный порт. |
| `agentsflow claude "<prompt>" [--model M] [--cwd DIR] [--yes]` | Один ход Claude, события в консоль, разрешения через `y/N`. |
| `agentsflow codex "<prompt>" [--sandbox read-only\|workspace-write] [--codex-bin PATH] …` | То же для Codex. |
| `agentsflow flow <file.toml> --task "<text>" --yes [опции]` | Headless-запуск workflow. `--yes` обязателен: отвечать на разрешения некому. Код выхода 0 — workflow завершён, 1 — провален, 2 — ошибка аргументов. |

Общие опции: `--cwd`, `--claude-model`, `--codex-model` (для `serve`/`flow`), `--sandbox`, `--codex-bin`. Бинарь `codex` ищется так: `--codex-bin`, переменная `CODEX_BIN`, установленное приложение (Windows), затем `PATH`.

## Единый формат событий

Каждое событие — `Envelope { session, agent, seq, ts_ms, event }`, где `agent` — `claude`, `codex` или `orchestrator`. Варианты `event` (поле `type`):

`message` · `tool_call` · `tool_result` · `diff` · `permission_request` · `permission_resolved` · `done` · `error` · `handoff` · `workflow`

Диффы Codex приходят из его `fileChange`; для Claude патч синтезируется из `Write`/`Edit`/`MultiEdit` и показывается раньше запроса разрешения.

## HTTP API

Все `/api/*` требуют токен: query `?token=…` или заголовок `x-agentsflow-token`.

| Метод и путь | Назначение |
|---|---|
| `GET /api/events` | SSE: сначала вся история (до 5000 событий), затем новые. `id` = `seq`. |
| `POST /api/prompt` `{agent, text}` | Промпт агенту. 202 / 409 (занят) / 404 / 400. |
| `POST /api/permission` `{agent, id, allow}` | Ответ на запрос разрешения. |
| `GET /api/state` | Рабочая папка, состояние агентов, текущий workflow. |
| `GET /api/workflows` | Список доступных workflow. |
| `POST /api/workflow/run` `{name, task}` | Запуск. 202 / 404 / 400 / 409 (уже идёт). |
| `POST /api/workflow/stop` | Остановка между шагами. Текущий ход агента прервать нельзя, он просто не ожидается. |

Безопасность: сервер слушает только `127.0.0.1`, проверяет заголовок `Host` (защита от DNS rebinding) и `Origin` у POST, токен генерируется при каждом запуске. Страница может одобрять выполнение команд, поэтому не пробрасывайте порт наружу.

## Workflow

Сценарий — TOML-файл. Шаги идут по порядку; результат шага можно подставить в промпт следующего.

```toml
name = "implement-and-review"
description = "Реализация и проверка с циклом исправлений"
max_steps = 20                      # предохранитель, по умолчанию 20

[[step]]
id = "implement"
agent = "codex"                     # claude | codex
prompt = "{task}"

[[step]]
id = "review"
agent = "claude"
prompt = "Задача:\n{task}\n\nОтчёт исполнителя:\n{prev.output}\n\nЕсли всё верно — последней строкой напиши VERDICT: APPROVED, иначе перечисли проблемы."
until = "VERDICT: APPROVED"         # последняя непустая строка вывода должна быть ровно такой
otherwise = "fix"                   # иначе перейти на шаг fix…
max_loops = 2                       # …не более 2 раз (по умолчанию 2), затем workflow Failed
next = "end"                        # при успехе завершить

[[step]]
id = "fix"
agent = "codex"
prompt = "Замечания ревьюера:\n{review.output}\n\nИсправь их."
next = "review"
```

- Плейсхолдеры: `{task}`, `{<id>.output}` (последний вывод шага) и `{prev.output}` (вывод предыдущего выполненного шага с учётом циклов).
- Выводом шага считается последнее сообщение агента за ход.
- `until` проверяет не подстроку, а вердикт: он засчитывается, только если это вся последняя непустая строка вывода (пробелы и markdown-выделение вокруг игнорируются). «NOT VERDICT: APPROVED» или вердикт, за которым идут замечания, не проходят. Берите редкий маркер, который ревьюер не станет писать в замечаниях.
- Когда промпт строится из чужого вывода, оркестратор публикует `handoff { from, to, step, text }`: на странице это карточка «claude → codex» и пометка «← claude · fix» у промпта в колонке получателя.
- Ошибки спецификации (дубликаты id, неизвестные шаги и плейсхолдеры, `otherwise` без `until`, неизвестные ключи) ловятся при загрузке.
- Встроены `implement-and-review` и `cross-check`; свои файлы подключаются `serve --workflow FILE` (можно несколько раз).
- Одновременно выполняется один workflow.

## Разработка

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test -p agentsflow-adapters -- --ignored   # живые тесты, нужны claude и codex
```

CI (GitHub Actions) выполняет те же проверки на Linux и Windows и проверяет синтаксис скрипта страницы.

Структура:
- `crates/events` — формат событий (`Envelope`, `Event`).
- `crates/adapters` — трейт `Agent`; `claude` (на `claude-codes`) и `codex` (на `codex-codes`).
- `crates/gateway` — `hub` (шина событий и актор на агента), `server` (axum), `workflow` (движок), `runtime`, `cli`.
- `web/index.html` — интерфейс. `workflows/` — встроенные сценарии.

## Ограничения

- Протоколы CLI нестабильны: крейты привязаны к версиям `claude` 2.1.x и `codex` 0.16x.
- Сообщения приходят целиком, без токенового стриминга.
- Агент не прерывается на лету: остановка workflow и завершение сервера корректны, но выполняющийся ход нельзя отменить (при завершении сервера процессы агентов убиваются).
- Одна рабочая папка и по одной долгоживущей сессии на агента.

## Лицензия

MIT, см. файл [LICENSE](LICENSE).
