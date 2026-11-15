# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

The workspace version is `0.1.0`, but no release has been published yet. Everything below is the history up to now, summarized from the git log. When the first release is tagged (`v0.1.0`), rename "Unreleased" to `0.1.0` with the date and start a new empty "Unreleased" section above it.

## [Unreleased]

### Added

#### Gateway, agents and workflows

- Added an ACP client for Gemini, OpenCode and configurable ACP commands.

- Cargo workspace with an event model, a Claude Code adapter and a Codex adapter behind one `Agent` trait, and a hub with an SSE server and a web page.
- Workflow engine: steps, handoff between agents, review loops (the verdict must be the last line of the output), diffs synthesized for Claude edits, and headless `agentsflow flow`.
- Event journal (`.agentsflow/events.jsonl`, on by default; `--log FILE`, `--no-log`) and restoring the last events after a restart.
- Agent mail: mailboxes, the MCP tools `send_mail` and `check_inbox`, a mail workflow step, personal agent tokens, a compose form on the page, `cc` in mail steps, the `mail-team` workflow and `serve --mail-thread-limit`.
- Session export to Markdown or JSONL.
- A Stop button that interrupts the current turn of one agent.
- Team roster: any number of named agents (1 to 6) with roles, models and effort, the `pair` and `trio` presets, and `team init`, `team check`, `team show`.
- Per-member access: `mode` (`ask`, `auto`, `smart`, `bypass`) and `sandbox`, with `smart` mode that lets safe requests pass and sends guard-flagged ones to the owner.
- Command guard that refuses destructive shell commands before they are allowed.
- Task board: shared task cards for the orchestrator and the agents, MCP tools `list_my_tasks` and `update_task`, dispatch to an assignee by mail.
- Dashboard and kanban views, agent statistics, token and cost statistics.
- `agentsflow doctor` with a live check, and a test on a recorded real Claude turn that catches a change in the CLI output format.
- Agent errors with restart, a health endpoint, a diagnostic server log, and Codex token usage.
- Project picker: `serve --projects` switches the whole team to another folder from the page or Telegram.
- The program version is shown by `--version`, in `/api/state`, on the page and in Telegram.
- MIT license.

#### Telegram

- Telegram bridge with notifications and permission buttons.
- A control panel with the same capabilities as the web page: team, requests, board, mail, workflows, statistics, log, export and help screens.

#### Departments mode (`serve --home`)

- Specification for departments and the orchestrator (`specs/001-departments`).
- A home folder with `agentsflow.toml` and departments (`department.toml`), `agentsflow home init` and `home check`.
- Department runs: a run queue, fresh sessions for each task, and budgets.
- An orchestrator that turns the owner's messages into department tasks, asks clarifying questions, proposes new departments, and accepts internal results.
- Department memory and owner-approved rules (committed to git and revertible), and a spend screen.
- Schedules (`[[schedule]]` with cron), and orders between departments.
- Reviewers: softer review, read-only Codex reviewers, and questions that skip review.
- Isolation of department folders, inputs from other departments copied into the run folder, and tokens shown next to dollars.
- Default roster: Codex writes, Sonnet reviews, Haiku takes small tasks, and a fallback agent takes over when the Codex limit is reached.
- Departments work in the owner's repositories, with solo and pair modes, and logins to sites for departments.
- Telegram: files from the owner, results back, and a reader in the chat.
- The "Сцена" ("Scene") page, where you watch department agents think and step in by note, with attaching and downloading task files, a result tab shown as a chat, and unread results.
- Mentor switch.
- Run supervision (restart or ask when a run goes silent), repository events as tasks, and the gateway watchdog (`agentsflow watch`).
- Deployment on a VPS (systemd units) and documentation for departments (`docs/departments.md`, `docs/vps.md`).

#### Release and community

- Release workflow that builds binaries for Linux x86_64, Windows x86_64, and macOS on Apple silicon and Intel, publishes SHA-256 checksums, and creates a GitHub release from a `v*` tag.
- `install.sh` (Linux, macOS) and `install.ps1` (Windows) that install the latest release into a user directory after verifying its checksum.
- English documentation: `README.en.md`, `docs/departments.en.md`, `docs/vps.en.md`, and language switch links in both language versions.
- `CONTRIBUTING.md`, `SECURITY.md`, this changelog, issue templates and a pull request template.

### Changed

- The web page was redesigned: the team overview became a transit diagram, with light and dark themes.
- `.gitattributes` was added so that the repository and checkouts use LF line endings.
- The page no longer drags the view back down while you scroll up.
- In departments mode the page always opens on the "Сцена".

[Unreleased]: https://github.com/Venius24/agentsflow/commits/master
