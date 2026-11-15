## What this changes

<!-- What and why, in a few sentences. Link the issue if there is one: "Fixes #123". -->

## How you checked it

<!-- Commands you ran, what you clicked. If you could not check something (live agents, Telegram), say so. -->

## Checklist

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] Web script syntax check (the `node -e` command in CONTRIBUTING.md), if `web/index.html` changed
- [ ] `web/index.html` is still a single file with no build step and no external resources, and agent output is shown with `textContent`
- [ ] Interface text is in Russian and uses the existing terms; anything new on the page is also in the Telegram bot
- [ ] No secrets (tokens, passwords, real journals) in code, logs, tests or this description
- [ ] Docs updated (README, `docs/`, and the English twin of any Russian document I changed), and `CHANGELOG.md` for user-visible changes
