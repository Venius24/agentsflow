# agentsflow

Лёгкий шлюз для событий агентов Claude Code и Codex.

Поток: `Claude Code / Codex -> адаптеры -> события -> консоль`

Структура проекта:
- `crates/events` — общий формат событий.
- `crates/adapters` — адаптеры Claude Code и Codex.
- `crates/gateway` — консольный шлюз.
- `web` — статическая страница с двумя колонками.
- `.github/workflows` — проверки CI.

Сборка: `cargo build`.
Тесты: `cargo test`.

Для запуска нужны настроенные CLI соответствующих агентов:
- `cargo run -p agentsflow-gateway -- claude "Привет" --model haiku`
- `cargo run -p agentsflow-gateway -- codex "Привет" --sandbox read-only`

Путь к Codex можно задать через `--codex-bin` или переменную `CODEX_BIN`.
Флаг `--yes` автоматически разрешает запросы инструментов; без него требуется подтверждение.
## Локальный сервер

Запуск: `cargo run -p agentsflow-gateway -- serve --port 7878`.
Откройте в браузере URL с токеном, напечатанный сервером. `--port 0` выбирает свободный порт.
Доступны `--cwd`, `--agents claude,codex`, `--claude-model`, `--codex-model`, `--sandbox`, `--codex-bin` и `--yes`.
`GET /api/events` отдаёт историю и новые события через SSE; `GET /api/state` возвращает состояние агентов.
`POST /api/prompt` принимает `agent` и `text`; `POST /api/permission` — `agent`, `id` и `allow`.
Все API требуют токен в query `token` или заголовке `x-agentsflow-token`.
Сервер слушает только 127.0.0.1, проверяет Host, Origin для POST и защищает API токеном.

Статус: **stage 3** — локальный сервер для Claude Code и Codex.
