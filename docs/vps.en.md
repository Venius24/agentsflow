# agentsflow on a VPS

[Русский](vps.md) · **English**

The departments work around the clock on a server, and you talk to them from Telegram. The page is reachable through an SSH tunnel: the gateway listens only on `127.0.0.1`.

A small VPS is enough: Ubuntu 24.04, 2 GB of memory, 20 GB of disk. The load is the `claude` and `codex` processes, two or three for every running task.

## 1. User and tools

```sh
sudo adduser --disabled-password --gecos "" agentsflow
sudo apt update && sudo apt install -y git build-essential curl nodejs npm
sudo -iu agentsflow
```

From here on, everything runs as `agentsflow`.

```sh
npm config set prefix ~/.npm-global
echo 'export PATH=$HOME/.npm-global/bin:$HOME/.cargo/bin:$PATH' >> ~/.profile && . ~/.profile
npm install -g @anthropic-ai/claude-code @openai/codex
```

## 2. Signing in to Claude and Codex

The agents work under your subscriptions, so you need to sign in once as `agentsflow`:

```sh
claude          # sign in; without a browser the CLI shows a link, which you can open on your phone
codex login     # for a way to sign in without a browser, see codex login --help
```

## 3. Build

```sh
curl https://sh.rustup.rs -sSf | sh -s -- -y && . ~/.cargo/env
git clone <your agentsflow repository> ~/agentsflow
cd ~/agentsflow && cargo build --release
sudo install target/release/agentsflow /usr/local/bin/   # as a user with sudo
agentsflow doctor --live
```

`doctor --live` gives each CLI a short task and shows that the sign-in and the output format are fine.

If you prefer not to compile on the server, you can install a release binary instead of building: the [install script](../README.en.md#install) (as `agentsflow`, to `~/.local/bin`) or the Linux archive from the releases page. The service file below expects the binary in `/usr/local/bin/agentsflow`, so either copy it there with `sudo install`, or change `ExecStart` in the unit.

## 4. The home

```sh
agentsflow home init ~/home
```

In `~/home/agentsflow.toml` set your own time zone for schedules, for example `utc_offset = "+05:00"`, and, if you want, the orchestrator's model. Departments are added as folders in `~/home/departments/` (the format is in [departments.en.md](departments.en.md)) or at the orchestrator's suggestion. Check:

```sh
agentsflow home check ~/home
```

The home is a git repository. Add a private remote and run `git push` from time to time: this is the backup of the rules, memory and departments.

## 5. Telegram and the service

```sh
sudo cp ~/agentsflow/deploy/agentsflow.env.example /etc/agentsflow.env
sudo chmod 600 /etc/agentsflow.env && sudo nano /etc/agentsflow.env   # the bot token and your ID
sudo cp ~/agentsflow/deploy/agentsflow.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now agentsflow
journalctl -u agentsflow -f
```

The bot answers only the users in `AGENTSFLOW_TELEGRAM_USERS`. Write `/help` to it, then a task as text.

The watchdog is a separate service that asks the gateway "are you alive?" once a minute, writes to you in Telegram if it goes silent, and terminates a hung process so that systemd brings up a new one. It takes the bot token and your ID from the same `/etc/agentsflow.env`:

```sh
sudo cp ~/agentsflow/deploy/agentsflow-watch.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now agentsflow-watch
agentsflow watch --home ~/home --once   # setup check: "шлюз отвечает" ("the gateway answers")
```

Details and the `[watch]` settings are in [departments.en.md](departments.en.md#gateway-watchdog).

Stopping (`systemctl stop`) sends SIGINT: running tasks are stopped and marked "застряла: сервер остановлен" ("stuck: server stopped"), and after the start you can continue them with the "Ещё круг" ("Another round") button. Tasks cut off by a crash are marked "сервер перезапущен" ("server restarted") at startup.

## 6. The page

```sh
ssh -L 7878:127.0.0.1:7878 <user>@<server>
journalctl -u agentsflow | grep -o 'token=[0-9a-f]*' | tail -1
```

Open `http://127.0.0.1:7878/?token=…` on your own machine. The token changes on every restart of the service.

## 7. Updating

```sh
cd ~/agentsflow && git pull && cargo build --release
sudo install target/release/agentsflow /usr/local/bin/ && sudo systemctl restart agentsflow
```

After updating `claude` or `codex`, run `agentsflow doctor --live`.

## Readiness check

The criterion from the [specification](../specs/001-departments/spec.md) (in Russian), section 9: from Telegram, send five tasks: three to existing departments, one vague, and one with no suitable department. Clarifying questions and a department proposal should arrive; after two taps and one answer there should be five results: the internal ones accepted by the orchestrator, the external one waiting for your acceptance, the stuck one as a summary with buttons.

## Departments with sites

Departments with `sites` need Chrome (or Chromium) and Node.js on the server. A server has no screen, so do not turn on `show_browser` there. How to set it up is described in `docs/departments.en.md`, section "Sites with a login".
