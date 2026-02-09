# nflow

CLI/TUI-оркестратор для Claude Code агентов. Управляет полным циклом разработки: написание спецификаций через диалог с Claude, декомпозиция в DAG задач, параллельное выполнение через Claude Code агентов в изолированных git worktrees. Каждая история = ветка + коммиты + merge request.

## Как это работает

```
nflow spec new "auth"       # 1. Пишем спеку в диалоге с Claude
nflow spec approve "auth"   # 2. Утверждаем
nflow plan generate         # 3. Claude декомпозирует в epic → story → task
nflow plan approve          # 4. Утверждаем план
nflow run                   # 5. Агенты работают параллельно, каждый в своём worktree
nflow status                # 6. Наблюдаем
```

Три фазы: **SPEC** (спецификация) → **DECOMPOSE** (декомпозиция) → **EXECUTE** (выполнение).

Задачи внутри истории выполняются последовательно с чередованием impl→verify. Истории без зависимостей выполняются параллельно (до `max_parallel` агентов). После завершения всех задач nflow делает rebase, push и создаёт PR/MR.

## Требования

| Зависимость | Версия | Назначение |
|---|---|---|
| Rust | 1.75+ | Сборка nflow |
| Claude Code CLI | latest | AI-агент (`claude`) |
| Git | 2.20+ | Worktrees |
| `gh` (GitHub CLI) | 2.0+ | Создание PR (GitHub) |
| `glab` (GitLab CLI) | 1.30+ | Создание MR (GitLab) |

```bash
# Проверить зависимости
rustc --version && claude --version && git --version && gh auth status
```

## Установка

### Из исходников (dev-режим)

```bash
git clone https://github.com/nolood/nflow.git
cd nflow
cargo build --release
```

Установить бинарники в `~/.cargo/bin/`:

```bash
cargo install --path crates/nflow-cli      # nflow — CLI-клиент
cargo install --path crates/nflow-daemon   # nflow-daemon — фоновый процесс
cargo install --path crates/nflow-tui      # nflow-tui — TUI-клиент (опционально)
```

Одного `nflow` достаточно — он может запускать и демон (`nflow daemon start`), и TUI (`nflow tui`) как подкоманды.

### Для разработки (без установки)

```bash
cargo build
# Запуск напрямую:
./target/debug/nflow --help
./target/debug/nflow-daemon
```

## Быстрый старт

### 1. Запустить демон

```bash
nflow daemon start
# Для отладки — в foreground:
nflow daemon start --foreground
```

Демон слушает `~/.nflow/nflow.sock`, пишет логи в `~/.nflow/logs/daemon.log`.

### 2. Инициализировать проект

```bash
cd /path/to/your/project    # должен быть git-репозиторий
nflow init --name "my-project"
```

### 3. Написать спецификацию

```bash
nflow spec new "auth-login"
# Claude задаёт вопросы, вы отвечаете
# Результат: ~/.nflow/projects/my-project/specs/auth-login.md

nflow spec approve "auth-login"
```

С доступом к кодовой базе (Claude видит файлы проекта):

```bash
nflow spec new "auth-login" --with-codebase
```

### 4. Декомпозировать в план

```bash
nflow plan generate                    # из всех неиспользованных спек
nflow plan generate --with-codebase    # Claude видит код при декомпозиции

nflow plan show                        # посмотреть план
nflow plan feedback "разбей story 3"   # дать фидбек
nflow plan approve                     # утвердить
```

### 5. Запустить выполнение

```bash
nflow run                      # запустить агентов
nflow run --parallel 4         # с ограничением параллельности
nflow run --dry-run            # посмотреть что запустится

nflow status                   # статус выполнения
nflow log W1-T1 -f             # лог агента в реальном времени
```

### 6. Управлять выполнением

```bash
nflow pause                    # приостановить (текущие агенты доработают)
nflow stop                     # остановить все агенты
nflow stop W1-S2               # остановить конкретную историю
nflow retry W1-T3              # перезапустить упавшую задачу
nflow skip W1-T3v              # пропустить verify и продолжить
nflow continue W1-S2           # продолжить после ручного фикса
```

## ID задач

Все команды используют wave-префиксные short ID:

```
W1-E1   — эпик 1, wave 1
W1-S2   — история 2, wave 1
W1-T3   — задача 3 (impl), wave 1
W1-T3v  — задача 3 (verify), wave 1
```

`nflow status` и `nflow plan show` показывают все текущие ID.

## Конфигурация

```bash
nflow config show
nflow config set max_parallel 4
nflow config set --project my-project git_provider gitlab
nflow config set --project my-project base_branch develop
```

Файл конфигурации: `~/.nflow/config.toml`

```toml
max_parallel = 3           # макс. параллельных агентов (глобально)
git_provider = "github"    # github | gitlab
base_branch = "main"       # базовая ветка
auto_execute = false       # автозапуск после plan approve
max_turns_per_task = 50    # лимит ходов агента
max_time_per_task = 1800   # таймаут задачи (сек), 0 = без лимита
cleanup_worktrees = false  # удалять worktrees после MR
log_level = "info"         # error | warn | info | debug | trace
```

Переменные окружения: `NFLOW_HOME`, `NFLOW_SOCKET`, `NFLOW_LOG_LEVEL`, `NFLOW_MAX_PARALLEL`.

## Архитектура

Шесть крейтов в Rust workspace:

```
crates/
├── nflow-core/      # Бизнес-логика (чистая, без IO)
├── nflow-claude/    # Обёртка Claude Code CLI
├── nflow-git/       # Git-операции (worktrees, ветки, MR)
├── nflow-daemon/    # Фоновый процесс (tokio, Unix socket, scheduler)
├── nflow-cli/       # CLI-клиент (clap)
└── nflow-tui/       # TUI-клиент (ratatui)
```

Демон — единственный писатель в SQLite. CLI/TUI — read-only клиенты, общаются с демоном через Unix socket по протоколу NDJSON.

## Файловая структура

```
~/.nflow/
├── config.toml          # Конфигурация
├── nflow.db             # SQLite база
├── nflow.sock           # Unix socket
├── daemon.pid           # PID демона
├── logs/daemon.log      # Логи демона
├── projects/
│   └── my-project/
│       ├── specs/       # Markdown-спецификации
│       └── agent-logs/  # Логи агентов (W1-T1.log, ...)
└── worktrees/           # Git worktrees для историй
```

## Устранение проблем

```bash
# Демон не запущен
nflow daemon start

# Завис сокет
rm ~/.nflow/nflow.sock && nflow daemon start

# База заблокирована
nflow daemon stop
rm -f ~/.nflow/nflow.db-wal ~/.nflow/nflow.db-shm
nflow daemon start

# Claude не найден
npm install -g @anthropic-ai/claude-code
```

## Разработка

```bash
cargo build              # собрать
cargo test               # все тесты
cargo test -p nflow-core # тесты конкретного крейта
cargo clippy             # линтер
cargo fmt --check        # проверка форматирования
```

## Лицензия

MIT
