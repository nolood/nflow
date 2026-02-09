# PRD: Daemon Full Activation — End-to-End Flow

## Introduction

nflow-daemon компилируется и стартует, но **не создаёт Unix-сокет** — единственный канал связи между CLI и daemon. В результате ни одна CLI-команда (кроме `daemon status`) не работает. Помимо этого, в daemon накопилось 32 warnings dead code: система миграций БД, crash recovery, spawn-функции — всё написано, протестировано, но не подключено к production flow. Эта PRD охватывает полное оживление daemon: от починки сокета до end-to-end flow `init → spec → plan → run`.

## Goals

- Починить создание Unix-сокета — daemon должен принимать клиентские подключения
- Подключить миграции БД при старте daemon
- Подключить crash recovery при старте daemon
- Устранить все 32 dead code warnings (подключить или удалить)
- Довести до рабочего состояния все 33 CLI-команды
- Обеспечить полное тестовое покрытие: unit + integration + E2E
- Починить падающий тест `test_acquire_lock_contention`
- Добавить unit-тесты для nflow-core (сейчас 0 тестов)

## User Stories

### US-001: Socket server startup in daemon
**Description:** As a developer, I want the daemon to create and listen on a Unix socket so that CLI commands can communicate with it.

**Acceptance Criteria:**
- [ ] `main.rs` вызывает `socket::start_server()` после инициализации (по аналогии с тестами в `e2e_tests.rs:56-67`)
- [ ] Сокет создаётся по пути `~/.nflow/nflow.sock`
- [ ] Сокет имеет permissions `0o600`
- [ ] `HandlerState` создаётся с путём к БД
- [ ] `create_handler()` подключает все 24 command handler-а к сокету
- [ ] При graceful shutdown сокет удаляется
- [ ] `nflow daemon start` + `nflow init --name test` работает без ошибок
- [ ] `cargo test` проходит
- [ ] `cargo clippy` без warnings

### US-002: Database migrations on daemon start
**Description:** As a developer, I want migrations to run automatically when the daemon starts so that the schema is always up to date.

**Acceptance Criteria:**
- [ ] `db::run_migrations()` вызывается в `foreground_mode()` после открытия соединения
- [ ] Миграция `001_init.sql` применяется при первом запуске (создаёт все таблицы)
- [ ] При повторном запуске миграции не применяются повторно (idempotent)
- [ ] Backup БД создаётся перед миграцией если есть обновление схемы
- [ ] `MIGRATIONS` constant и `DbError::Migration` больше не dead code
- [ ] `cargo test` проходит
- [ ] `cargo clippy` без warnings

### US-003: Crash recovery on daemon start
**Description:** As a developer, I want the daemon to recover from crashes on startup so that stale state doesn't block execution.

**Acceptance Criteria:**
- [ ] `recovery::recover_session_state()` вызывается в `foreground_mode()` после миграций
- [ ] Stale agents (мёртвые PID) помечаются как failed
- [ ] Orphaned in-progress tasks помечаются как failed
- [ ] Active spec sessions сбрасываются
- [ ] Stale socket/PID файлы очищаются
- [ ] Все 16 recovery-функций подключены или помечены как используемые
- [ ] `RecoveryReport` используется (логируется или возвращается)
- [ ] `cargo test` проходит
- [ ] `cargo clippy` без warnings

### US-004: Clean up dead code in daemon spawning
**Description:** As a developer, I want to eliminate dead code warnings for daemon spawn functions so that the codebase is clean.

**Acceptance Criteria:**
- [ ] `spawn_daemon()`, `spawn_daemon_foreground()`, `spawn_daemon_with_mode()` — либо подключены к CLI (для `nflow daemon start`), либо удалены
- [ ] `is_daemon_running()` — подключена к CLI или удалена
- [ ] `read_pid_file()`, `remove_pid_file()` — подключены или удалены
- [ ] Если функции перемещаются в nflow-cli — обновить зависимости в Cargo.toml
- [ ] `cargo clippy` без warnings для daemon.rs

### US-005: Clean up dead code in DB queries and handlers
**Description:** As a developer, I want to eliminate remaining dead code warnings so that `cargo clippy` is clean.

**Acceptance Criteria:**
- [ ] `list_specs_by_status()` — подключить к фильтрации в `spec.list` handler или удалить
- [ ] `reset_active_sessions()` — подключить к recovery или удалить
- [ ] `count_tasks_by_status()` — подключить к `exec.status` handler или удалить
- [ ] `find_running_agent_runs_by_session()` — подключить или удалить
- [ ] `read_process_start_time()` — консолидировать с `platform::get_pid_start_time()` или удалить
- [ ] Unused `state` parameter в `handle_cleanup_logs()` — использовать или заменить на `_state`
- [ ] Unused import `get_pid_start_time` в recovery.rs — удалить
- [ ] `cargo build 2>&1 | grep warning | wc -l` = 0
- [ ] `cargo clippy` без warnings

### US-006: Fix failing test `test_acquire_lock_contention`
**Description:** As a developer, I want the lock contention test to pass reliably so that CI is green.

**Acceptance Criteria:**
- [ ] Тест `test_acquire_lock_contention` стабильно проходит
- [ ] Timing assertion ослаблен или заменён на polling-based подход
- [ ] Тест работает корректно на быстрых машинах и в CI
- [ ] `cargo test -p nflow-cli` — 0 failures

### US-007: Unit tests for nflow-core
**Description:** As a developer, I want unit tests for the core business logic library so that DAG, scheduler, and state machine logic is verified.

**Acceptance Criteria:**
- [ ] Тесты для `dag.rs` — валидация DAG, обнаружение циклов, топологическая сортировка
- [ ] Тесты для `scheduler.rs` — выбор готовых задач, уважение зависимостей, max_parallel
- [ ] Тесты для `work_item.rs` — state transitions (pending → in_progress → done/failed)
- [ ] Тесты для `spec.rs` — state machine (draft → approved → decomposed)
- [ ] Тесты для `short_id.rs` — генерация и парсинг коротких ID (E1, S1, T1, T1v)
- [ ] Тесты для `decomposition.rs` — валидация структуры epic → story → task
- [ ] Тесты для `project.rs` — создание и валидация проекта
- [ ] Тесты для `config.rs` — парсинг и дефолты конфигурации
- [ ] `cargo test -p nflow-core` — все тесты проходят
- [ ] Минимум 50 тестов в nflow-core

### US-008: E2E test — daemon socket lifecycle
**Description:** As a developer, I want an E2E test that starts the real daemon binary, connects via socket, sends commands, and verifies responses.

**Acceptance Criteria:**
- [ ] Тест запускает `nflow-daemon` binary как child process
- [ ] Ожидает появления сокета (polling с timeout)
- [ ] Выполняет handshake по NDJSON протоколу
- [ ] Отправляет `project.init` и получает success response
- [ ] Отправляет `project.list` и видит созданный проект
- [ ] Останавливает daemon и проверяет cleanup (сокет удалён, PID файл удалён)
- [ ] Тест использует temp directory для `~/.nflow/`
- [ ] Timeout: 30 секунд максимум
- [ ] Тест проходит стабильно (без flaky timing)

### US-009: E2E test — full init → spec → plan flow
**Description:** As a developer, I want an E2E test covering the complete spec-to-plan workflow to verify end-to-end integration.

**Acceptance Criteria:**
- [ ] Тест инициализирует проект (`project.init`)
- [ ] Создаёт spec (`spec.new`) — использует mock-claude для генерации
- [ ] Одобряет spec (`spec.approve`)
- [ ] Генерирует plan (`plan.generate`) — использует mock-claude для декомпозиции
- [ ] Проверяет структуру plan (`plan.show`) — epics, stories, tasks присутствуют
- [ ] Одобряет plan (`plan.approve`)
- [ ] Проверяет статус (`exec.status`) — все items в pending
- [ ] Тест использует mock-claude binary для предсказуемых ответов
- [ ] Тест проходит за < 30 секунд

### US-010: E2E test — execution with mock agents
**Description:** As a developer, I want an E2E test that runs execution with mock Claude agents to verify the scheduler, worktrees, and branch creation work end-to-end.

**Acceptance Criteria:**
- [ ] Тест продолжает flow из US-009 (или повторяет setup)
- [ ] Запускает execution (`exec.run`)
- [ ] Mock agents выполняют impl и verify tasks
- [ ] Worktrees создаются в правильных директориях
- [ ] Branches создаются с правильными именами
- [ ] Статус обновляется: pending → in_progress → done
- [ ] После завершения всех stories — wave помечается completed
- [ ] Agent logs записываются в `~/.nflow/projects/{name}/agent-logs/`
- [ ] Тест проходит за < 60 секунд

### US-011: Integration tests for CLI → daemon communication
**Description:** As a developer, I want integration tests verifying that each CLI command correctly communicates with the daemon.

**Acceptance Criteria:**
- [ ] Тест для каждой группы команд: project, spec, plan, exec, worktree, cleanup, config
- [ ] Проверка корректных exit codes (0=success, 3=not found, 4=invalid args, 5=invalid state)
- [ ] Проверка JSON output (`--json` flag) — валидный JSON, правильная структура
- [ ] Проверка human-readable output — ожидаемые строки присутствуют
- [ ] Проверка error messages — понятные сообщения об ошибках
- [ ] Streaming commands (`spec new`, `log -f`) — тест получает потоковые ответы
- [ ] Минимум 20 integration тестов

### US-012: Verify all 33 CLI commands work end-to-end
**Description:** As a developer, I want to verify that every CLI command produces the expected result when connected to a running daemon.

**Acceptance Criteria:**
- [ ] `daemon start/stop/status` — работают корректно
- [ ] `init`, `projects list`, `project delete` — CRUD для проектов
- [ ] `spec new/list/view/approve/reopen/delete/resume` — полный lifecycle спецификации
- [ ] `plan generate/show/feedback/approve/discard` — полный lifecycle плана
- [ ] `run/pause/status/retry/skip/continue/stop/cancel` — управление execution
- [ ] `log <task_id>` и `log <task_id> -f` — просмотр логов
- [ ] `worktree list/clean` — управление worktrees
- [ ] `cleanup --logs` — очистка старых данных
- [ ] `config show/set` — управление конфигурацией
- [ ] `tui` — возвращает "not implemented" message (не crash)
- [ ] Каждая команда задокументирована в `--help`
- [ ] Ни одна команда не паникует (все ошибки обработаны)

## Functional Requirements

- FR-1: Daemon MUST create Unix socket at `~/.nflow/nflow.sock` при старте и удалять при shutdown
- FR-2: Daemon MUST выполнять DB миграции при каждом старте (idempotent)
- FR-3: Daemon MUST выполнять crash recovery при старте (detect stale agents, reset orphaned tasks)
- FR-4: Socket MUST иметь permissions `0o600` (owner-only read/write)
- FR-5: CLI MUST автоматически стартовать daemon если он не запущен (auto-start pattern)
- FR-6: Все command handlers (24 штуки) MUST быть подключены к socket server
- FR-7: NDJSON protocol version handshake MUST выполняться при каждом подключении
- FR-8: Streaming responses MUST использовать `done: true/false` для сигнализации завершения
- FR-9: `cargo build` MUST выдавать 0 warnings
- FR-10: `cargo clippy` MUST выдавать 0 warnings
- FR-11: `cargo test` MUST выдавать 0 failures (82 existing + new tests)
- FR-12: Recovery при старте НЕ ДОЛЖЕН ломать корректно работающие agents (проверка PID + start_time)

## Non-Goals

- Реализация TUI (остаётся заглушка "not implemented")
- Интеграция с реальным Claude Code CLI (используем mock-claude для тестов)
- Создание MR/PR в реальных git-репозиториях
- Performance-оптимизация scheduler-а
- Поддержка TCP-сокетов или HTTP API
- CI/CD pipeline setup
- Документация пользователя (man pages, tutorials)

## Technical Considerations

- **Socket server** уже реализован в `socket.rs` — нужно только вызвать `start_server()` в `main.rs` (по аналогии с E2E тестами)
- **Миграции** уже реализованы в `db/mod.rs` — нужно вызвать `run_migrations()` после открытия connection
- **Recovery** реализован в `recovery.rs` (1068 строк) — нужно вызвать `recover_session_state()` при старте
- **mock-claude** binary уже есть в `crates/mock-claude/` — использовать для E2E тестов
- **Tokio runtime** уже используется в daemon — socket server уже async-ready
- Dead code в `daemon.rs` (spawn functions) скорее всего нужно перенести в CLI crate, т.к. CLI отвечает за auto-start daemon
- nflow-core — чистая библиотека без IO, тесты должны быть чисто unit (без tempdir, без async)

## Success Metrics

- `cargo build 2>&1 | grep "warning" | wc -l` = 0 (ноль warnings)
- `cargo test` — все тесты зелёные, 0 failures
- `cargo test 2>&1 | grep "test result"` — итого 150+ тестов (82 existing + 50 core + 20 integration)
- `nflow daemon start && nflow init --name demo && nflow daemon stop` — работает без ошибок
- Полный flow `init → spec → plan → run → status → done` работает с mock-claude

## Open Questions

- Нужно ли переносить spawn-функции из `daemon.rs` в `nflow-cli`, или CLI уже spawns daemon binary напрямую через `Command::new()`?
- Как `nflow daemon start` должен находить binary `nflow-daemon`? Через PATH, sibling binary, или hardcoded path?
- Нужен ли event_bus для socket server в production (сейчас в тестах передаётся `None`)?
- Стоит ли добавить health-check endpoint для daemon (помимо PID file check)?
