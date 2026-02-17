# nflow CLI — Полный план E2E-тестирования

## Цель

Провести исчерпывающее E2E-тестирование всех CLI-команд nflow, используя оба рабочих потока (SDD и Pipeline) для создания реального проекта — **Rust CLI утилиты `taskfile`** (менеджер задач в формате JSON). Результат тестирования — рабочий проект, написанный AI-агентом через nflow CLI, с проверяемыми критериями приёмки на каждом этапе.

## Тестовый проект: `taskfile`

**Описание**: CLI-утилита на Rust для управления задачами. Хранит задачи в `tasks.json`. Поддерживает операции: add, list, done, delete, filter.

**Почему этот проект подходит для тестирования**:
- Достаточно прост для выполнения AI-агентом за 1 цикл
- Имеет чёткие, проверяемые критерии (cargo build/test/run)
- Требует несколько файлов (main.rs, lib.rs, models, tests) — проверяет декомпозицию на stories
- Можно разбить на 2-3 независимые story с зависимостями — проверяет DAG и параллельное выполнение
- Результат легко верифицировать через `cargo test` и ручной прогон CLI

---

## Фаза 0: Подготовка окружения

### T-0.1: Сборка проекта nflow

**Действия**:
```bash
cd /home/nolood/general/nflow
cargo build --workspace
```

**Критерии приёмки**:
- [ ] `cargo build --workspace` завершается с exit code 0
- [ ] Бинарники `nflow-cli`, `nflow-daemon`, `nflow-tui` присутствуют в `target/debug/`
- [ ] `mock-claude` собран в `target/debug/`

### T-0.2: Подготовка тестовой директории

**Действия**:
```bash
export NFLOW_HOME=/tmp/nflow-e2e-test
rm -rf $NFLOW_HOME
mkdir -p $NFLOW_HOME

# Создать пустой git-репозиторий для тестового проекта
mkdir -p /tmp/taskfile-project
cd /tmp/taskfile-project
git init
git commit --allow-empty -m "initial commit"
```

**Критерии приёмки**:
- [ ] `$NFLOW_HOME` существует и пуст
- [ ] `/tmp/taskfile-project` — валидный git-репозиторий с веткой `main`

### T-0.3: Проверка зависимостей

**Действия**:
```bash
claude --version       # Claude Code CLI
git --version          # Git 2.20+
gh --version           # GitHub CLI
rustc --version        # Rust 1.75+
```

**Критерии приёмки**:
- [ ] Все утилиты доступны и версии совместимы

---

## Фаза 1: Жизненный цикл демона

### T-1.1: Запуск демона

**Действия**:
```bash
nflow daemon start
```

**Критерии приёмки**:
- [ ] Команда завершается с exit code 0
- [ ] Файл сокета `$NFLOW_HOME/nflow.sock` создан
- [ ] Файл блокировки `$NFLOW_HOME/daemon.lock` создан
- [ ] PID-файл `$NFLOW_HOME/daemon.pid` создан и содержит валидный PID
- [ ] Процесс `nflow-daemon` виден через `ps aux | grep nflow-daemon`
- [ ] Файл БД `$NFLOW_HOME/nflow.db` создан (SQLite с WAL)

### T-1.2: Статус демона

**Действия**:
```bash
nflow daemon status
```

**Критерии приёмки**:
- [ ] Возвращает JSON/текст со статусом "running"
- [ ] Содержит PID процесса
- [ ] Содержит версию протокола
- [ ] Exit code 0

### T-1.3: Повторный запуск (идемпотентность)

**Действия**:
```bash
nflow daemon start
```

**Критерии приёмки**:
- [ ] Не порождает второй процесс (flock предотвращает)
- [ ] Возвращает сообщение "daemon already running" или аналогичное
- [ ] Существующий демон продолжает работать

### T-1.4: Foreground-режим

**Действия**:
```bash
nflow daemon start --foreground &
DAEMON_PID=$!
sleep 2
kill $DAEMON_PID
```

**Критерии приёмки**:
- [ ] Демон запускается в foreground и пишет логи в stderr
- [ ] Процесс корректно завершается по SIGTERM
- [ ] Сокет-файл удалён после завершения

### T-1.5: Остановка демона

**Действия**:
```bash
nflow daemon start   # Сначала запустить
nflow daemon stop
```

**Критерии приёмки**:
- [ ] Демон завершается (PID больше не существует)
- [ ] Сокет-файл удалён
- [ ] `nflow daemon status` показывает "not running"
- [ ] Exit code 0

### T-1.6: Автозапуск при первой команде

**Действия**:
```bash
nflow daemon stop           # Убедиться, что демон не запущен
nflow projects              # Команда, которая требует демона
```

**Критерии приёмки**:
- [ ] Демон автоматически запускается
- [ ] Команда `projects` возвращает результат (пустой список)
- [ ] `nflow daemon status` показывает "running"

---

## Фаза 2: Управление проектами

### T-2.1: Инициализация проекта

**Действия**:
```bash
cd /tmp/taskfile-project
nflow init
```

**Критерии приёмки**:
- [ ] Exit code 0
- [ ] Ответ содержит имя проекта "taskfile-project"
- [ ] Ответ содержит определённый `base_branch` (main)
- [ ] Ответ содержит определённый `git_provider` (github)
- [ ] Проект появляется в БД

### T-2.2: Повторная инициализация (идемпотентность)

**Действия**:
```bash
cd /tmp/taskfile-project
nflow init
```

**Критерии приёмки**:
- [ ] Возвращает ошибку `ALREADY_EXISTS` или обновляет существующий проект
- [ ] Не создаёт дубликат в БД

### T-2.3: Инициализация с явным именем

**Действия**:
```bash
nflow --project custom-name init
```

**Критерии приёмки**:
- [ ] Проект создан с именем "custom-name"
- [ ] `nflow projects` показывает оба проекта

### T-2.4: Список проектов

**Действия**:
```bash
nflow projects
nflow projects --json
```

**Критерии приёмки**:
- [ ] Текстовый вывод показывает все проекты
- [ ] JSON-вывод (`--json`) содержит массив с проектами
- [ ] Каждый проект содержит: name, path, base_branch, git_provider, created_at

### T-2.5: Удаление проекта

**Действия**:
```bash
nflow project delete custom-name
```

**Критерии приёмки**:
- [ ] Проект удалён из БД
- [ ] `nflow projects` не показывает "custom-name"
- [ ] Попытка повторного удаления возвращает `NOT_FOUND`

### T-2.6: Удаление проекта с данными (--force)

**Действия**:
```bash
# Создать проект, добавить spec, потом удалить
nflow --project to-delete init
nflow project delete to-delete          # Без --force
nflow project delete to-delete --force  # С --force
```

**Критерии приёмки**:
- [ ] Без `--force` — ошибка если есть связанные данные (specs, plans)
- [ ] С `--force` — успешное удаление вместе со всеми связанными данными

### T-2.7: Инициализация вне git-репозитория

**Действия**:
```bash
cd /tmp
mkdir not-a-repo && cd not-a-repo
nflow init
```

**Критерии приёмки**:
- [ ] Возвращает ошибку `INVALID_PARAMS` (путь не является git-репозиторием)
- [ ] Exit code != 0

---

## Фаза 3: Конфигурация

### T-3.1: Просмотр конфигурации

**Действия**:
```bash
cd /tmp/taskfile-project
nflow config show
```

**Критерии приёмки**:
- [ ] Показывает все 10 полей конфигурации с дефолтными значениями:
  - `max_parallel` = 3
  - `git_provider` = "github"
  - `base_branch` = "main"
  - `cleanup_worktrees` = false
  - `max_turns_per_task` = 50
  - `auto_execute` = false
  - `max_time_per_task` = 1800
  - `log_level` = "info"
  - `branch_template` = "nflow/{project}/{story_id}-{story_slug}"
  - `worktree_dir` = "worktrees"

### T-3.2: Установка параметра

**Действия**:
```bash
nflow config set max_parallel 2
nflow config show
```

**Критерии приёмки**:
- [ ] `config set` возвращает подтверждение
- [ ] `config show` показывает `max_parallel` = 2
- [ ] Значение сохранено в `<project>/.nflow/config.toml`

### T-3.3: Невалидные параметры

**Действия**:
```bash
nflow config set max_parallel 0          # Не может быть 0
nflow config set git_provider bitbucket  # Только github/gitlab
nflow config set unknown_key value       # Несуществующий ключ
```

**Критерии приёмки**:
- [ ] Каждая команда возвращает ошибку валидации
- [ ] Значение не изменяется в конфигурации
- [ ] Сообщение об ошибке описывает причину

### T-3.4: Переопределение через env

**Действия**:
```bash
NFLOW_MAX_PARALLEL=5 nflow config show
```

**Критерии приёмки**:
- [ ] `max_parallel` показан как 5 (env > файл > дефолт)

---

## Фаза 4: SDD Flow — Spec → Plan → Execute

Это основной тест: через полный SDD-цикл создаётся проект `taskfile`.

### T-4.1: Создание спецификации

**Действия**:
```bash
cd /tmp/taskfile-project
nflow spec new "taskfile-cli"
```

**Ожидаемое поведение**: Claude начинает интерактивный диалог, задаёт вопросы о проекте. Пользователь (или тестовый скрипт) отвечает на вопросы.

**Сценарий ответов для теста**:
1. "CLI утилита для управления задачами, хранит данные в tasks.json"
2. "Команды: add <title>, list [--status done|todo], done <id>, delete <id>"
3. "Формат задачи: id (u64 auto-increment), title (String), status (todo/done), created_at (DateTime)"
4. "Rust, используя clap для CLI, serde для JSON"
5. "Да, нужны unit и integration тесты"

**Критерии приёмки**:
- [ ] Команда запускает стриминг от Claude
- [ ] Claude задаёт релевантные вопросы (тип `question` в стриме)
- [ ] После завершения диалога спецификация сохранена в файл
- [ ] `nflow spec list` показывает спецификацию со статусом `Draft` или `Approved`
- [ ] Файл спецификации содержит markdown с описанием проекта

### T-4.2: Список и просмотр спецификаций

**Действия**:
```bash
nflow spec list
nflow spec view taskfile-cli
```

**Критерии приёмки**:
- [ ] `spec list` показывает "taskfile-cli" с корректным статусом
- [ ] `spec view` выводит полное содержимое markdown-файла
- [ ] Markdown содержит: описание, команды, формат данных

### T-4.3: Возобновление диалога

**Действия**:
```bash
nflow spec resume taskfile-cli
```

**Критерии приёмки**:
- [ ] Возобновляет предыдущую Claude-сессию (использует `--resume`)
- [ ] Контекст предыдущего диалога сохранён
- [ ] Если сессия завершена — возвращает ошибку `INVALID_STATE`

### T-4.4: Утверждение спецификации

**Действия**:
```bash
nflow spec approve taskfile-cli
nflow spec list
```

**Критерии приёмки**:
- [ ] Статус изменился на `Approved`
- [ ] `spec list` показывает `Approved`

### T-4.5: Переоткрытие спецификации

**Действия**:
```bash
nflow spec reopen taskfile-cli
nflow spec list
nflow spec approve taskfile-cli   # Утвердить обратно для продолжения
```

**Критерии приёмки**:
- [ ] `reopen` возвращает статус в `Draft`
- [ ] Повторное `approve` работает корректно

### T-4.6: Удаление спецификации

**Действия**:
```bash
nflow spec new "temp-spec"
# ... завершить диалог
nflow spec delete temp-spec
nflow spec list
```

**Критерии приёмки**:
- [ ] Спецификация удалена из списка
- [ ] Повторное удаление возвращает `NOT_FOUND`

### T-4.7: Ошибки спецификаций

**Действия**:
```bash
nflow spec approve nonexistent         # Несуществующая
nflow spec approve taskfile-cli        # Уже approved
nflow spec new "taskfile-cli"          # Дубликат имени (при активной сессии)
```

**Критерии приёмки**:
- [ ] Каждый случай возвращает корректное сообщение об ошибке
- [ ] `NOT_FOUND` для несуществующих
- [ ] `INVALID_STATE` для неправильных переходов

### T-4.8: Генерация плана

**Действия**:
```bash
nflow plan generate
```

**Ожидаемый результат**: Claude анализирует утверждённую спецификацию и создаёт план декомпозиции:
- 1 Epic: "taskfile CLI"
- 3 Stories:
  - S1: "Core data model and storage" (models, JSON read/write)
  - S2: "CLI interface" (clap, commands) — зависит от S1
  - S3: "Integration tests" — зависит от S1 и S2
- Tasks для каждой story (impl + auto-generated verify)

**Критерии приёмки**:
- [ ] Стриминг Claude показывает процесс анализа
- [ ] `nflow plan show` показывает иерархию Epic → Story → Task
- [ ] `nflow plan show --dag` показывает DAG зависимостей
- [ ] Каждая story имеет минимум 1 impl task и 1 verify task
- [ ] Зависимости корректны (S2 зависит от S1, S3 зависит от S1+S2)
- [ ] Short IDs назначены (E1, S1, S2, S3, T1, T1v, T2, T2v и т.д.)
- [ ] Wave number = 1
- [ ] Статус плана = InProgress (ещё не утверждён)

### T-4.9: Просмотр плана

**Действия**:
```bash
nflow plan show
nflow plan show --dag
nflow plan show --wave 1
```

**Критерии приёмки**:
- [ ] Обычный вид показывает плоский список с иерархией
- [ ] `--dag` показывает визуализацию зависимостей
- [ ] `--wave 1` фильтрует по конкретной волне

### T-4.10: Обратная связь по плану

**Действия**:
```bash
nflow plan feedback "Добавь в S1 отдельный task для создания Cargo.toml с зависимостями"
```

**Критерии приёмки**:
- [ ] Claude получает feedback и обновляет план
- [ ] Стриминг показывает процесс пересмотра
- [ ] `nflow plan show` показывает обновлённый план
- [ ] Структура плана изменилась в соответствии с feedback

### T-4.11: Утверждение плана

**Действия**:
```bash
nflow plan approve
```

**Критерии приёмки**:
- [ ] Сессия декомпозиции переходит в `Approved`
- [ ] Спецификация переходит в `Decomposed`
- [ ] Все work items сохранены в БД
- [ ] `nflow plan show` продолжает работать

### T-4.12: Отклонение и повторная генерация плана

**Действия**:
```bash
# Предварительно: создать новую спеку, сгенерировать план
nflow plan discard
nflow plan generate    # Повторная генерация
```

**Критерии приёмки**:
- [ ] `discard` удаляет work items из БД
- [ ] Спецификация возвращается в `Approved`
- [ ] Повторная генерация создаёт новый план с wave 2

### T-4.13: Запуск выполнения

**Действия**:
```bash
nflow run
```

**Критерии приёмки**:
- [ ] Команда активирует execution для проекта
- [ ] Scheduler начинает подбирать ready stories
- [ ] `nflow status` показывает `execution_enabled: true`
- [ ] Первая story без зависимостей (S1) переходит в `in_progress`
- [ ] Git worktree создан для S1
- [ ] Ветка создана по шаблону `nflow/taskfile-project/S1-*`

### T-4.14: Мониторинг выполнения

**Действия**:
```bash
nflow status
nflow status --json
```

**Критерии приёмки**:
- [ ] Текстовый вывод показывает иерархию статусов
- [ ] JSON содержит: waves → epics → stories → tasks с текущими статусами
- [ ] Запущенные tasks показывают `in_progress`
- [ ] Blocked stories показывают `pending`

### T-4.15: Просмотр логов

**Действия**:
```bash
nflow log                    # Все логи
nflow log S1                 # Логи конкретной story
nflow log T1                 # Логи конкретного task
nflow log S1 --follow        # Следить за логами в реальном времени
```

**Критерии приёмки**:
- [ ] Без аргументов — показывает последние логи
- [ ] По item_id — показывает логи только этого элемента
- [ ] `--follow` — стримит логи в реальном времени (long-polling)
- [ ] Логи содержат вывод Claude (stream-json формат или текст)

### T-4.16: Пауза и продолжение

**Действия**:
```bash
nflow pause
nflow status    # Проверить что новые task'и не стартуют
sleep 5
nflow continue
nflow status    # Проверить что выполнение возобновлено
```

**Критерии приёмки**:
- [ ] `pause` — `execution_enabled` = false
- [ ] Запущенные агенты продолжают работать
- [ ] Новые task'и не стартуют
- [ ] `continue` — `execution_enabled` = true
- [ ] Scheduler снова подбирает ready stories

### T-4.17: Обработка ошибок задач (retry)

**Действия**:
```bash
# Дождаться пока task T1 завершится с ошибкой (или имитировать через mock-claude)
nflow retry T1
nflow status
```

**Критерии приёмки**:
- [ ] `retry` сбрасывает task в `pending`, story в `ready`
- [ ] Worktree сбрасывается к pre-task коммиту через `git reset --hard`
- [ ] Task перезапускается scheduler'ом
- [ ] Retry несуществующего item — `NOT_FOUND`
- [ ] Retry успешного task — `INVALID_STATE`

### T-4.18: Пропуск задачи (skip)

**Действия**:
```bash
# При failed task:
nflow skip T1v    # Пропустить verify task
nflow status
```

**Критерии приёмки**:
- [ ] Task помечается как `done` (пропущен)
- [ ] Story продолжает выполнение следующих tasks
- [ ] Skip не-failed task — `INVALID_STATE`

### T-4.19: Остановка выполнения

**Действия**:
```bash
nflow stop
nflow status
```

**Критерии приёмки**:
- [ ] Отправлен SIGTERM запущенным агентам
- [ ] Новые task'и не стартуют
- [ ] Статус: `execution_enabled = false`
- [ ] Запущенные агенты получают время на завершение

### T-4.20: Принудительная отмена

**Действия**:
```bash
nflow run        # Запустить снова
nflow cancel
nflow status
```

**Критерии приёмки**:
- [ ] Все in_progress stories/tasks переходят в `cancelled`
- [ ] Агенты получают SIGTERM → ожидание → SIGKILL
- [ ] `execution_enabled` = false

### T-4.21: Полный SDD-цикл до завершения

**Действия**: Запустить полный цикл и дождаться завершения всех stories.

**Критерии приёмки**:
- [ ] Все stories завершены (`done`)
- [ ] Все epics завершены (`done`)
- [ ] Для каждой story создан git worktree
- [ ] Для каждой story создана ветка с коммитами
- [ ] Каждый impl task создал коммит с `[short_id]` в сообщении
- [ ] Каждый verify task прошёл (содержит "VERIFICATION PASSED")
- [ ] MR/PR создан для каждой story (если настроен git_provider)
- [ ] Код в worktree S1 содержит: Cargo.toml, src/main.rs, модели данных
- [ ] Код в worktree S2 содержит: CLI (clap), обработчики команд
- [ ] Код в worktree S3 содержит: тесты
- [ ] `cargo build` проходит в каждом worktree
- [ ] `cargo test` проходит в каждом worktree

---

## Фаза 5: Pipeline Flow

### T-5.1: Запуск pipeline (auto mode)

**Действия**:
```bash
cd /tmp/taskfile-project
nflow pipeline start "Добавь команду 'search' для поиска задач по подстроке в title" --mode auto
```

**Критерии приёмки**:
- [ ] Pipeline run создан в БД
- [ ] Стриминг показывает фазы: Plan → Implement → Review
- [ ] **Plan stage**: Claude создаёт JSON-план с шагами
- [ ] **Implement stage**: Claude вносит изменения в код
- [ ] **Review stage**: Claude проверяет реализацию
- [ ] При `review.passed = true` — pipeline завершается со статусом `Completed`
- [ ] При `review.passed = false` — цикл повторяется (Implement → Review)
- [ ] Максимум `max_iterations` циклов (по умолчанию 5)
- [ ] Финальный статус: `Completed` или `Failed`

### T-5.2: Запуск pipeline (manual mode)

**Действия**:
```bash
nflow pipeline start "Добавь команду 'edit' для редактирования title задачи" --mode manual
```

**Ожидаемое поведение**: Pipeline останавливается после Plan для получения подтверждения.

**Критерии приёмки**:
- [ ] Pipeline создан с `mode: manual`
- [ ] После Plan stage — статус `WaitingForApproval`
- [ ] Стриминг прекращается, ожидается approve/reject

### T-5.3: Утверждение плана pipeline

**Действия**:
```bash
nflow pipeline approve <pipeline_id>
```

**Критерии приёмки**:
- [ ] Статус переходит из `WaitingForApproval` → `Running`
- [ ] `current_stage` переходит в `Implement`
- [ ] Стриминг возобновляется с Implement stage
- [ ] Event `PipelinePlanApproved` транслируется через event bus

### T-5.4: Отклонение плана pipeline

**Действия**:
```bash
# Запустить новый pipeline в manual mode
nflow pipeline start "Добавь команду 'priority' для задач" --mode manual
# Дождаться WaitingForApproval
nflow pipeline reject <pipeline_id> "План слишком сложный, упрости подход"
```

**Критерии приёмки**:
- [ ] Статус переходит обратно в `Running` с `current_stage: Plan`
- [ ] `iteration` инкрементирован
- [ ] Claude получает feedback и пересоздаёт план
- [ ] Цикл Plan → WaitingForApproval повторяется

### T-5.5: Финальное утверждение pipeline

**Действия**:
```bash
# После Review stage в manual mode — статус WaitingForFinalApproval
nflow pipeline approve <pipeline_id>
```

**Критерии приёмки**:
- [ ] Pipeline переходит в `Completed`
- [ ] Event `PipelineFinalApproved` транслируется
- [ ] Все stage records в БД имеют финальные статусы

### T-5.6: Финальное отклонение pipeline

**Действия**:
```bash
# После Review, WaitingForFinalApproval
nflow pipeline reject <pipeline_id> "Не все тесты проходят"
```

**Критерии приёмки**:
- [ ] Если `iteration + 1 <= max_iterations`: pipeline возвращается в `Running`, stage = `Implement`, iteration++
- [ ] Если `iteration + 1 > max_iterations`: pipeline переходит в `Failed`
- [ ] Event `PipelineFinalRejected` транслируется

### T-5.7: Вопросы pipeline (manual mode)

**Действия**:
```bash
# Во время Plan stage Claude может задавать вопросы через PIPELINE_QUESTIONS
nflow pipeline questions <pipeline_id>
nflow pipeline answer <pipeline_id> <question_id> "Используй стандартный вывод в таблице"
```

**Критерии приёмки**:
- [ ] `questions` возвращает список неотвеченных вопросов
- [ ] Каждый вопрос содержит: id, question text, context
- [ ] `answer` помечает вопрос как отвеченный
- [ ] После ответа на все вопросы — pipeline продолжается
- [ ] Answer на уже отвеченный вопрос — `NOT_FOUND`
- [ ] Answer на чужой pipeline — ошибка

### T-5.8: Вопросы pipeline (auto mode)

**Действия**:
```bash
nflow pipeline start "Добавь экспорт в CSV" --mode auto
```

**Критерии приёмки**:
- [ ] Вопросы автоматически отвечаются через auto-answer агент
- [ ] Используется шаблон `pipeline_auto_answer.md`
- [ ] Pipeline не останавливается на вопросах

### T-5.9: Список pipeline runs

**Действия**:
```bash
nflow pipeline list
nflow pipeline list --json
```

**Критерии приёмки**:
- [ ] Показывает все pipeline runs для текущего проекта
- [ ] Каждый run содержит: id, name, goal, status, mode, iteration, created_at
- [ ] JSON-формат корректен
- [ ] Отсортированы по дате создания (новые первые)

### T-5.10: Статус pipeline

**Действия**:
```bash
nflow pipeline status <pipeline_id>
```

**Критерии приёмки**:
- [ ] Показывает детальный статус: id, name, goal, mode, status, current_stage, iteration, max_iterations
- [ ] Показывает список stages с их статусами
- [ ] Показывает pending questions (если есть)
- [ ] Несуществующий ID — `NOT_FOUND`

### T-5.11: Логи pipeline

**Действия**:
```bash
nflow pipeline log <pipeline_id>
nflow pipeline log <pipeline_id> --stage plan
nflow pipeline log <pipeline_id> --stage implement --iteration 2
```

**Критерии приёмки**:
- [ ] Без фильтров — показывает все логи
- [ ] `--stage` фильтрует по типу стадии
- [ ] `--iteration` фильтрует по номеру итерации
- [ ] Логи содержат вывод Claude для каждой стадии

### T-5.12: Отмена pipeline

**Действия**:
```bash
nflow pipeline start "Тестовая задача" --mode auto &
sleep 3
nflow pipeline cancel <pipeline_id>
```

**Критерии приёмки**:
- [ ] Pipeline переходит в `Cancelled`
- [ ] Запущенный Claude-процесс завершается
- [ ] Event `PipelineCompleted(cancelled)` транслируется
- [ ] Повторная отмена — `INVALID_STATE`

### T-5.13: Ограничение max_iterations

**Действия**:
```bash
nflow pipeline start "Задача, которая не пройдёт review" --mode auto --max-iterations 2
```

**Критерии приёмки**:
- [ ] Pipeline проходит максимум 2 цикла Implement → Review
- [ ] После 2-го неудачного review — статус `Failed`
- [ ] Итоговый `iteration` = `max_iterations`

### T-5.14: Конфликт параллельных pipeline

**Действия**:
```bash
nflow pipeline start "Задача 1" --mode auto &
sleep 1
nflow pipeline start "Задача 2" --mode auto
```

**Критерии приёмки**:
- [ ] Вторая команда возвращает `INVALID_STATE` (уже есть активный pipeline для проекта)
- [ ] Первый pipeline продолжает работу

### T-5.15: Pipeline с явным количеством итераций

**Действия**:
```bash
nflow pipeline start "Простое изменение" --mode auto --max-iterations 1
```

**Критерии приёмки**:
- [ ] С `max-iterations 1` — ровно одна попытка (Plan → Implement → Review)
- [ ] При неудаче review — сразу `Failed` (без loop back)

---

## Фаза 6: Worktree Management

### T-6.1: Список worktree

**Действия**:
```bash
nflow worktree list
```

**Критерии приёмки**:
- [ ] Показывает все nflow-managed worktrees
- [ ] Каждый worktree содержит: path, branch, head commit, story ID
- [ ] Пустой список если нет активных worktrees

### T-6.2: Очистка worktrees

**Действия**:
```bash
nflow worktree clean
nflow worktree clean --all
```

**Критерии приёмки**:
- [ ] `clean` удаляет worktrees завершённых stories
- [ ] `--all` удаляет все worktrees
- [ ] `git worktree list` подтверждает удаление
- [ ] Ветки остаются (только worktree удалён)

---

## Фаза 7: Cleanup

### T-7.1: Очистка логов

**Действия**:
```bash
nflow cleanup --logs
nflow cleanup --logs --older-than 7d
nflow cleanup --logs --dry-run
```

**Критерии приёмки**:
- [ ] `--logs` удаляет логи agent runs
- [ ] `--older-than 7d` удаляет только старше 7 дней
- [ ] `--dry-run` показывает что будет удалено, но не удаляет

### T-7.2: Полная очистка

**Действия**:
```bash
nflow cleanup --all --dry-run
nflow cleanup --all
```

**Критерии приёмки**:
- [ ] `--all` удаляет логи и временные данные
- [ ] `--dry-run` показывает план без выполнения

---

## Фаза 8: JSON-вывод и флаги форматирования

### T-8.1: Флаг --json

**Действия**:
```bash
nflow projects --json
nflow spec list --json
nflow status --json
nflow pipeline list --json
nflow config show --json
```

**Критерии приёмки**:
- [ ] Каждая команда возвращает валидный JSON
- [ ] JSON парсится через `jq` без ошибок
- [ ] Структура JSON соответствует ожидаемой схеме для каждой команды

### T-8.2: Флаг --verbose

**Действия**:
```bash
nflow --verbose spec list
nflow --verbose pipeline list
```

**Критерии приёмки**:
- [ ] Verbose-режим показывает дополнительную информацию (request IDs, timing)
- [ ] Не ломает основной вывод

### T-8.3: Флаг --no-color

**Действия**:
```bash
nflow --no-color status
```

**Критерии приёмки**:
- [ ] Вывод не содержит ANSI escape sequences
- [ ] Информация полностью присутствует

---

## Фаза 9: Обработка ошибок протокола

### T-9.1: Демон недоступен

**Действия**:
```bash
nflow daemon stop
rm -f $NFLOW_HOME/nflow.sock
nflow projects    # При отсутствии сокета
```

**Критерии приёмки**:
- [ ] CLI показывает понятное сообщение об ошибке подключения
- [ ] Или автоматически запускает демон

### T-9.2: Таймаут handshake

**Действия**: (через тестовый скрипт, создающий фейковый сокет)

**Критерии приёмки**:
- [ ] CLI корректно обрабатывает таймаут 5 секунд
- [ ] Показывает сообщение о невозможности подключиться

### T-9.3: Невалидный ответ от демона

**Критерии приёмки**:
- [ ] CLI обрабатывает malformed JSON от демона
- [ ] Не паникует, показывает ошибку

---

## Фаза 10: TUI (базовые проверки)

### T-10.1: Запуск TUI

**Действия**:
```bash
nflow tui
```

**Критерии приёмки**:
- [ ] TUI запускается и показывает главный экран
- [ ] 5 вкладок доступны: Specs, Plan, Execute, Logs, Pipeline
- [ ] Подключение к демону через Unix socket установлено
- [ ] Нажатие `q` завершает TUI

### T-10.2: Навигация по вкладкам

**Критерии приёмки**:
- [ ] Клавиши 1-5 переключают вкладки
- [ ] Tab переключает на следующую вкладку
- [ ] Содержимое каждой вкладки соответствует текущему состоянию проекта

### T-10.3: TUI с данными

**Критерии приёмки**:
- [ ] Specs tab показывает созданные спецификации
- [ ] Plan tab показывает план декомпозиции
- [ ] Execute tab показывает статусы stories/tasks
- [ ] Pipeline tab показывает pipeline runs
- [ ] Logs tab показывает логи выполнения

---

## Фаза 11: Интеграционный сценарий — полный цикл создания проекта `taskfile`

Это главный тест, объединяющий все предыдущие фазы в один непрерывный сценарий.

### Сценарий

```
1. [Подготовка]
   nflow daemon start
   cd /tmp/taskfile-project && nflow init

2. [SDD Flow — основной функционал]
   nflow spec new "taskfile-core"
   → Диалог: CLI-утилита на Rust, add/list/done/delete, tasks.json, clap+serde
   nflow spec approve "taskfile-core"
   nflow plan generate
   → Ожидается: 1 Epic, 3 Stories (models, CLI, tests)
   nflow plan approve
   nflow run
   → Ожидание завершения всех stories
   nflow status → все Done

3. [Верификация SDD-результата]
   cd <worktree-S3> или мерж веток
   cargo build → OK
   cargo test → OK
   ./target/debug/taskfile add "Test task" → OK
   ./target/debug/taskfile list → показывает задачу
   ./target/debug/taskfile done 1 → OK
   ./target/debug/taskfile list --status done → показывает завершённую
   ./target/debug/taskfile delete 1 → OK

4. [Pipeline Flow — добавление фичи]
   nflow pipeline start "Добавь команду search для поиска задач по подстроке" --mode auto
   → Ожидание: Plan → Implement → Review → Completed
   cargo build → OK
   cargo test → OK
   ./target/debug/taskfile add "Buy groceries"
   ./target/debug/taskfile add "Buy new laptop"
   ./target/debug/taskfile search "Buy" → показывает обе задачи

5. [Pipeline Manual Mode — ещё одна фича]
   nflow pipeline start "Добавь цветной вывод в list с использованием colored crate" --mode manual
   → Ожидание WaitingForApproval
   nflow pipeline questions <id>
   nflow pipeline answer <id> <qid> "Зелёный для done, белый для todo"
   nflow pipeline approve <id>
   → Ожидание: Implement → Review → WaitingForFinalApproval
   nflow pipeline approve <id>
   cargo build → OK

6. [Очистка]
   nflow worktree clean --all
   nflow cleanup --all
   nflow daemon stop
```

### Финальные критерии приёмки проекта `taskfile`

| # | Критерий | Проверка |
|---|----------|----------|
| FA-1 | Проект собирается без ошибок | `cargo build` exit code 0 |
| FA-2 | Все тесты проходят | `cargo test` exit code 0, 0 failures |
| FA-3 | Команда `add` создаёт задачу | `taskfile add "X"` → "Task created" + ID |
| FA-4 | Команда `list` показывает задачи | `taskfile list` → таблица задач |
| FA-5 | Команда `done` помечает задачу | `taskfile done <id>` → status = done |
| FA-6 | Команда `delete` удаляет задачу | `taskfile delete <id>` → задача удалена |
| FA-7 | Фильтр `--status` работает | `taskfile list --status done` → только done |
| FA-8 | Команда `search` ищет по title | `taskfile search "query"` → результаты |
| FA-9 | Файл tasks.json создаётся | `cat tasks.json` → валидный JSON |
| FA-10 | Структура задачи корректна | JSON содержит: id, title, status, created_at |
| FA-11 | Cargo.toml содержит зависимости | clap, serde, serde_json, chrono |
| FA-12 | `cargo clippy` не показывает warnings | `cargo clippy -- -D warnings` exit code 0 |
| FA-13 | `cargo fmt --check` проходит | exit code 0 |
| FA-14 | Все nflow worktrees очищены | `nflow worktree list` → пусто |
| FA-15 | Демон корректно остановлен | `nflow daemon status` → not running |

---

## Матрица покрытия команд

| Команда | Тест(ы) | Happy Path | Error Cases |
|---------|---------|:----------:|:-----------:|
| `nflow init` | T-2.1, T-2.2, T-2.3, T-2.7 | ✅ | ✅ |
| `nflow projects` | T-2.4 | ✅ | — |
| `nflow project delete` | T-2.5, T-2.6 | ✅ | ✅ |
| `nflow daemon start` | T-1.1, T-1.3, T-1.4 | ✅ | ✅ |
| `nflow daemon stop` | T-1.5 | ✅ | — |
| `nflow daemon status` | T-1.2 | ✅ | — |
| `nflow spec new` | T-4.1 | ✅ | — |
| `nflow spec list` | T-4.2 | ✅ | — |
| `nflow spec view` | T-4.2 | ✅ | — |
| `nflow spec resume` | T-4.3 | ✅ | ✅ |
| `nflow spec approve` | T-4.4, T-4.7 | ✅ | ✅ |
| `nflow spec reopen` | T-4.5 | ✅ | — |
| `nflow spec delete` | T-4.6, T-4.7 | ✅ | ✅ |
| `nflow plan generate` | T-4.8, T-4.12 | ✅ | — |
| `nflow plan show` | T-4.9 | ✅ | — |
| `nflow plan feedback` | T-4.10 | ✅ | — |
| `nflow plan approve` | T-4.11 | ✅ | — |
| `nflow plan discard` | T-4.12 | ✅ | — |
| `nflow run` | T-4.13 | ✅ | — |
| `nflow status` | T-4.14 | ✅ | — |
| `nflow log` | T-4.15 | ✅ | — |
| `nflow pause` | T-4.16 | ✅ | — |
| `nflow continue` | T-4.16 | ✅ | — |
| `nflow retry` | T-4.17 | ✅ | ✅ |
| `nflow skip` | T-4.18 | ✅ | ✅ |
| `nflow stop` | T-4.19 | ✅ | — |
| `nflow cancel` | T-4.20 | ✅ | — |
| `nflow pipeline start` | T-5.1, T-5.2, T-5.14, T-5.15 | ✅ | ✅ |
| `nflow pipeline list` | T-5.9 | ✅ | — |
| `nflow pipeline status` | T-5.10 | ✅ | ✅ |
| `nflow pipeline log` | T-5.11 | ✅ | — |
| `nflow pipeline cancel` | T-5.12 | ✅ | ✅ |
| `nflow pipeline approve` | T-5.3, T-5.5 | ✅ | — |
| `nflow pipeline reject` | T-5.4, T-5.6 | ✅ | ✅ |
| `nflow pipeline answer` | T-5.7 | ✅ | ✅ |
| `nflow pipeline questions` | T-5.7 | ✅ | — |
| `nflow config show` | T-3.1, T-3.4 | ✅ | — |
| `nflow config set` | T-3.2, T-3.3 | ✅ | ✅ |
| `nflow worktree list` | T-6.1 | ✅ | — |
| `nflow worktree clean` | T-6.2 | ✅ | — |
| `nflow cleanup` | T-7.1, T-7.2 | ✅ | — |
| `nflow tui` | T-10.1, T-10.2, T-10.3 | ✅ | — |

**Итого: 40 команд, 55 тестовых сценариев, 100% покрытие CLI.**

---

## Автоматизация тестирования

### Рекомендуемый подход

Для полной автоматизации плана используется **mock-claude** бинарник с заранее подготовленными ответами:

1. **Расширить mock-claude** новыми режимами:
   - `PipelinePlan` — возвращает JSON-план со `steps`, `affected_files`
   - `PipelineImplement` — имитирует изменения кода, возвращает `build_status: "ok"`
   - `PipelineReview` — возвращает `review.passed: true/false` (управляемо через env)
   - `PipelineAutoAnswer` — возвращает ответ на вопрос

2. **Env-переменные для новых режимов**:
   - `MOCK_CLAUDE_PIPELINE_PLAN_JSON` — предопределённый план
   - `MOCK_CLAUDE_PIPELINE_REVIEW_PASS` — true/false для результата review
   - `MOCK_CLAUDE_PIPELINE_QUESTIONS` — JSON с вопросами для Plan stage
   - `MOCK_CLAUDE_PIPELINE_ITERATION` — имитировать конкретную итерацию

3. **Тестовый скрипт** (`scripts/e2e-cli-test.sh`) который:
   - Устанавливает `NFLOW_CLAUDE_BINARY=mock-claude`
   - Создаёт временные директории
   - Последовательно выполняет все фазы 0-11
   - Проверяет каждый критерий приёмки через assert'ы
   - Генерирует отчёт в формате TAP (Test Anything Protocol)

### Альтернативный подход (без mock)

Для тестирования с реальным Claude (интеграционное тестирование):
- Запускать на CI с Claude API ключом
- Таймаут на каждую фазу: 10 минут
- Результаты непредсказуемы → проверять только структурные критерии (exit codes, JSON schema, state transitions)
- Не проверять содержимое сгенерированного кода

---

## Приоритеты

| Приоритет | Фазы | Обоснование |
|-----------|-------|-------------|
| P0 (блокер) | 0, 1, 2 | Без демона и проекта ничего не работает |
| P0 (блокер) | 4.1-4.4, 4.8, 4.11, 4.13 | Критический путь SDD flow |
| P0 (блокер) | 5.1, 5.2, 5.3 | Критический путь Pipeline flow |
| P1 (важно) | 4.5-4.7, 4.14-4.21 | Полный SDD flow + error handling |
| P1 (важно) | 5.4-5.12 | Полный Pipeline flow + interactive mode |
| P1 (важно) | 3, 8 | Конфигурация и форматирование |
| P2 (желательно) | 5.13-5.15 | Edge cases Pipeline |
| P2 (желательно) | 6, 7, 9 | Worktrees, cleanup, protocol errors |
| P3 (бонус) | 10, 11 | TUI и полный интеграционный сценарий |
