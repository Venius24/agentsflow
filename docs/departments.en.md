# Departments and the orchestrator (`serve --home`)

[Русский](departments.md) · **English**

A mode in which you write tasks as plain text and everything else happens on its own: the orchestrator splits the message into tasks for departments, each department does the work in fresh Claude and Codex sessions under its own rules, the orchestrator accepts what stays internal, and you accept what goes outside. Answers arrive in Telegram as they are ready.

The reasoning and decisions behind this are in [specs/001-departments](../specs/001-departments/spec.md) (in Russian). Server installation is in [vps.en.md](vps.en.md).

The web page and the Telegram bot are in Russian for now. Interface labels are quoted in Russian with an English gloss the first time they appear, for example "Ждёт вас" ("Waiting for you").

## Quick start

```sh
agentsflow home init ~/home        # the home: settings, orchestrator, common department, git
agentsflow home check ~/home       # check it after editing
agentsflow serve --home ~/home --telegram
```

Telegram is configured with the variables `AGENTSFLOW_TELEGRAM_TOKEN` and `AGENTSFLOW_TELEGRAM_USERS` (see the README, [Telegram section](../README.en.md#telegram)). After that, just write to the bot:

> Make a post for the channel about the Orca launch, check the Baitak stock, and see what is being written about the AI-agent market.

## The home

```
home/
  agentsflow.toml          settings: parallelism, orchestrator, Codex prices, time zone
  orchestrator/AGENTS.md   orchestrator rules
  departments/<id>/
    department.toml        members, workflow, limits, schedules
    AGENTS.md              department rules; changed only with your consent
    memory/notes.md        department memory; the agents write it themselves
    runs/<task>/           run folder: brief.md, result.md, artifacts/
  .agentsflow/             board, spend ledger, schedules, log (not in git)
```

The home is a git repository: every added rule, new department and limit change is a separate commit, and can be rolled back.

## How a task goes

1. **Triage.** Your message goes to a fresh orchestrator session. It answers with a plan: which tasks, for which departments, with what risk, and what to ask you. The gateway checks every action and creates cards.
2. **Questions.** If answering would otherwise mean guessing, the task waits: a card with the questions and a "✍ Ответить" ("✍ Answer") button arrives in Telegram.
3. **New department.** If no department fits, the orchestrator proposes one: "✅ Создать отдел" ("✅ Create department") or "В общий отдел" ("To the common department").
4. **Run.** The department receives a brief (`runs/<task>/brief.md`): rules, a digest of memory, the task, your answers and input data. The department's workflow, usually doer → reviewer → fix, runs until the reviewer approves or the loop limit is reached.
5. **Acceptance.** An `internal` task (a report, an analysis, a draft for you) is accepted by the orchestrator: it accepts, returns it once with a comment, or hands it to you. An `external` task (a post, a letter, a change in someone else's system) always comes to you: "✅ Принять" ("✅ Accept"), "🔁 Ещё круг" ("🔁 Another round"), "✍ Вернуть с замечанием" ("✍ Return with a comment").
6. **Stuck.** Review not passed, time ran out, budget spent: the task stops and a summary arrives with what was done, how much was spent, and the buttons "Ещё круг" and "Закрыть" ("Close").

## department.toml

```toml
id = "pr"                       # Latin letters, matches the folder name
title = "PR"
description = "Posts, announcements, social media texts"   # the orchestrator picks a department by this
default_risk = "external"       # internal | external; external by default
max_parallel = 1                # tasks of this department at the same time

[limits]
task_usd = 3.0                  # budget for one task
month_usd = 30.0                # monthly limit of the department (optional)
warn_ratio = 0.8                # warn at 80% of the monthly limit
timeout_mins = 60
max_loops = 2                   # review rounds
stall_mins = 10                 # how long agents may stay silent before the run is considered stuck
tool_stall_mins = 30            # the same while a long command (build, tests) is running
on_stall = "restart"            # restart | notify: one automatic restart of the step, or just ask you

[[agent]]                       # 1-4 agents, at least one doer
id = "writer"
kind = "codex"                  # claude | codex
model = "gpt-6.1-terra"         # optional
effort = "medium"               # optional
roles = ["doer"]

[[agent]]
id = "critic"
kind = "claude"
model = "claude-sonnet-5-5"
effort = "medium"
roles = ["reviewer"]

[[agent]]                       # optional: small tasks
id = "fast"
kind = "claude"
model = "claude-haiku-5-5"
effort = "high"
roles = ["small"]

[[agent]]                       # optional: the Codex limit has run out
id = "spare"
kind = "claude"
model = "claude-sonnet-5-5"
effort = "medium"
roles = ["fallback"]

[[step]]                        # the same step format as in a workflow
id = "do"
agent = "@doer"
prompt = "{brief}"

[[step]]
id = "review"
agent = "@reviewer"
prompt = "{brief}\n\nResult:\n{prev.output}\n\nYou only review; do not change files. Send the work back only for errors of substance, and list minor points. If there are none, write VERDICT: APPROVED as the last line."
until = "VERDICT: APPROVED"
otherwise = "fix"
next = "end"

[[step]]
id = "fix"
agent = "@doer"
prompt = "Comments:\n{review.output}\n\nFix them and update result.md."
next = "review"

[[schedule]]                    # optional, as many as you like
cron = "0 9 * * mon-fri"        # minute hour day month weekday
task = "Yesterday's Baitak sales summary"
title = "Morning summary"
```

In the template, the reviewer sends work back only for errors of substance (wrong facts, part of the task not done, a broken rule). Minor points it lists and approves anyway, and on a second check it does not add new requirements. A strict reviewer keeps sending good work around in circles until the budget runs out.

An agent whose only role is `reviewer` works with `sandbox = "read-only"` by default: with write access it would put its review into `result.md` on top of the doer's result. For Codex this is its sandbox. Claude has no sandbox, so the guard enforces it: write tools and commands that look like writes (`>`, `rm`, `mv`, `sed -i`, `git commit`…) are denied at once, without asking you. A script that writes a file by itself, the guard will not see. If a reviewer needs to write, set `sandbox = "workspace-write"` explicitly for Codex and give Claude a second role.

The default roster (the template for a new department and the common department): Codex `gpt-6.1-terra` medium writes the code, Sonnet 5.5 medium reviews it, Haiku 5.5 high does small tasks, and when Codex hits its limit, Sonnet 5.5 medium writes.

- **`small`.** The orchestrator marks a task as small (`"size":"small"`), and this agent does it alone, without review. An `external` task is never small. A task that was returned with a comment goes through the whole department.
- **`fallback`.** Codex reports that the subscription limit is reached: the task is put back in the queue, the spare agent replaces the doer and continues from the files in the folder, and you get a notification. The next task tries Codex again. Other Codex errors stop the task as usual.

These agents are not started until needed. A Codex doer has a catch: without `[prices.codex]` its work does not count toward the dollar budget, and the guard does not see its actions (writing is limited by the sandbox, while reading other people's folders is forbidden only in the brief).

`home check` shows the errors of all departments at once, and warnings: no reviewer, the reviewer is the same model as the doer, bypass mode, `live` with automatic restart of a silent step, a department with `[source]` whose rules do not say that outside text is data, and `delivery = "pr"` into a repository with a `deploy/` folder.

## Mentor

One strong model holds the intent of the task: it writes the brief for the doer and checks the result itself, in the same session. The doers stay those listed in the department. You switch it on and off on the "Сцена" ("Scene") page (bottom left), in the bot with the command `/mentor`, or through `POST /api/mentor {"preset": "..."}`:

| Choice | Model | Effort |
|---|---|---|
| `fable-medium` | `claude-fable-5-2` | medium |
| `fable-low` | `claude-fable-5-2` | low |
| `opus-high` | `claude-opus-5-5` | high |
| `off` | the department roster as written | |

- The mentor is the department's agent with the `reviewer` role: for the duration of the run its model and effort are replaced. In a department without such an agent, one more is added, read-only.
- If the workflow does not start with a mentor step, the mentor's brief is placed before the first step, and the doer receives it together with the department brief. If there is no mentor check, a check and a fix are placed after the last step, up to `max_loops` rounds.
- Small tasks (`small`) run without the mentor.
- The choice is stored in `.agentsflow/mentor.json` and applies to runs started after it. The mentor spends the task budget (`task_usd`) like everyone else.

## Memory and rules

- When an agent learns something useful for later tasks, it appends it to `memory.md` in the run folder. After the run the gateway moves the entry into `memory/notes.md` with the date and task number; the tail of the memory goes into every following brief (`memory_digest_kb`).
- An agent proposes a permanent rule in `rule.md`. You receive a card with "📜 Добавить / Отклонить" ("📜 Add / Reject"). An added rule is appended to `AGENTS.md` as a separate commit; a rejected one is remembered in `memory/rejected.md` and is not proposed again.
- The orchestrator also proposes rules (for example, when you write "remember: reports are always in xlsx"), for a department or for itself.
- `/rules` in the bot: open proposals and the latest rules with a "↩ Отменить" ("↩ Undo") button (`git revert`).

## Orders between departments

If a department needs another's result (PR needs numbers from analytics), the agent describes an order in `request.md`. The task waits, the orchestrator turns the order into a task for the other department, and when it is done, the requesting task is started again with the order's result in its input data. Limits: no more than two levels of orders, an order does not go to a department higher up the chain, the order's budget is the remainder of the requester's budget, and the order's spend is charged to the requester.

## Department questions

If a task cannot be done without your answers, the agent writes the questions in `questions.md`, one per line. The run ends there without review, the task waits, and you get a "✍ Ответить" card; after your answer the department starts again with the answers in the brief. An order to another department works the same way: no review after `request.md`. If you answer "as you see fit", the department treats that as an answer and decides itself.

## Isolation

A department run works in its own folder and may read its own department: rules, memory, past runs. The folders of other departments, the orchestrator and `.agentsflow` are closed: the guard denies such accesses by a Claude agent at once, without asking you. The results of the tasks a run depends on (and completed orders) are copied by the gateway into `inputs/<task>/` of its folder together with `artifacts/`. The guard does not see Codex; it is told in the brief, and a Codex reviewer only reads by default.

## Spend

Every turn is recorded in `.agentsflow/ledger.jsonl` with its tokens: Claude reports its own cost, while Codex is computed from tokens and the prices `[prices.codex."<model>"]` in `agentsflow.toml` (without a price it is not counted in dollars, but its tokens appear on a separate line, "без цены" ("no price")). Task cards, departments on the page and `/spend` show dollars and tokens: `$0.25 · 340k → 3k токенов` (input → output; "токенов" means "tokens"). A task that exceeds its budget is stopped. A department that has used up `month_usd` stops runs and holds the queue; at `warn_ratio` a warning arrives. `/spend` in the bot shows the month's spend and raises the limit by 50%.

## Schedules

`[[schedule]]` in a department puts a task in the queue on a five-field cron (`*`, lists, ranges, `*/15` steps, names such as `mon`/`jan`, `@daily`, `@weekly`, `@monthly`). Time follows `utc_offset` in `agentsflow.toml` (for example `"+05:00"`). After a server restart a firing is not repeated, and one missed during downtime runs once. `/schedules`: pause, "⚡ Сейчас" ("⚡ Now").

## Run supervision

The gateway watches a running run for silence: any event from its agents (a thought, a message, a tool call and its result) counts as a sign of life. While a tool is running (a build, tests), longer silence is allowed: `tool_stall_mins` instead of `stall_mins`. A permission request waiting for you does not count as silence.

When a run is silent longer than allowed:

- **`on_stall = "restart"` (the default).** The step is restarted once in a fresh session, and the agent gets a note: the previous session hung, the files in the run folder are still there, continue from this point and do not repeat sends, publications and pushes that were already done. You receive a message "Шаг перезапущен" ("Step restarted"). If the step has already done something outward, there is no automatic restart: repeating such an action would be worse than silence. The guard counts as "outward" everything that is not on a short list of known local programs (reading, building, testing, editing files): `git push`, `gh`, `curl` and `wget` with a write, `npm publish`, `docker push`, `ssh`, a letter, a message, a write through a connector. An unfamiliar program, and a command inside `bash -lc`, `$(...)` or parentheses, are analysed the same way, and if it cannot be parsed, that also counts as "outward".
- **`on_stall = "notify"`, a second stuck round of the same step, or an outward action.** The task card (page and Telegram) shows "молчит N мин" ("silent for N min") and three buttons: "Подождать ещё" ("Wait more", one more silence period), "Перезапустить шаг" ("Restart the step"), "Остановить" ("Stop").

Silence is reported once; any new agent event clears the mark. For a department with `live = true` it is better to turn off the automatic restart (`on_stall = "notify"`); `home check` reminds you of this.

## Events from a repository

A department can create tasks by itself from GitHub events. The gateway listens to nothing from outside: every `poll_secs` seconds it asks `gh` (at most 60 calls an hour per department), and turns each new event into one internal (`internal`) task.

```toml
[source]
repo = "owner/name"
events = ["ci_failure", "issue_label:agent", "pr_comment:/agent"]   # empty: the repository is needed only for PRs
branches = ["main"]            # branches whose CI failures count
poll_secs = 120                # 30..=3600
own = true                     # your repository: the closing comment goes out at once
```

- `ci_failure` is a failed CI run on a watched branch (pull request runs, including those from forks, do not count); `issue_label:<label>` is an issue with that label; `pr_comment:<prefix>` is a comment on a PR or issue that starts with the prefix and is from the repository's owner, member or collaborator only (a comment from an outsider creates no task).
- One poll produces at most three new tasks; the rest come on later polls. Every `gh` call is limited to 120 seconds, so a hung call does not stop the gateway.
- The first poll only remembers what already exists and creates no tasks. Repeats are filtered out by keys in `.agentsflow/sources.json`.
- The text from an issue, a comment and a CI log (together with the title) goes into the brief between delimiters with a random marker and is treated as data, not instructions. The gateway composes the task title itself ("Issue #7 in o/r"); someone else's title does not go into it. The department's rules must say this (the template for a new department already has the line, and `home check` verifies the others).
- The task card shows which event it came from, with a link to it.

## Result as a pull request

```toml
worktree = true
repos = ["C:/Dev/sbx"]
delivery = "pr"                 # requires worktree, repos and [source] with repo
```

The agent only commits in its own copy of the repository. The gateway takes the branch `agentsflow/<task>` into your repository, pushes it from there without `--force`, and opens a draft pull request (`gh pr create --draft`); the link appears in the card and in the summary. Merging and removing the draft mark stay with you. The gateway's `GH_TOKEN` and `GITHUB_TOKEN` variables do not reach the agents' environment, but a login saved on the machine (`gh auth login`) stays available to any agent command: that is why the guard counts an agent's `git push` and `gh` as an outward action, and the protection is the review and your look at the draft, not the absence of access.

Delivery rules:

- A branch goes out by itself only after review. A task done by a small agent alone is not delivered: the branch stays in the local copy, and a "PR не открыт" ("PR not opened") message arrives.
- `delivery = "pr"` is not accepted for a department without an agent with the `reviewer` role.
- A branch that changed CI files (`.github/`, `.gitlab-ci`, `Jenkinsfile`, `.circleci/` and similar) does not go out by itself: such a push runs code with the repository's secrets, so you look at the branch and push it yourself.
- The PR title and comment are cleaned of `@name`: nobody is pinged.

If a task came from an issue or a comment, the gateway writes a closing comment of a fixed form there: the first line of the summary, a link to the PR, the spend; `@name` mentions are defused. In repositories with `own = true` the comment goes out at once; in other people's repositories it waits for your "Опубликовать комментарий" ("Post the comment") or "Не нужно" ("Not needed") in the card. The comment is written once per task: a second round after "Ещё круг" adds no new one, and "Не нужно" cancels it for good.

## Changes

The Changes tab in the task card on the Scene shows everything the task changed: files and their added and removed line counts on the left, and the changes with line numbers and `+` and `−` markers on the right. The markers do not rely on color alone. On a phone, the file list moves above the changes.

The page shows the source above the list:

- **git.** For a department with `worktree = true`, the gateway compares the `agentsflow/<task>` branch in `runs/<task>/repo` with the commit it branched from (the copy's `origin/HEAD`, or a branch from `[source] branches`, `main`, or `master`). Only committed changes appear. The gateway compares commits and does not read the working tree, so copy settings such as filters and fsmonitor do not run external programs in the gateway. It does not read a copy that is a link or whose `.git` is a file pointing elsewhere. Each git call has a 20-second limit.
- **Agent feed.** Without a copy, or until the branch has commits, changes come from the task's agent feed, separately and in order. Claude's changes have no line numbers, and a change that was later rejected still appears in the feed; the page explains this.

Up to 2 MB of patch text is returned. The file list and line counts are complete; a file with missing text is marked as incomplete.

To comment, select a line number (or "Comment on file"), write a comment, and select "Add". Comments collect at the bottom and are sent together: while the task is running, as a note to its active agent (`/api/agent/note`); when it awaits acceptance, is stuck, or is done, with "Return with comments" (`/api/tasks/{id}/return`, starting another round). The agent receives each comment with `path:line`, a quotation of the line, and your text. The browser stores unsent comments.

## Gateway watchdog

`agentsflow watch --home DIR` is a separate small process that watches the gateway from outside. Every `interval_secs` it asks `GET /api/alive` (the only route without a token; it answers only `{ok, version}`). If the gateway is silent `misses` times in a row, the watchdog writes to you in Telegram directly, not through the gateway, and with `kill_hung = true` it also terminates the hung process so that autostart brings up a new one (no more than once in 5 minutes and no more than three times in a row; only the `agentsflow` process listening on the port is terminated). When the gateway answers again, you get "снова работает" ("working again"). If Telegram is unavailable at that moment, the message is not lost: the watchdog repeats it at every following look (for up to an hour, at most 20 messages). The watchdog reads only `agentsflow.toml`, so a broken `department.toml` does not stop it. After a restart the gateway itself sends which runs were interrupted.

```toml
# agentsflow.toml
[watch]
alive_url = "http://127.0.0.1:7878/api/alive"   # local address only
interval_secs = 60
misses = 3
kill_hung = true
```

The watchdog needs the same `AGENTSFLOW_TELEGRAM_TOKEN` and `AGENTSFLOW_TELEGRAM_USERS`. To check the setup without the loop: `agentsflow watch --home DIR --once`.

- **VPS.** `deploy/agentsflow-watch.service` (installation is in the file's header), a service `agentsflow-watch` next to `agentsflow`.
- **Windows.** `deploy/watch.ps1` keeps the watchdog alive (as `autostart.ps1` does for the gateway) and starts it from a copy of `agentsflow-watch.exe`, so that rebuilding the gateway does not hit a locked file. Create the Task Scheduler task once, by hand:

  ```powershell
  $action = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument '-NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File "<path to the repository>\deploy\watch.ps1"'
  Register-ScheduledTask -TaskName 'agentsflow-watch' -Action $action -Trigger (New-ScheduledTaskTrigger -AtLogOn)
  Start-ScheduledTask agentsflow-watch
  ```

## Telegram

| What | How |
|---|---|
| A silent run | a "🔇 молчит" ("🔇 silent") card: "Подождать ещё", "Перезапустить шаг", "Остановить" |
| A comment on GitHub | a "💬 опубликовать комментарий?" ("💬 post the comment?") card: "Опубликовать" ("Post"), "Не нужно" |
| New tasks | any text message |
| Files for a task | send a file, photo or album with a caption, or in the next message |
| Files for an answer | tap "✍ Ответить" or "Вернуть с замечанием" and send a file |
| A task's summary and files | "📖 Читать итог" ("📖 Read the summary") and "📎 Файлы" ("📎 Files") in the card and the notification, `/files` |
| Task changes | "🧾 Changes" in the card: files with line counts in the caption and all changes as a `.diff` document. Line comments are available on the page. |
| Orchestrator questions | the "✍ Ответить" card |
| New department | the "Создать отдел / В общий" ("Create department / To the common one") card |
| Acceptance | "Принять", "Ещё круг", "Вернуть с замечанием" |
| Board and cards | `/board` |
| Rules | `/rules` |
| Spend | `/spend` |
| Mentor | `/mentor` |
| Schedules | `/schedules` |

### Files

The bot accepts documents, photos, audio, video and voice messages up to 20 MB (Telegram's limit for bots). Files wait in `.agentsflow/inbox/`, and when a task starts they are copied into `inputs/owner/` of its folder; the brief names them to the agent. The orchestrator passes the files to all tasks from the message. If there is no caption, the bot asks what to do, and after 30 minutes without an answer it removes the files.

The reader shows `result.md` and the text files from `artifacts/` page by page right in the chat: Markdown is turned into ordinary formatting, and wide tables into cards. Binary files and files larger than 1 MB arrive as a document. Telegram accepts up to 50 MB from a bot; for a larger file the bot names the path on disk. The bot does not follow symbolic links. When the orchestrator accepted a task itself, the bot sends its summary with reading buttons.

## HTTP API (`--home` mode)

| Method and path | What it does |
|---|---|
| `GET /api/departments` | departments, agents, limits, spend for the month |
| `POST /api/orchestrator` `{text, files?}` | a message to the orchestrator; `files` are paths from `/api/upload` |
| `POST /api/work` `{text, department, title?, risk?, depends_on?, budget_usd?}` | a task straight to a department |
| `POST /api/tasks/{id}/answer` `{text}` | an answer to the questions |
| `POST /api/tasks/{id}/proposal` `{create}` | the decision on a new department |
| `POST /api/tasks/{id}/return` `{text}` | return with a comment |
| `POST /api/tasks/{id}/stop`, `/retry` | stop, put back in the queue |
| `POST /api/tasks/{id}/supervise/{wait\|restart\|stop\|publish\|drop}` | a silent run: wait, restart the step, stop; a closing comment: publish, not needed; 409 if the run is no longer going |
| `GET /api/alive` | without a token: `{ok, version}`, for the watchdog |
| `POST /api/agent/note` `{agent, text, files?}` | a note to the agent of a running task (`agent` of the form `department:member:task`); files are copied into the run folder and the note names the copies; 409 if the task is not running |
| `GET /api/tasks/{id}/result` | the text of result.md |
| `GET /api/tasks/{id}/diff` | task changes by file: `{source, branch?, base?, note?, truncated, files: [{path, status, added, removed, binary, truncated, patches: [{agent?, text}]}]}`; `source` is `git`, `events`, or `none`; patch text is limited to 2 MB and the file list is complete; 404 for an unknown task or a card without a department |
| `POST /api/upload?name=…&batch=…` | the request body is a file (up to 50 MB); it goes into the inbox `.agentsflow/inbox/<batch>/`, the response is `{path, name, size, batch}` |
| `POST /api/tasks/{id}/attach` `{files}` | attach files from the inbox to a task: the next round gets them in `inputs/owner/` |
| `GET /api/tasks/{id}/files` | the task's files: the owner's attachments, `result.md`, the run's `artifacts/` |
| `GET /api/tasks/{id}/file?path=…&inline=true` | download a file from that list; HTML and SVG are served as text |
| `GET /api/rules`, `POST /api/rules/{target}/{id}/approve\|reject`, `POST /api/rules/revert/{commit}` | rules |
| `GET /api/schedules`, `POST /api/schedules/{key}/pause\|resume\|run` | schedules |
| `GET /api/mentor`, `POST /api/mentor` `{preset}` | the mentor: the current choice and the options; `off` turns it off |
| `GET /api/runs`, `GET /api/spend`, `POST /api/home/reload` | runs, spend, re-read the home |

## Good to know

- A fresh Claude session is about 95 thousand tokens of context at the start; the orchestrator's triage of a message on Sonnet costs about $0.08, and a short department task $0.2–0.4.
- Codex on a subscription does not report cost: without `[prices.codex]` its spend does not count toward the limits.
- The month in the spend ledger is counted in UTC.
- The gateway log (`.agentsflow/server.log` and stderr, that is, journalctl on a VPS): task creation, every state change with its spend, department runs, orchestrator decisions and errors, server shutdown and panics. Times in the log are UTC.
- In this mode the page opens on the "Сцена" ("Scene"). On the left are "Ждёт вас" ("Waiting for you") and the departments with their tasks. In the middle are the agents of the selected task side by side, each with its own feed: the brief or what a colleague passed on, "Думал N с" ("Thought for N s") with its thoughts, "Работал · N команд" ("Worked · N commands") with the commands and their outcome, file edits as cards, replies and the reviewer's verdict. Above the agents is a strip of steps (execution, review, "вернул" ("sent back"), fix, acceptance); on the right is the task card: department, who accepts, budget, time, questions, result, files and actions (accept, another round, return with a note, stop). On a phone the agents switch with tabs.
- You can write to an agent right in its feed while the task is running. If it is busy, the note arrives right after its current turn: the agent gets it as a separate turn and answers again with it in mind. If it is waiting for its turn, the note goes along with its next step. A note whose turn never came (the task ended or the server restarted) is marked "не дошло" ("not delivered").
- The "Доска" ("Board") shows tasks in the columns "В очереди" ("Queued"), "В работе" ("In progress"), "Ждёт вас" and "Готово" ("Done"). Dragging a running task to the queue stops it and returns it with a note; a result into "Готово" accepts it (for an external task the page asks again); a finished or stuck task into the queue starts a new round with a note. Direct links: `#task=<id>`, `#board`, `#new`. A finished task opens on the "Итог" ("Summary") tab, like a chat: at the top your request as a bubble on the right with your files, below it the department's answer with tables, under it the files as cards (the summary downloads under the task's title) and the buttons "копировать" ("copy"), "скачать" ("download"), "как работали" ("how they worked"). A finished summary you have not opened yet is highlighted on the left and on the board with a "новое" ("new") mark; the mark clears once the task has stayed open for a couple of seconds (the browser remembers this).
- Tasks that the orchestrator creates when accepting a result get the files of the accepted task: the owner's attachments, `result.md` and the run's `artifacts/` (without browser logs). The doer finds them in its input data, and the orchestrator sees their list during acceptance. The former "Отделы" ("Departments") tab with rules and schedules opens with the button with squares at the bottom left.

## Sites with a login

A department can log in to sites, for example a Kaspi or 1C:Fresh (1С:Фреш) account:

1. Copy `deploy/sites.example.toml` to `<home>/.agentsflow/sites.toml` and fill in the logins and passwords yourself. Create separate read-only accounts for this: a logged-in agent can click everything the account allows.
2. Add the line `sites = ["kaspi", "onec"]` at the start of `department.toml`. Such a department has `max_parallel = 1`, the doer is Claude, and the guard is on for all agents.
3. `agentsflow home check` will show what is missing.

How it works:

- The doer gets the tool `site_login` and the browser tools `browser` (Playwright MCP). The reviewer does not get a browser.
- `site_login` reads the file itself, fills in the form in the department's Chrome and returns only the outcome to the agent. The password reaches neither the model nor `events.jsonl`.
- The guard closes the `.agentsflow` folder to departments. The guard judges by the command text, so it may not stop an agent that deliberately goes around it.
- If a site asks for a code, the agent asks the owner for it as an ordinary question, Chrome stays open and waits, and the next run enters the code in the same form.
- Nobody gets around a captcha: the agent reports it to the owner. On a wrong password the agent does not try again, so as not to lock the account.
- The browser profile is in `<home>/.agentsflow/browser/<department>`. Chrome and Node.js (`npx`) are required.
