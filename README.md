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
Статус: **stage 2** — оба агента выводят события в консоль; сервер ещё не реализован.
