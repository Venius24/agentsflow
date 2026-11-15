# Security policy

agentsflow starts coding agents that can run commands and change files on your computer, and it can be steered from a web page and from Telegram. Please take security reports seriously and send them privately.

## Reporting a vulnerability

Please do not open a public issue for a security problem.

1. Use GitHub private vulnerability reporting: open the repository's **Security** tab and choose **Report a vulnerability**.
2. If that option is not available, open a public issue that says only that you need a private channel to report a security problem. Do not put any details in it. The maintainer will arrange a private way to receive them.

Please include:

- the agentsflow version (`agentsflow --version`) and your operating system;
- what you did, what you expected and what happened, ideally with the smallest steps that reproduce it;
- what an attacker could achieve, and what they would need first (local access, the page token, a Telegram account, a malicious repository or issue, and so on).

Do not include real tokens, passwords or journals. If an example needs them, replace them with fake values.

This is a small project, so there is no formal response time. Reports are read and answered as soon as possible, and a fix for a confirmed problem is published in a release; the release notes say what was fixed. You are welcome to ask to be credited.

## Supported versions

Only the latest release and the current `master` branch receive fixes. There is no long-term support for older versions.

## Threat model

agentsflow is a local, single-owner tool. The points below describe what it defends against and, just as important, what it does not. They are taken from the [README](README.en.md).

**The web page and the API (localhost only).**

- The server listens only on `127.0.0.1`. Do not forward the port to the outside, and do not put the page behind a public tunnel or reverse proxy: the page can approve command execution.
- Every `/api/*` route needs a token, in the `?token=` query or the `x-agentsflow-token` header. The token is generated on every start. The one exception is `GET /api/alive`, which needs no token and returns only `{ok, version}`.
- The server checks the `Host` header (protection against DNS rebinding) and the `Origin` of POST requests.
- Each agent gets a personal token that works only on the mail routes and only on its own behalf. It is still visible in the agent's process arguments, and it allows writing in the agent's name and reading its mail.
- Anyone who can run code as your user on the same machine, or who can read your process list or the console line with the page token, can use the page. agentsflow does not defend against a hostile local user.

**Telegram.**

- Access is limited to an allowlist of numeric user IDs (`AGENTSFLOW_TELEGRAM_USERS`). The bot token is read only from the environment (`AGENTSFLOW_TELEGRAM_TOKEN`) and is never passed as a launch argument.
- Telegram can send tasks to agents and answer permission requests. **If the Telegram account of an allowed user is hacked, the attacker can control that user's computer.** Turn on Telegram two-factor authentication, and allow only people you would trust with these actions. Set `AGENTSFLOW_TELEGRAM_PROMPTS=0` to stop the bot from sending tasks.
- Messages pass through Telegram's servers and are not end-to-end encrypted. They can contain agent commands and parts of mail. Do not enable the bridge if commands or diffs contain secrets.

**The guard is not a sandbox.**

- The guard checks the text of shell commands in permission requests against a list of known destructive patterns. It parses commands approximately and does not run anything.
- Complex constructs, obfuscation, unknown wrappers, a script that does the harm by itself (for example, a downloaded script that is then run) and actions outside the shell can go unnoticed. Anything it cannot parse is not blocked.
- In `bypass` mode, permission requests and (for Codex) the sandbox are skipped, so the guard does not work. With `guard = false` the check is off.
- The guard cannot see everything a Codex agent does, and it does not stop an agent that deliberately goes around it. The Codex `sandbox` setting is the real restriction there.
- `--yes` allows every request automatically.

**Data on disk.**

- `.agentsflow/events.jsonl` holds whole conversations, commands and diffs in plain text. Do not send it to others. The Markdown export has the same content.
- In departments mode, website passwords you put into `.agentsflow/sites.toml` are read by the `site_login` tool itself and do not reach the model or the journal. Use dedicated, read-only accounts for this.
- A login saved on the machine (for example `gh auth login`) stays available to any command an agent runs. The protections there are review and your own look at the result, not the absence of access.

**Content from outside.**

- Mail between agents, and the text of issues, comments and CI logs that a department receives, are treated as data from another party, not as instructions. Prompt injection through such text is reduced, not eliminated.

## Out of scope

Reports are not needed for the following, because they are documented limits:

- the points listed above under "The guard is not a sandbox", for example "a command the guard did not recognize ran";
- problems that need access you already gave on purpose (`bypass`, `--yes`, an allowed Telegram account, a port you forwarded yourself);
- vulnerabilities in `claude`, `codex`, Telegram or other third-party software; please report those to their vendors.

If you are not sure whether something is in scope, report it privately anyway.
