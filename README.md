# agentsflow

Лёгкий шлюз для событий агентов Claude Code и Codex.

Планируемый поток:
`Claude Code / Codex -> адаптеры -> события -> локальный сервер -> веб-страница`

Структура проекта:
- `crates/events` — общий формат событий.
- `crates/adapters` — адаптер Claude Code.
- `crates/gateway` — консольный шлюз.
- `web` — статическая страница с двумя колонками.
- `.github/workflows` — проверки CI.

Сборка: `cargo build`.
Тесты: `cargo test`.
Запуск (требуется настроенный Claude CLI): `cargo run -p agentsflow-gateway -- claude "Привет" --model haiku`.
Для автоматического разрешения всех запросов инструментов добавьте `--yes`.

Статус: **stage 1** — события Claude Code выводятся в консоль; сервер и адаптер Codex ещё не реализованы.
