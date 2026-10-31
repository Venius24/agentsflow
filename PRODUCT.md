# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Stack
Plain static HTML/CSS/JS in a single file (`web/index.html`), embedded into the Rust binary at build time and served by the local gateway. No external fonts, scripts or network resources: it must work offline. Decided by the owner ("do everything the way development standards require"); the single-file, no-dependency form is the standard for this embedded local tool.

## Users
One person: the owner and operator ("orchestrator"), who runs a small team of AI coding agents (Claude and Codex CLIs) on their own computer. They sit at the machine with several agents working at once, put tasks on a board, answer permission requests, accept results and read the agents' mail. They also follow and steer the team from a phone through a Telegram bot, so the web page and the bot must offer the same capabilities. They are not a Rust or Cargo programmer: plain-language labels, no jargon. Interface language is Russian.

## Product Purpose
agentsflow lets one person run and supervise several AI agents as a team: each agent has its own session, model and effort, a personal mailbox, and a card on a shared task board. The page shows who is doing what right now, asks the owner for permissions, shows dangerous commands blocked or flagged by the guard, tracks tasks and results, and keeps the team's correspondence and history. Success: the owner always knows what every agent is doing, never approves something dangerous by accident, and gets finished work to review without babysitting.

## Positioning
A local, owner-controlled control room for a mixed team of Claude and Codex agents with roles, a shared task board, agent-to-agent mail and a built-in destructive-command guard. Nothing leaves the machine except the optional Telegram bridge.

## Operating Context
- Runs locally (`agentsflow serve`), opened in a desktop browser alongside editors and terminals; also used in a narrower window next to other work. Light and dark theme follow the system.
- Teams of 2 to 6 agents (preset `pair`, preset `trio`, or a custom `team.toml`).
- Each agent column streams messages, tool calls, diffs and permission requests live. The orchestrator side has the journal, the mail threads with a compose form, workflow runs, export of the session, and (new) a dashboard and a kanban board.
- Permission requests can carry a guard warning (rule and reason); members run in modes ask / auto / bypass, which the owner must always be able to see.
- The same capabilities are mirrored in a Telegram bot (private chat, owner-only allowlist).

## Capabilities and Constraints
- Capabilities (web): per-agent chat with prompt field and Stop, permission cards (allow / deny) with guard warnings, workflow picker and run/stop, mail threads with compose and reply, task board (create, assign, status, progress, dispatch to the assignee), per-agent dashboard (state, turns, tools, permissions, guard denials, mail, last activity), session export (Markdown / JSONL).
- Terminology (Russian UI): агент, команда, задача (карточка), запрос разрешения, опасная команда (guard), режим доступа (ask / auto / bypass), оркестратор (the owner), почта, тред, workflow.
- Task statuses: backlog, todo, in progress, review, done, blocked, archived. Agents can set review but only the owner closes a task (done).
- Constraints: single file, offline, textContent only (no HTML injection from agent output), light and dark theme, works from about 900 px wide up to wide monitors, columns scale with team size (2 to 6 agents), old event journals must keep rendering.
- Parity rule from the owner: everything available on the web page must also be available in the Telegram bot.

## Brand Commitments
Name: agentsflow (lowercase). Interface copy in Russian, short and plain. No existing logo or palette: the current look is a plain functional placeholder and is not binding.

## Evidence on Hand
Real running product (Rust gateway, tested with real Claude and Codex), current page `web/index.html`, README with the API. No customers, testimonials or metrics exist; do not invent any.

## Product Principles
1. The owner's attention is the scarce resource: show state at a glance, interrupt only for what needs a decision.
2. Dangerous things are never quiet: bypass mode, guard warnings and permission requests are unmistakable and one tap from a safe answer.
3. One truth, two surfaces: the web page and the Telegram bot expose the same capabilities with the same words.
4. Plain language over jargon; nothing that requires knowing how the tool is built.
5. Calm by default: dense information stays readable over a long working session, in both themes.

## Accessibility & Inclusion
Keyboard operable, visible focus, sufficient contrast in both themes, state never conveyed by color alone (also text or icon), reduced-motion respected. Russian-language interface.
