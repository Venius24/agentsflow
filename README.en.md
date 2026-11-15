# agentsflow

[Русский](README.md) · **English**

A lightweight gateway for a team of Claude Code and Codex agents. Each team member runs as its own CLI process, and all their events are converted to one common format. The orchestrator manages workflows and the mail between members.

```
 CLI members ── adapters ──► Hub (seq, history) ── SSE ──► web: member and orchestrator columns
                                   ▲                  ◄── POST /api/prompt, /api/permission, /api/workflow/*
                                   │
                            Workflow engine (steps, handoff, review loops)
```

The whole codebase is Rust. The web page is a single HTML file with no build step and no external dependencies.

> **Note on the interface language.** The web page and the Telegram bot are in Russian only for now. In this document, interface labels are quoted in Russian with an English gloss the first time they appear, for example "Разрешить" ("Allow"). The documentation, configuration files and API are in English or language-neutral.

**Departments mode** (`serve --home`): you write tasks as plain text in Telegram, an AI orchestrator distributes them among departments, each department works in fresh sessions under its own rules and memory, the orchestrator accepts internal results, and you accept anything that goes outside. See [docs/departments.en.md](docs/departments.en.md) for how it works and [docs/vps.en.md](docs/vps.en.md) for server installation.

## Install

Pick one of three ways. All of them need the `claude` and `codex` CLIs installed and signed in (see [Quick start](#quick-start)); the installer does not install them.

**1. Install script (Linux, macOS, Windows).** It downloads the latest GitHub release for your platform, verifies the SHA-256 checksum, and puts `agentsflow` in a user directory. It does not use `sudo` and does not change your `PATH` or any system setting; it only tells you what to do.

```sh
# Linux and macOS: installs to ~/.local/bin
curl -fsSL https://raw.githubusercontent.com/Venius24/agentsflow/master/install.sh | sh
```

```powershell
# Windows (PowerShell): installs to %LOCALAPPDATA%\agentsflow\bin
irm https://raw.githubusercontent.com/Venius24/agentsflow/master/install.ps1 | iex
```

If you would rather read the script before running it, download `install.sh` or `install.ps1` from the repository root and run it from disk. Both scripts accept a version (`AGENTSFLOW_VERSION=v0.1.0` for `install.sh`; `-Version v0.1.0` or the `AGENTSFLOW_VERSION` variable for `install.ps1`) and a target directory (`AGENTSFLOW_INSTALL_DIR`, or `-InstallDir` for `install.ps1`). If no release has been published yet, the script stops with a message; use the source build below until the first release appears.

**2. From a release archive.** Download the archive for your platform and `SHA256SUMS` from the [Releases](https://github.com/Venius24/agentsflow/releases) page.

| Platform | Archive |
|---|---|
| Linux x86_64 (glibc) | `agentsflow-x86_64-unknown-linux-gnu.tar.gz` |
| macOS, Apple silicon | `agentsflow-aarch64-apple-darwin.tar.gz` |
| macOS, Intel | `agentsflow-x86_64-apple-darwin.tar.gz` |
| Windows x86_64 | `agentsflow-x86_64-pc-windows-msvc.zip` |

Check the checksum (`sha256sum -c --ignore-missing SHA256SUMS` on Linux, `shasum -a 256 -c --ignore-missing SHA256SUMS` on macOS, `Get-FileHash` on Windows), unpack the archive, and put the `agentsflow` binary in a directory that is on your `PATH`.

**3. From source.** You need Rust (stable, 1.85 or newer) and a C compiler. TLS is provided by rustls, so OpenSSL is not required.

```sh
git clone https://github.com/Venius24/agentsflow
cd agentsflow
cargo build --release      # the binary is target/release/agentsflow
```

After installing, check the version and the environment:

```sh
agentsflow --version
agentsflow doctor
```

If `agentsflow` is not found, the install directory is not on your `PATH`. The install script prints the exact line to add for your shell.

## Quick start

You need the `claude` and `codex` CLIs, installed and signed in. Rust is needed only if you build from source.

```sh
agentsflow serve --cwd path/to/project
# from a source checkout, without installing:
cargo run -p agentsflow-gateway -- serve --cwd path/to/project
```

Environment check: `agentsflow doctor` reports whether `git`, `claude` and `codex` are found and start. `agentsflow doctor --live` additionally gives each agent one short task (a few tokens) and catches changes in the CLI output format; the test `crates/adapters/tests/claude_format.rs` checks the same thing against a recorded real Claude turn. CI has no real CLIs, so run the live check after updating `claude` or `codex`.

The server prints one line like `agentsflow: http://127.0.0.1:7878/?token=…`. Open it in a browser.

- Each team member has its own column with a feed, settings and a prompt field; the orchestrator is on the right. If the columns do not fit, the container scrolls horizontally.
- Permission requests (running a command, writing a file) arrive as a card with "Разрешить" ("Allow") and "Отклонить" ("Deny") buttons. Diffs are shown before you decide.
- On the right is the orchestrator: pick a workflow, enter a task and start it.

By default Codex runs in the `read-only` sandbox and asks permission for every write. For autonomous work use `--sandbox workspace-write`; to stop answering by hand use `--yes` (it automatically allows everything, so use it deliberately).

## Run modes

| Command | What it does |
|---|---|
| `agentsflow serve [--port N] [--cwd DIR] [--team pair\|trio\|FILE] [--allow-bypass] [--workflow FILE]…` | Server and web page. `--port 0` picks a free port. |
| `agentsflow serve --home DIR [--telegram]` | Departments mode: orchestrator, task queue, schedules ([docs/departments.en.md](docs/departments.en.md)). |
| `agentsflow home init DIR` / `home check [DIR]` | Create a home with a common department, or check one. |
| `agentsflow watch --home DIR [--once]` | Gateway watchdog: polls `/api/alive`, writes to Telegram if the gateway goes silent, and kills a hung process so autostart brings up a new one ([docs/departments.en.md](docs/departments.en.md#gateway-watchdog)). |
| `agentsflow team init [FILE]` / `team check <pair\|trio\|FILE>` / `team show` | Create a configuration, check a team, or show the presets. |
| `agentsflow claude "<prompt>" [--model M] [--cwd DIR] [--yes]` | One Claude turn, events printed to the console, permissions answered with `y/N`. |
| `agentsflow codex "<prompt>" [--sandbox read-only\|workspace-write] [--codex-bin PATH] …` | The same for Codex. |
| `agentsflow flow <file.toml> --task "<text>" --yes [options]` | Headless workflow run. `--yes` is required because nobody is there to answer permission requests. Exit code 0: workflow finished; 1: failed; 2: argument error. |

Common options: `--cwd`, `--claude-model`, `--codex-model` (for `serve`/`flow`), `--sandbox`, `--codex-bin`. The `codex` binary is looked up in this order: `--codex-bin`, the `CODEX_BIN` variable, the installed app (Windows), then `PATH`.

## Agent team

The default is `pair`: members `claude` and `codex`. The `trio` preset has `lead` (Claude Sonnet 5.5, high), `main` (Terra, medium) and `junior` (Terra, low). For three members, `agentsflow serve --team trio --mail-thread-limit 40` is recommended.

`agentsflow team init [FILE]` creates a `team.toml` template without overwriting an existing file. `agentsflow team check FILE` validates it and prints a table; `agentsflow team show` shows the presets. An example of your own file:

```toml
[[agent]]
id = "lead"
kind = "claude"
model = "claude-sonnet-5-5"
effort = "high"
title = "lead and reviewer"
roles = ["lead", "reviewer"]
lead = true

[[agent]]
id = "main"
kind = "codex"
roles = ["engineer"]
```

Start it with `agentsflow serve --team team.toml`. A team has 1 to 6 members and exactly one lead; IDs consist of lowercase Latin letters, digits, `_` and `-`. In mail, members are addressed by ID; in workflows, by ID or role. The old `--agents claude,codex` flag filters `pair` and cannot be combined with `--team`. A model set in a member's settings takes priority over the global model flag.

The access mode is set per member:

| Field | Values and inheritance |
|---|---|
| `mode` | `ask`: ask about everything. `auto`: allow everything, and the guard silently denies dangerous commands. `smart`: allows harmless requests itself and sends you the commands the guard flagged (a red card with double confirmation, on the page and in Telegram). `bypass`: skips permissions and the Codex sandbox. The `pair` and `trio` presets set `smart`. If the field is not set, `auto` is used with `--yes`, otherwise `ask`. The guard is a list of known dangerous commands, not complete protection: for example, it does not see a download of someone else's script that is then run. With `guard = false`, `smart` allows everything. |
| `sandbox` | Codex only: `read-only` or `workspace-write`. Without the field it inherits `--sandbox`, otherwise `read-only`. The field is not allowed together with `bypass`. |

In the `trio` preset, `main` and `junior` use `workspace-write` and inherit the mode. To use bypass you must confirm the launch explicitly with `--allow-bypass` on `serve` or `flow`; `team check` prints a warning. **Bypass removes permission checks, and for Codex the sandbox as well:** use it only when you trust both the task and the environment. An example of a member without a sandbox:

```toml
[[agent]]
id = "worker"
kind = "codex"
lead = true
mode = "bypass"
```

Start it with `agentsflow serve --team team.toml --allow-bypass`. The global `--sandbox` does not accept `danger-full-access`.

The web page gets the team roster from `/api/state` and shows columns in configuration order. Events from old journals for unknown IDs appear in separate neutral columns.

## Common event format

Every event is an `Envelope { session, agent, seq, ts_ms, event }`, where `agent` is a member ID or `orchestrator`. The variants of `event` (field `type`) are:

`message` · `thinking` · `tool_call` · `tool_result` · `diff` · `permission_request` · `permission_resolved` · `done` · `error` · `handoff` · `workflow` · `note`

`thinking` is what the agent thought through before acting. Claude sends a short summary of its thoughts (CLI flag `--thinking-display summarized`); Codex sends reasoning headings (`model_reasoning_summary = "detailed"`). Thoughts arrive only when the model decided to think, so a simple task may have none. `AGENTSFLOW_THINKING=off` stops requesting them; this is needed if a new Claude CLI version drops the undocumented flag. `note` is a note from the owner to a department agent and what became of it: `queued`, `delivered`, `dropped`.

Codex diffs come from its `fileChange`. For Claude, a patch is synthesized from `Write`/`Edit`/`MultiEdit` and shown before the permission request.

The journal is on by default: `serve` appends events to `.agentsflow/events.jsonl` inside `--cwd` (the folder is in `.gitignore`). `--log FILE` sets another file and `--no-log` turns writing off. The journal holds the whole conversation and all commands in plain text, so do not send it to others. After a restart the last 5000 events are restored. Anything the previous run did not finish (a permission request, an agent turn, a workflow) is closed with events marked `interrupted: server restarted`, so the page does not show dead cards.

## Session export

The "Экспорт" ("Export") buttons download the current history as Markdown or JSONL.
The route `GET /api/export?format=md|jsonl` returns Markdown by default.
Export is available only with the main token; agents' personal tokens do not work.

## HTTP API

All `/api/*` routes require a token: the query parameter `?token=…` or the `x-agentsflow-token` header. There is one exception: `GET /api/alive` answers without a token and says only `{ok, version}`, so the watchdog can check the gateway even if it has no access to the token; the `Host` check still applies to it.

| Method and path | Purpose |
|---|---|
| `GET /api/events` | SSE: first the whole history (up to 5000 events), then new ones. `id` = `seq`. |
| `POST /api/prompt` `{agent, text}` | Send a prompt to an agent. 202 / 409 (busy) / 404 / 400. |
| `POST /api/agent/interrupt` `{agent}` | Interrupt a member's current turn. 200 / 404 / 409 (not busy); main token only. |
| `POST /api/agent/restart` `{agent}` | Stop the turn and restart the session. 200 / 404 / 409; main token only. |
| `POST /api/permission` `{agent, id, allow}` | Answer a permission request. |
| `GET /api/state` | Working folder, agent states, current workflow. |
| `GET /api/projects` | The current project and the available projects; main token only. |
| `POST /api/project/switch` `{path}` | Switch project after the team has stopped working; main token only. |
| `GET /api/health` | Version, uptime and member states; main token only. |
| `GET /api/team` | Team members in configuration order. |
| `GET /api/workflows` | List of available workflows. |
| `POST /api/workflow/run` `{name, task}` | Start. 202 / 404 / 400 / 409 (already running). |
| `POST /api/workflow/stop` | Stop between steps. The agent's current turn cannot be interrupted this way; it is just no longer awaited. |

Security: the server listens only on `127.0.0.1`, checks the `Host` header (protection against DNS rebinding) and the `Origin` of POST requests, and generates a new token on every start. The page can approve command execution, so do not forward the port to the outside.

## Stopping a turn

The "Стоп" ("Stop") button in a member's column interrupts its current turn and keeps the session for the next prompt.
The route `POST /api/agent/interrupt` takes `{agent}` and returns 200 on success, 404 for an unknown member and 409 `not busy` if the member is idle.
An agent's last error is available as `last_error` in `/api/state` and `/api/health`, on the page and in Telegram; a successful turn or a restart clears it. Server diagnostics are written to `.agentsflow/server.log` in the working folder; at startup a file larger than 5 MiB is moved to `server.log.1`.

The route is available only with the main token; an agent's personal token gets 403.
Pending permission requests are denied, and the turn ends with a `done` event with the reason `interrupted`.
A workflow step waiting for that turn fails with the error `interrupted`; mail steps are not affected.

If you start `serve --projects DIR`, you can choose a project on the page or in Telegram. The list contains the immediate subfolders that have `.git`, plus the current working folder; the flag can be repeated for several roots. Stop all agents and workflows first. Each project has its own history and board in `<project>/.agentsflow/`; the port and the main token are kept when you switch.

Example: `.\serve-telegram.ps1 --team trio --projects F:\Dev --cwd F:\Dev\project`.

## Protection against destructive commands (guard)

The guard checks the command text in permission requests for shell tools. It is on by default for every member; `guard = false` in the member's `[[agent]]` block turns the check off. In `auto`, a dangerous command is denied automatically; in `ask`, the card shows a warning and a human decides. In `bypass`, permission requests are skipped, so the guard does not work. When it denies automatically, Claude receives the reason; the Codex response protocol passes on only the denial.

| Rule | Examples |
|---|---|
| `rm-recursive` | Recursive deletion of the project root, `.git`, or a path outside the allowed directories. Project subfolders and temporary directories are allowed. |
| `git-reset-hard`, `git-clean`, `git-force-push`, `git-discard` | Loss of changes, history rewriting, or deletion of Git references. |
| `disk` | Disk formatting and overwriting. |
| `sql` | Destructive SQL commands passed to a database client. |
| `perm` | Recursive permission changes on the filesystem root. |

The check parses commands and known wrappers approximately, does not run any processes, and is not a sandbox. Complex constructs, obfuscation, unknown wrappers and actions outside the shell can go unnoticed; anything that cannot be parsed is not blocked. Paths are checked lexically, without resolving symbolic links. The warning is visible on the permission card and in the Markdown export.

## Telegram

The bot defaults to Russian. Set `AGENTSFLOW_LANG=en` for English or `AGENTSFLOW_LANG=ru` for Russian; `/lang` switches the language for the current chat and remembers it across restarts.

Create a bot with @BotFather and give its token only through the environment variable `AGENTSFLOW_TELEGRAM_TOKEN`. In `AGENTSFLOW_TELEGRAM_USERS`, list the numeric IDs of the users you trust, separated by commas; you can find your own ID with @userinfobot. Start `agentsflow serve --telegram`. The optional `AGENTSFLOW_TELEGRAM_API` changes the Bot API address for testing.

The bridge shows the team and the state of agents, and sends permission requests with buttons, workflow results, mail to the orchestrator that was submitted for review, stuck tasks, errors, and the end of long turns. Dangerous commands require separate confirmation; BYPASS requires confirmation before the first task.

Telegram can send tasks to agents and answer permission requests. **If the owner's Telegram account is hacked, the attacker can control the owner's computer.** Give access only to people you are ready to allow such actions, and enabling Telegram two-factor authentication is recommended. To turn off sending tasks, set `AGENTSFLOW_TELEGRAM_PROMPTS=0`. Access is limited by an allowlist, and the token is not passed as a launch argument.

Everything on the page is also available in the bot, under the same names. At the bottom of the chat there are permanent buttons "Команда" ("Team"), "Запросы" ("Requests"), "Доска" ("Board"), "Почта" ("Mail") and "Ещё" ("More").

| Capability | Page | Telegram |
|---|---|---|
| Team and agent state | "Обзор" ("Overview"), chats | "Команда" → agent, `/team`, `/status`, `/agent <name>` |
| Task for an agent | Input field in the column | "Написать" ("Write"), `/ask <name> <text>` |
| Stop a turn | "Стоп" | ⏹ on the agent card, `/stop <name>` |
| Permissions and dangerous commands | Card in "Ждёт вас" ("Waiting for you") | Notification with buttons, "Запросы", `/requests` |
| Board: list by column | "Доска" | "Доска", `/board`: status tabs with counters and pages |
| Create a task | "Новая карточка" ("New card") | "＋ Новая задача" ("＋ New task") or `/task <title> \| <description>` |
| Card: status, assignee, title, description | Card fields | Buttons "Статус" ("Status"), "Исполнитель" ("Assignee"), "Название" ("Title"), "Описание" ("Description") |
| Start, accept, return, archive | Card buttons | The same buttons; accept and archive ask for confirmation |
| A silent department run | A "молчит N мин" ("silent for N min") card in "Ждёт вас": wait, restart the step, stop | A "🔇 молчит" ("🔇 silent") card with the same buttons |
| Closing comment on GitHub | In the task card: "Опубликовать комментарий" ("Post the comment") / "Не нужно" ("Not needed") | A "💬 опубликовать комментарий?" ("💬 post the comment?") card |
| Dragging a card | With the mouse | Not available: the "Статус" menu replaces it |
| Mail: list, thread, reply, new message | "Почта" | "Почта", `/mail`, `/mail <agent> <subject> \| <text>` |
| Start and stop a workflow | "Workflow" | "Ещё" → "Запуск workflow" ("Run workflow"), `/run`; ⏹ in the live message |
| Statistics | "Статистика" ("Statistics") | "Ещё" → "Статистика", `/stats` |
| What the agent did | Journal in the column | "Ещё" → "Что делал агент" ("What the agent did"), `/log <name> [n]` |
| Export | Export button | "Ещё" → "Экспорт", `/export md\|jsonl`; the file goes through Telegram's servers, so the bot asks you to confirm once |
| Silence | Not available | `/mute`, `/unmute`, "Ещё" |
| Page token and access link | The address in the console | Not available: the link stays on the computer |

Full list of commands: `/team`, `/requests`, `/board`, `/task`, `/mail`, `/ask`, `/agent`, `/stop`, `/run`, `/stats`, `/log`, `/export`, `/mute`, `/unmute`, `/status`, `/more`, `/help`. A test checks that every command in the bot's menu is actually handled. The "Ещё" screen gathers statistics, workflow launch, agent history, export, silence and help. Times in messages are UTC.

Messages pass through Telegram's servers and are not end-to-end encrypted. They may contain agent commands and parts of mail; do not enable the bridge if commands or diffs contain secrets.

## Tasks (the board)

A card has a title, description, assignee, progress, labels, a note, a mail thread and creation/modification times. Statuses: `backlog`, `todo`, `in_progress`, `review`, `done`, `blocked`, `archived`. The `--log` journal stores a full snapshot after every change; on restore, the last snapshot in the history wins. At most 500 non-archived cards.

| API | Purpose |
|---|---|
| `GET /api/tasks?include_archived=0\|1` | Board list; the archive is hidden by default. |
| `POST /api/tasks` | Create a card with `title`, `description`, `assignee`, `status`, `labels`. |
| `PATCH /api/tasks/{id}` | Change a card; `assignee: null` clears the assignee. |
| `POST /api/tasks/{id}/dispatch` | Send the task to the assignee by mail and save the thread. |
| `GET /api/tasks/mine` | Tasks of the current agent, by its personal token. |
| `POST /api/tasks/{id}/update` | An agent updates `status`, `progress` and `note` of its own card. |

Agents have the MCP tools `list_my_tasks` and `update_task`. After finishing work, the agent sets `review`; only the orchestrator can close a card with `done` or `archived`. Dispatch requires mail to be enabled and an assignee to be set; the message wakes the agent through the existing mail mechanism. If the subject with the ID and title exceeds the mail limit, the title in the subject is shortened.

## Dashboard and board

The "Дашборд" ("Dashboard") tab shows the state of members, work counters and current tasks. Statistics come from `/api/stats` and are rebuilt from the available journal history; team data comes from `/api/state`, and cards from `/api/tasks` and SSE events. After a restore, the counters are limited to the saved history.

Claude's token usage and cost come from the final message of a turn; input tokens include cache reads and writes. For Codex, tokens are counted as the difference between the thread's cumulative counters before and after the turn (checked on a live turn). It does not report cost, so the cost total covers Claude only. Claude's cost is Claude Code's estimate from the price list, not the bill for your plan.

On the "Доска" ("Board") tab you can create a card, assign an assignee, change the description and status, send the task to an agent with the "Запустить" ("Start") button, or move a card between columns with the mouse. The status selector is available without dragging. The styling of the tabs is temporary.

In Telegram the board works the same way: status tabs, cards, status and assignee through buttons (see the table in the Telegram section). `/task <title>` creates a task in the queue without starting it.

## Agent mail

The option `serve --mail-thread-limit N` sets the maximum number of messages in a thread, from 2 to 100 (default 12).

When `serve` starts, Claude, Codex and the orchestrator have mailboxes. Agents use the MCP tools `send_mail` and `check_inbox`; the recipient gets a short notification about new mail. Replies are linked by a thread identifier.

`POST /api/mail/send` takes `{from, to, subject, body, thread?}`; `POST /api/mail/inbox` takes `{agent}` and marks the received messages as read. Both routes are protected by the token and the Host/Origin checks. By default the maximum is 12 messages per thread; the message body is up to 65536 bytes and the subject up to 200 characters. Mailbox history is restored from the saved events, within the server's history. Mail is data from another agent, not instructions from the user. A workflow mail step waits for a reply addressed to the orchestrator in the same thread. In the "Почта" ("Mail") tab you can send a new message or reply in a thread; messages sent from the page go out in the orchestrator's name.

The notification wakes an agent only for new messages: unread mail waits for a new message after a restart. Each agent has a personal token that works only on the mail routes and only on its own behalf; the main token stays with the page. The personal token is still visible in the agent's process arguments and allows writing in its name and reading its mail.

## Workflow

A scenario is a TOML file. Steps run in order; a step's result can be substituted into the next prompt.

```toml
name = "implement-and-review"
description = "Implementation and review with a fix loop"
max_steps = 20                      # safety fuse, 20 by default

[[step]]
id = "implement"
agent = "codex"                     # team member ID
prompt = "{task}"

[[step]]
id = "review"
agent = "claude"
prompt = "Task:\n{task}\n\nThe implementer's report:\n{prev.output}\n\nIf everything is correct, write VERDICT: APPROVED as the last line; otherwise list the problems."
until = "VERDICT: APPROVED"         # the last non-empty line of the output must be exactly this
otherwise = "fix"                   # otherwise go to the step fix…
max_loops = 2                       # …at most 2 times (2 by default), then the workflow is Failed
next = "end"                        # on success, finish

[[step]]
id = "fix"
agent = "codex"
prompt = "The reviewer's comments:\n{review.output}\n\nFix them."
next = "review"
```

- `agent` and `cc` accept `@role`: the first member with that tag is chosen, and `@lead` always chooses the lead. In `cc`, `@others` adds the remaining members. Literal IDs are supported as well.
- Placeholders: `{task}`, `{<id>.output}`, `{prev.output}`, `{team}` (the list of members with their roles) and `{@role}` (the ID of the member with that role). Unknown roles and duplicate recipients are detected at launch.
- A step's output is the agent's last message of the turn.
- `until` does not check for a substring but for a verdict: it counts only if it is the whole last non-empty line of the output (spaces and Markdown emphasis around it are ignored). "NOT VERDICT: APPROVED", or a verdict followed by comments, does not pass. Choose a rare marker that the reviewer will not write in their comments.
- When a prompt is built from someone else's output, the orchestrator publishes `handoff { from, to, step, text }`: on the page this is a "claude → codex" card and a "← claude · fix" mark on the prompt in the recipient's column.
- Specification errors (duplicate ids, unknown steps and placeholders, `otherwise` without `until`, unknown keys) are caught at load time.
- At launch, all `agent` and `cc` values are checked to be in the current team.
- Built in: `implement-and-review`, `cross-check`, `mail-review` and `mail-team`; add your own files with `serve --workflow FILE` (can be repeated). For `pair`, the engineer is `codex` and the reviewer and lead are `claude`; for `trio`, the engineer is `main`, the reviewer and lead are `lead`, and `junior` takes part in `mail-team`.
- With `mode = "mail"`, the orchestrator sends the `prompt` as a message to the given `agent` and waits for a reply addressed to itself in the same thread; the body of the reply becomes the step's output. Mail is available only when started through `serve`.
- The optional `subject` sets the message subject (the step id by default), and `timeout_secs` sets how long to wait for the reply (900 seconds by default, range 1–7200). `until` checks the last line of the reply, as in a normal step.
- Example `workflows/mail-review.toml`: Codex implements the task, corresponds with Claude on its own in one thread, and sends the orchestrator the result with a verdict.
- In a mail step, `cc = ["claude"]` sends a copy of the same message to an additional agent in the shared thread; these messages count toward the thread limit.
- `mail-team`: the lead and the other members divide the files by mail, work, and review in a shared thread; the lead sends the result with `VERDICT: APPROVED`. For three members, `serve --mail-thread-limit 40` is recommended.
- Only one workflow runs at a time.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test -p agentsflow-adapters -- --ignored   # live tests, need claude and codex
```

CI (GitHub Actions) runs the same checks on Linux and Windows and checks the syntax of the page script. See [CONTRIBUTING.md](CONTRIBUTING.md) for the contribution rules.

Layout:
- `crates/events`: the event format (`Envelope`, `Event`).
- `crates/adapters`: the `Agent` trait; `claude` (on `claude-codes`) and `codex` (on `codex-codes`).
- `crates/gateway`: `hub` (event bus and an actor per agent), `server` (axum), `workflow` (engine), `runtime`, `cli`.
- `web/index.html`: the interface. `workflows/`: built-in scenarios.

## Limitations

- CLI protocols are unstable: the crates are tied to `claude` 2.1.x and `codex` 0.16x versions.
- Messages arrive whole, without token streaming.
- One working folder and one long-lived session per agent.

## Security

See [SECURITY.md](SECURITY.md) for how to report a vulnerability and for the threat model.

## License

MIT, see the [LICENSE](LICENSE) file.
