# agentsflow на VPS

**Русский** · [English](vps.en.md)

Отделы работают круглосуточно на сервере, вы общаетесь с ними из Telegram. Страница доступна через SSH-туннель: шлюз слушает только `127.0.0.1`.

Хватит небольшого VPS: Ubuntu 24.04, 2 ГБ памяти, 20 ГБ диска. Нагрузка — это процессы `claude` и `codex`, по два-три на каждую идущую задачу.

## 1. Пользователь и инструменты

```sh
sudo adduser --disabled-password --gecos "" agentsflow
sudo apt update && sudo apt install -y git build-essential curl nodejs npm
sudo -iu agentsflow
```

Дальше всё от имени `agentsflow`.

```sh
npm config set prefix ~/.npm-global
echo 'export PATH=$HOME/.npm-global/bin:$HOME/.cargo/bin:$PATH' >> ~/.profile && . ~/.profile
npm install -g @anthropic-ai/claude-code @openai/codex
```

## 2. Вход в Claude и Codex

Агенты работают под вашими подписками, поэтому войти нужно один раз от имени `agentsflow`:

```sh
claude          # войдите; без браузера CLI покажет ссылку, её можно открыть на телефоне
codex login     # способ входа без браузера — в codex login --help
```

## 3. Сборка

```sh
curl https://sh.rustup.rs -sSf | sh -s -- -y && . ~/.cargo/env
git clone <ваш репозиторий agentsflow> ~/agentsflow
cd ~/agentsflow && cargo build --release
sudo install target/release/agentsflow /usr/local/bin/   # из-под пользователя с sudo
agentsflow doctor --live
```

`doctor --live` даёт каждому CLI по короткой задаче и показывает, что вход и формат вывода в порядке.

Если не хочется собирать на сервере, поставьте готовый бинарь вместо сборки: [скрипт установки](../README.md#установка) (кладёт `agentsflow` в `~/.local/bin`) или архив для Linux со страницы релизов. Файл службы ниже ждёт бинарь в `/usr/local/bin/agentsflow`: скопируйте его туда через `sudo install` или поменяйте `ExecStart` в юните.

## 4. Дом

```sh
agentsflow home init ~/home
```

В `~/home/agentsflow.toml` поставьте свой часовой пояс для расписаний, например `utc_offset = "+05:00"`, и при желании модель оркестратора. Отделы добавляются папками в `~/home/departments/` (формат — в [departments.md](departments.md)) или по предложению оркестратора. Проверка:

```sh
agentsflow home check ~/home
```

Дом — git-репозиторий. Добавьте приватный remote и время от времени делайте `git push`: это резервная копия правил, памяти и отделов.

## 5. Telegram и служба

```sh
sudo cp ~/agentsflow/deploy/agentsflow.env.example /etc/agentsflow.env
sudo chmod 600 /etc/agentsflow.env && sudo nano /etc/agentsflow.env   # токен бота и ваш ID
sudo cp ~/agentsflow/deploy/agentsflow.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now agentsflow
journalctl -u agentsflow -f
```

Бот отвечает только пользователям из `AGENTSFLOW_TELEGRAM_USERS`. Напишите ему `/help`, потом задачу текстом.

Сторож — отдельная служба, которая раз в минуту спрашивает шлюз «ты жив?», пишет вам в Telegram, если он замолчал, и завершает зависший процесс, чтобы systemd поднял новый. Он берёт токен бота и ваш ID из того же `/etc/agentsflow.env`:

```sh
sudo cp ~/agentsflow/deploy/agentsflow-watch.service /etc/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now agentsflow-watch
agentsflow watch --home ~/home --once   # проверка настройки: «шлюз отвечает»
```

Подробности и настройки `[watch]` — в [departments.md](departments.md#сторож-шлюза).

Остановка (`systemctl stop`) посылает SIGINT: идущие задачи останавливаются и помечаются «застряла: сервер остановлен», после запуска их можно продолжить кнопкой «Ещё круг». Задачи, оборвавшиеся при сбое, при старте помечаются «сервер перезапущен».

## 6. Страница

```sh
ssh -L 7878:127.0.0.1:7878 <пользователь>@<сервер>
journalctl -u agentsflow | grep -o 'token=[0-9a-f]*' | tail -1
```

Откройте `http://127.0.0.1:7878/?token=…` у себя. Токен меняется при каждом перезапуске службы.

## 7. Обновление

```sh
cd ~/agentsflow && git pull && cargo build --release
sudo install target/release/agentsflow /usr/local/bin/ && sudo systemctl restart agentsflow
```

После обновления `claude` или `codex` стоит прогнать `agentsflow doctor --live`.

## Проверка готовности

Критерий из [спецификации](../specs/001-departments/spec.md), раздел 9: из Telegram отправьте пять задач — три в существующие отделы, одну расплывчатую и одну без подходящего отдела. Должны прийти уточняющие вопросы и предложение отдела; после двух нажатий и одного ответа — пять результатов: внутренние приняты оркестратором, внешний ждёт вашей приёмки, застрявшее — сводкой с кнопками.

## Отделы с сайтами

Отделам со `sites` на сервере нужны Chrome (или Chromium) и Node.js. На сервере нет экрана, поэтому `show_browser` там не включай. Как настроить, описано в `docs/departments.md`, раздел «Сайты с входом».
