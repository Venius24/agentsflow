# Contributing to agentsflow

Thanks for wanting to help. agentsflow is a small Rust gateway with a single-file web page, so the rules below are short. Please read them before you open a pull request.

The project documentation is available in two languages: [English](README.en.md) and [Russian](README.md). The product interface (the web page and the Telegram bot) is in Russian for now.

## What you need

- Rust, stable channel, 1.85 or newer (`rust-toolchain.toml` selects `stable` with `clippy` and `rustfmt`).
- A C compiler. TLS is provided by rustls, so OpenSSL is not needed.
- Node.js, only for the web script syntax check below.
- The `claude` and `codex` CLIs, installed and signed in, only if you run the live tests or try the product with real agents. Normal tests need neither.

## Build and run

```sh
git clone https://github.com/Venius24/agentsflow
cd agentsflow
cargo build --release          # binary: target/release/agentsflow
cargo run -p agentsflow-gateway -- serve --cwd path/to/project
```

`agentsflow doctor` checks that `git`, `claude` and `codex` are found. The repository layout is described in the "Development" section of the [README](README.en.md#development).

## The checks

CI runs these on Linux and Windows. Run them locally before you push; a pull request needs all of them green.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
node -e "const s=require('fs').readFileSync('web/index.html','utf8');new Function(s.match(/<script>([\s\S]*)<\/script>/)[1])"
```

The first three are the checks from the README "Development" section; the fourth checks that the page script parses (CI runs it too). The README also lists the live tests, which need real `claude` and `codex`:

```sh
cargo test -p agentsflow-adapters -- --ignored
```

CI has no real CLIs, so run the live tests yourself when you change an adapter, and after updating `claude` or `codex`. `agentsflow doctor --live` is a quick version of the same check.

## Rules for the code

- Lints are strict: `unsafe_code` is forbidden and clippy warnings fail the build.
- Add a test for behavior you add or fix. Tests that need a real CLI must be marked `#[ignore]`.
- Keep changes focused. A bug fix does not need a refactor next to it.

## Rules for the web page

- **One file.** `web/index.html` is plain HTML, CSS and JavaScript in a single file. It is embedded into the binary at build time. Do not add a build step, a framework, a package, an external font, script or any other network resource: the page must work offline.
- **Never inject agent output as HTML.** Show it with `textContent` (or equivalent), not `innerHTML`.
- Keep light and dark themes working, keep it usable from about 900 px wide up to wide monitors, and keep old event journals rendering.
- **Same capabilities in the Telegram bot.** Everything available on the web page must also be available in the bot, under the same names.

## Rules for the interface text (Russian UI copy)

- All text the user sees in the web page and in the Telegram bot is in Russian: short, plain, no jargon. Do not add English labels to the interface.
- Use the existing terms (агент, команда, задача, карточка, запрос разрешения, опасная команда, режим доступа, оркестратор, почта, тред, workflow) and the same word for the same thing on the page and in the bot.
- State is never shown by color alone: add text or an icon.
- Code, comments, commit messages, issues and documentation can be in English. If you change a Russian document, change its English twin in the same pull request (`README.md` and `README.en.md`, `docs/departments.md` and `docs/departments.en.md`, `docs/vps.md` and `docs/vps.en.md`), or say in the pull request that the translation is still to do.

## No secrets in logs

agentsflow handles bot tokens, personal agent tokens, the page token and, in some setups, website passwords. When you write or change code:

- Do not write tokens, passwords, API keys or the page token into logs, events, the journal, the exported session, error messages or panics. Check every new log line and error message for this.
- Do not pass secrets as command-line arguments (they are visible in the process list). Use environment variables or files with restricted access.
- Never put a real token, password or the contents of a real journal into a test, a fixture, an issue or a pull request. Use obviously fake values.
- Remember that `.agentsflow/events.jsonl` holds whole conversations and commands in plain text. Redact it before you attach any part of it to an issue.

## Commits and pull requests

- Work on a branch and open a pull request against `master`.
- Commit messages are a short imperative summary in English, for example `Telegram: show the file reader for long results`, with details in the body if needed.
- Fill in the pull request template. Describe what changed and how you checked it. If you could not check something (for example the Telegram bridge or a live agent), say so.
- Update the documentation when behavior changes: the README, `docs/`, and `CHANGELOG.md` for anything a user would notice.

## Reporting bugs, ideas and security problems

- Bugs and feature requests: open an issue and fill in the template.
- Security problems: do not open a public issue. Follow [SECURITY.md](SECURITY.md).

By contributing you agree that your work is released under the [MIT license](LICENSE) of this project.
