# 07 · Критика UI, раунд 2, и макет экрана аккаунтов

Смотрел: `.dev/research/ui/after-2/` (PNG, ~25 из 37 сцен) против `baseline/` (сверял `chat-working__default__120x40`).
Не смотрел глазами: `landing-typing`, `landing`/`landing-empty`/`chat-long` на 200×60, `landing` в gruvbox/paper, `dock-agents` 120×40, `chat-working-ascii` 120×40.
Код не менял и не запускал. Имена функций ниже взяты из `src/tui/**` (grep по именам), тела читал только у `composer.rs`, `popup.rs`, `handle.rs`, `types.rs`.
Данные сцен искусственные (`capture.rs`), анимация — один кадр, шрифт — Cascadia/Consolas из HTML, не реальный терминал.
Сравнений с другими программами не делал: всё, что звучит как «у других так», — предположение.

---

## 1. Оценки (1–10)

| Направление | Оценка | Почему |
|---|---|---|
| Первое впечатление | **6** | Чат 7, главная 5: на `landing__default__120x40` 28 пустых строк из 40, список сессий без курсора, подсказки продублированы. |
| Иерархия | **7** | Один скелет, ввод внизу, правило-заголовок, футер. Слабо: заголовок ответа (`pooprusteek · deepseek-chat`) и мета-строка шумят на каждом сообщении. |
| Ясность «что происходит сейчас» | **8** | Док (инструмент, суб-агент, очередь, GOAL) — самое сильное улучшение. Минус: 6 агентов превращаются в `+5 more agents` (`dock-agents__default__80x24`). |
| Читаемость вывода инструментов | **6** | Компактно, но показана голова вывода, а итог теста (`2 passed; 1 failed`) спрятан в `… 7 more lines`; `✓` стоит у упавшей команды. |
| Плотность | **7** | Служебных строк 3–7 вместо 7 всегда. На 80×24 при работе док+правило+футер = 7 строк из 24. |
| Цвет | **7** | Роли читаются во всех темах на кадрах (nord, dracula, paper, matrix). Контраст глазами, не расчётом. Матрица: ✓, имя инструмента и заголовок одного зелёного. |
| Консистентность экранов | **5** | Три манеры выделения строки, три манеры рамки, инвертированный фокус в `/themes`, `?` вместо значка поиска. |
| Узкое окно | **7** | 60×20 держится: футер роняет `mcp` и время, но оставляет `ctx`. Слабо: док (`next: … пуст… +5 more agents`) и заголовок модалки (`--noca…`). |

---

## 2. Топ-10 правок (по убыванию влияния)

Размер: **быстро** — до ~30 строк, один файл; **большая** — несколько файлов или новый общий кусок.

| № | Правка | Кадр | Где | Почему | Строк | Размер |
|---|---|---|---|---|---|---|
| 1 | **Блок инструмента: хвост и итог.** Для `bash` показывать последние 3 строки (не первые), в заголовке — время и код выхода; `✓` только если код 0, иначе `✗ exit 101` в цвете ошибки | `chat-working__default__120x40` (`✓ bash` над `… FAILED`) | `widgets/chat.rs::tool_lines` | Сейчас человек видит зелёную галку над упавшим тестом, а строка «2 passed; 1 failed» скрыта. Это главный смысл вывода | ~40 | быстро–средне |
| 2 | **Главная: не пустота, а рабочий экран.** Курсор и номера у сессий (`1–9` открыть), убрать `tips_line` (дублирует правило: `/ commands` дважды), добавить строку «аккаунты» (см. §6) | `landing__default__120x40`, `…80x24` | `render/welcome.rs::plan`, `session_rows`, `tips_line` | Первый кадр релиза. Сейчас это карточка и 4 строки; главная фича (подписки, лимиты) её не касается | ~60 | большая |
| 3 | **Док не прячет параллельную работу.** `MAX_DOCK_ROWS = 4` при любой высоте: на 40+ строках дать до 8; `+N more` вынести из строки `queued` на свою строку или в конец ряда агентов | `dock-agents__default__80x24`, `chat-working__default__60x20` | `render/dock.rs` (`MAX_DOCK_ROWS`, `queue_line`, `child_line`) | При 6 агентах видно одного. Для режима «много подписок и суб-агентов» это дыра | ~40 | средне |
| 4 | **Окно подтверждения.** Заголовок `Tool call · bash · <команда…>` дублирует тело и режется (`--stat…`, `--noca…`) → `Allow bash?` + `from sub-agent: refactor tests`. Радио `● Just this once / ○ Always` и клавиши `Y/N/A` — две схемы управления одновременно: оставить одну. `ANYTHING` заглавными → `any command` | `approval__default__120x40`, `…80x24` | `render/approval.rs::title_line`, `grant_rows`, `key_hints` | Решение о безопасности; сейчас непонятно, что нажать и что значит `ANYTHING` | ~40 | быстро–средне |
| 5 | **Один язык выделения строки.** Сейчас: `/providers` — заливка + `●`, `/mcp` — заливка + `●` + слово, `/models` — `▶` + заливка, `/themes` — `▶` цветом без заливки, автодополнение — `▶` + слабая заливка. Свести к одному `selection_row` (маркер `▌` + `theme.selection`, текст не перекрашивать) | `providers`, `mcp`, `models`, `themes-gallery`, `autocomplete` (все 120×40) | `render/chrome.rs` (новый хелпер), `providers.rs`, `mcp.rs`, `themes.rs`, `modals.rs` | Главная причина оценки 5 по консистентности; хелпер понадобится экрану аккаунтов | ~60 | большая |
| 6 | **Широкий экран.** Транскрипт зажат на ~110 колонок (хорошо), а правило ввода, футер и правые метки дока (`0:47 · 1.8k tok · 39 t/s`) растянуты на все 200: метка в 150 колонках от названия. Панель справа статична (`Rate off`, `Session 2026-11-06T14:02-..`) | `chat-working-wide__default__200x60` | `render/layout.rs`, `dock.rs::two_sided`, `footer.rs`, `widgets/panel.rs` | Потолок ширины нужен всему «рабочему столбцу», а не только тексту; панель — место для мини-списка аккаунтов вместо `Rate off` | ~60 | большая |
| 7 | **Заголовки в markdown.** `## Почему тест падает` рисуется с решётками и тем же цветом, что тело | `chat-long__default__120x40`, `autocomplete__default__120x40` | `markdown.rs` (`Tag::Heading`, ~стр. 167) | Выглядит как необработанный текст. Убрать `#`, дать `accent` + bold, H1 — подчёркивание | ~15 | быстро |
| 8 | **Идущий ответ без признака «пишу».** Последнее сообщение кончается словом `потом`, жёлтая полоска слева — единственный намёк, спиннер только в доке | `chat-working__default__120x40` | `widgets/chat.rs::assistant_header` / `build_segment_lines` | Глаз смотрит в конец текста; там должен мигать `▍` | ~10 | быстро |
| 9 | **Ритм транскрипта.** Мета-строка (`14:03:02  612t  think 12.3s`) сдвинута на 1 колонку правее текста; два пустых ряда между кодом и таблицей, один — между остальными блоками; `612t` без пояснения | `chat-long__default__120x40` | `widgets/chat.rs::meta_line`, `compact_lines`; `markdown.rs` (отступы блоков) | Мелочь, но видна на каждом сообщении | ~20 | быстро |
| 10 | **Мелочи экранов.** `?` вместо значка в фильтре моделей и поиске; в поиске пустое состояние ровно 80 знаков без полей и лишняя `·` после `unique [off]`; в `/mcp` статус дважды (`ON` + `●`) и `2t` без расшифровки | `models__default__120x40`, `search__default__80x24`, `mcp__default__120x40` | `render/modals.rs` (фильтр), `search.rs::render_results`, `mcp.rs` | Каждая по минуте работы, вместе — ощущение недоделанности | ~25 | быстро |

---

## 3. Настоящие баги отображения

| № | Что | Кадр | Проверено |
|---|---|---|---|
| D1 | Под модалкой **не затемнены** док, правило ввода и футер: на кадре они яркие, транскрипт тёмный. `dim_backdrop` зовётся с полной `area` (`render/mod.rs`), красит только RGB-цвет текста (`popup.rs::dim_color`) и не трогает фон. Почему нижние ряды остались яркими, **не нашёл** | `approval__default__120x40`, `…80x24` | кадр — да; причина — нет |
| D2 | `+1 more agents` / `+5 more agents` сидит на строке `queued 1 · next: …` — читается как «ещё 5 в очереди» | `chat-working__default__120x40`, `dock-agents__default__80x24` | да |
| D3 | Панель на 200×60: `Speed` показывает спиннер `⠇` вместо числа (в доке `39 t/s`); `Session 2026-11-06T14:02-..` — обрезанный штамп; `mcp linear: authorization requ` обрезан без `…` (это B8 из раунда 1, остался) | `chat-working-wide__default__200x60` | да |
| D4 | Окно автодополнения шириной 76 колонок оставляет справа обрывок реплики (`рый тест заменён на`) — текст за окном не затемнён | `autocomplete__default__120x40` | да |
| D5 | `/themes`: заголовок `Gallery` тусклый, как у неактивной панели, хотя курсор в ней; яркая рамка у `Preview`. Фокус выглядит перевёрнутым | `themes-gallery__default__120x40` | да |
| D6 | `?` как значок поиска (в `.txt` буквально `? type to filter...`) — читается как «справка» | `models__default__120x40`, `search__default__80x24` | да, по `.txt` |
| D7 | Поиск 80×24: текст `Type a query and press Enter — semantic + keyword search over all saved sessions` ровно 80 колонок, вплотную к краям; после `unique [off]` висит `·` | `search__default__80x24` | да |
| D8 | `/mcp`: колонка статуса показывает `ON` и ещё отдельную точку без заголовка; `AUTH ⚠` | `mcp__default__120x40` | да |
| D9 | `✓` у `bash` над `FAILED` в выводе: галка означает «инструмент отработал», а не «команда успешна» | `chat-working__default__120x40` | да (сцена искусственная, но логика та же) |
| D10 | В ASCII-режиме успех = `+`, который читается как «добавлено» (как в diff) | `chat-working-ascii__default__80x24` | да |
| D11 | Футер `83% of 38k`, а строка в чате `Context 38% (16.2k / 42k)`. Строка вписана руками в `capture.rs:293` — артефакт сцены. Но футер делит на **бюджет** (`footer.rs:144`, `budget_from_config`), а не на окно, и подпись этого не говорит. Что 38k — бюджет, а не окно, — **предположение** (значение не проверял) | `chat-working__default__120x40` | частично |
| D12 | В приглушённом состоянии подсказка в правиле — `@ file`, а на главной строкой выше — `@file attach`. Это не обрезка (в `composer.rs::hint_variants` вариант именно такой), но два написания одного и того же | `landing__default__80x24` | да |

Не баг, а что проверил и снял: подсказка в правиле **не режется** посреди слова — `composer.rs::rule_text` подбирает целый вариант от длинного к короткому.

---

## 4. Что стало заметно лучше и что нельзя трогать

Лучше, чем в `baseline` (сравнение `chat-working__default__120x40`):
- Три цветные плашки на инструмент и заливка тела ушли: теперь `✓ имя` + полоса слева + 3 строки вывода.
- Статус-бар на 130 знаков с обрезанной правой частью заменён футером с приоритетом сегментов; на 60×20 остаётся `ctx`.
- Поле ввода внизу и на главной, и в чате; правило с подписью `message · 1 queued` и подсказками по состоянию.
- Док: инструмент, дерево суб-агентов, очередь, GOAL, ошибки. Ноль строк в простое (по коду `dock::height`, на кадрах простой не смотрел).
- Таблица markdown с колонками, чекбоксы, подпись языка у кода, цитата с полосой.
- Окно подтверждения: аргументы переносятся полностью, указан источник (`From: sub-agent`), фон приглушён.
- Режим ASCII-глифов работает целиком (`chat-working-ascii`).
- Транскрипт ограничен по ширине (виден на 200×60).

**Не трогать:** структуру дока (строки, порядок, дерево `├ └`); правило ввода с состоянием в подписи и подсказке; `fit_segments` в футере (приоритет сегментов); `glyphs.rs` как единую точку глифов; полный перенос аргументов в подтверждении; превью тем со списком ролей-цветов; кэш строк чата.

---

## 5. Одна смелая идея: «лента маршрута» и «шов»

Когда у пользователя больше одного аккаунта, внизу, в футере, живёт тонкая лента маршрута:

```
 route default   ● claude-max ▸ ◔ opencode-go ▸ ● deepseek                    ctx ▓▓▓▓▒▒ 52%   14:02
```

- Активный шаг подсвечен (`accent`), остальные — приглушены, точка показывает состояние (`●` готов, `◔` остывает, `✕` выбыл).
- Когда маршрутизатор переключается, на **одном** кадре лента сдвигает подсветку, а в транскрипт встаёт тонкий «шов»:

```
 ── claude-max → opencode-go · rate limit, back in 2m 10s ───────────────────────
```

- В заголовке каждого ответа — модель и аккаунт: `pooprusteek · opus-5-5 via claude-max`.

Почему работает и не мешает: одна строка, которую уже занимает футер; без нескольких аккаунтов её нет вообще; на 80 колонок остаётся только активный шаг и `+2`. В журнале разговора видно, кто и когда отвечал — это то, что нужно, когда два аккаунта дают разные ответы. Данные уже есть: `Decision.target`, `Decision.why`, `AccountView.status`.

---

## 6. Макеты экрана аккаунтов

Опирался на реальные типы: `AccountView` (`id, label, provider, enabled, status, limits, in_flight, max_concurrency, default_model, favorites`), `AccountStatus` (`Ready, Cooling, Exhausted, NeedsAttention, Disabled, Unavailable`), `LimitSnapshot` (`windows`, `observed_at`, `source`), `Route` (`steps`, `on`, `switch_at_pct`), `Roles` (`main, title, summary, evaluator`), `Decision.explain()` и тексты `SkipReason::text`. Данные в макетах выдуманы.

### 6.1 Принципы

1. **Порядок = приоритет** (стратегия `Ordered`): номер 1…N всегда виден, `K/J` двигает аккаунт (`ConfigEdit::MoveAccount`). Это та самая «настройка маршрута» без отдельного редактора.
2. **Статус — глиф + слово**, не только цвет: `● ready`, `◔ cooling 2m 10s`, `✕ exhausted · resets 3h`, `! login expired`, `○ off`, `– unavailable: claude not found`. Цвета: success / warning / error / text_dim.
3. **Лимит всегда с возрастом и источником.** Сплошная шкала `█` — свежее показание (`Header` / `CliEvent`, `is_hard_signal`); штриховая `▒` и префикс `~` — оценка (`Ledger` / `Declared`); `stale` и приглушённый цвет — старше TTL. Так «жёсткое» не путается с прикидкой, как и в движке.
4. **Худшее окно первым**; остальные — на следующих строках.
5. **Решение объясняется на экране**, а не только в `/route why`: последняя строка `explain()` и список `skipped` с причинами (`SkipReason::text`).
6. Тип — `API` / `CLI`; для встроенного DeepSeek — `WEB` (третий тип, в `AccountProvider::DeepseekWeb`).

### 6.2 120 колонок

```
 Accounts & routes                                            gateway ON · 3 ready · 1 cooling · 1 needs login
────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
   #  ACCOUNT       USER                     TYPE STATUS            LIMIT WINDOWS                        SOURCE · AGE
 ▌ 1  claude-max    alex@example.com         CLI  ● in use 1/1      5h    █████░░░░░  52%  resets 1h 12m     live · 5 s
      ★ opus-5-5   ★ sonnet-5-5                                     week  ██░░░░░░░░  18%  resets 4d 03h     live · 5 s
   2  opencode-go   alex+oc@example.com      CLI  ◔ cooling 2m 10s  5h    █████████░  91%  resets 38m       live · 41 s
      ★ deepseek-v4.1-flash                                         week  ████░░░░░░  40%  resets 2d 11h    live · 41 s
      rate limit hit twice in a row
   3  anthropic-api org acme-dev · key …a91f API  ● ready           rpm   ██░░░░░░░░  18%  resets 24 s      live · 3 s
      ★ claude-sonnet-5-5                                           spend ▒▒▒▒░░░░░░ ~37%  $18 of $50/mo  est
   4  deepseek      web session · token set  WEB  ● ready           no limit data · declared: 1 call at a time
      ★ deepseek-chat   ★ deepseek-reasoner
   5  claude-work   alex@work.example        CLI  ! login expired   last reading 2 d old · stale
      Enter: log in again

─ ROUTES ─ fall to the next step on: rate_limit quota overloaded timeout server auth · switch early at 85% ─────────
  ▶ default   1 ● claude-max/opus-5-5  →  2 ◔ opencode-go/flash  →  3 ● anthropic-api/sonnet-5-5  →  4 ● deepseek/chat
    cheap     1 ● deepseek/chat  →  2 ● opencode-go/flash
    ROLES     main → default   title → cheap   summary → cheap   evaluator → default

─ LAST DECISION ─ 14:02:31 · main · limits read 5 s ago ──────────────────────────────────────────────────────────────
  ✔ claude-max · opus-5-5 — first ready step
    skipped   opencode-go    cooling down after a rate limit — back in 2m 10s
    raw       claude-max · opus-5-5: first ready step; skipped opencode-go: cooling down after a rate limit

────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 ↑↓ select · Enter details · K/J move · Space on/off · p pin chat here · e explain · h history · a add · Esc back
```

Цвет и стиль (в ASCII не видны): `▌` и строка выбранного аккаунта — `theme.selection`; `●` success, `◔` warning, `!` error; шкала до `soft_pct` — success, выше — warning, выше `hard_pct` — error; `★` — accent; `skipped` и `raw` — `text_dim`; текущий шаг маршрута (`▶`, имя) — accent.
Высота 27 строк: на 40 строках остаётся место для 6–7 аккаунтов; больше — прокрутка списка, нижние блоки не сжимаются.

### 6.3 80 колонок

```
 Accounts                                   3 ready · 1 cooling · 1 login
────────────────────────────────────────────────────────────────────────────────
 ▌1 claude-max    CLI  ● in use 1/1
     5h   █████░░░░░  52%  ↻1h12m  live 5s
  2 opencode-go   CLI  ◔ cooling 2m10s
     5h   █████████░  91%  ↻38m    live 41s
  3 anthropic-api API  ● ready
     spend ▒▒▒▒░░░░░░ ~37%  $18 of $50  est
  4 deepseek      WEB  ● ready
     no limit data
  5 claude-work   CLI  ! login expired
     last reading 2d old · stale
────────────────────────────────────────────────────────────────────────────────
 default  ▶1 ● claude-max → 2 ◔ opencode-go → 3 ● anthropic-api → 4 ● deepseek
 roles    main → default · title, summary → cheap · evaluator → default
────────────────────────────────────────────────────────────────────────────────
 14:02:31 ✔ claude-max · opus-5-5 — first ready step
   skipped opencode-go: cooling down after a rate limit (2m10s)
────────────────────────────────────────────────────────────────────────────────
 ↑↓ select · Enter details · K/J move · Space on/off · e explain · Esc back
```

На 80 колонках: два ряда на аккаунт (имя + статус; худшее окно). Пользователь, избранные модели и остальные окна — в `Enter` (ниже). Ниже 60 колонок: один ряд `1 claude-max ● 52%`, блоки маршрута и решения — по `Tab`.

### 6.4 Подробности по `Enter` (120 колонок, ниже списка вместо блока маршрутов)

```
─ claude-max ─ CLI · Claude Code · alex@example.com ──────────────────────────────────────────────────────────────────
  status     in use 1/1 · last success 14:02:31 · last failure never
  limits     5h   █████░░░░░ 52%   soft 70 · hard 90   resets 1h 12m   live (CLI event) · 5 s ago
             week ██░░░░░░░░ 18%                       resets 4d 03h   live (CLI event) · 5 s ago
  today      41 calls · 1.2M tok in · 96k out · cache read 78% · 0 fallbacks
  favorites  ★ opus-5-5   ★ sonnet-5-5   (f: edit)
  used by    default (step 1) · cheap (—)
```
`today` берётся из `Ledger::totals` (`LedgerRow`: вызовы, сбои, fallback, токены, кэш); `soft/hard` — из `AccountLimits`.

### 6.5 Пустое и ошибочное состояние

- Нет аккаунтов: `No accounts yet. a add API key · c connect Claude Code / OpenCode · w DeepSeek web`.
- Шлюз выключен (`enabled = false`): шапка `gateway OFF — the old single-provider mode is on` и список только для чтения.
- Ошибка конфига (`Problem`): строка `! route "default" is off: <message>` над маршрутами, шаги серые.
- Выбирать некого (`Why::RouteExhausted`): в блоке решения `✕ no account available · next one back in 2m 10s` (`Decision.retry_at`).

### 6.6 Чего в данных сейчас нет (нужно для макета)

| Что на макете | Есть ли в `AccountView` / `Decision` | Что делать |
|---|---|---|
| **USER** (`alex@example.com`, `org acme-dev · key …a91f`) | **Нет.** Есть `label` и `id`; ни логина, ни организации, ни хвоста ключа | Добавить `identity: Option<String>`; источник у CLI — не проверял (`06-cli-spike.md` я не читал), у API — организация из ответа или последние 4 знака ключа. До этого показывать `label` |
| `resets 1h 12m`, `live · 5 s` | Да: `LimitWindow.resets_at`, `LimitSnapshot.age()`, `LimitSource` | Рисовать как есть |
| `~37% $18 of $50/mo est` | Частично: `budget_usd_month` есть, расход — `Ledger::totals` | Свести в `LimitWindow { kind: Spend, source: Ledger }` |
| `back in 2m 10s` в `skipped` | **Нет** в `SkipReason::text` (текст — `cooling down after a rate limit`) | Дописать срок из `SkipReason::Cooling { until }` |
| `rate limit hit twice in a row` | Да: `AccountStatus::Cooling { strikes }` | Рисовать при `strikes ≥ 2` |
| `in use 1/1` | Да: `in_flight`, `max_concurrency` | — |
| `p pin chat here` | `Why::Pinned` и `/pin` в движке есть, экранной команды не проверял | Привязать к `DecisionQuery.pinned` |
| `h history` | `GatewaySnapshot.recent` — да | Список с `explain()` на строку |

### 6.7 `/providers` и аккаунты

Сегодняшний `/providers` (`providers__default__120x40`) показывает имя, `[openai-compat]`, адрес и `key set` — без состояния, лимитов и порядка. Это частный случай нового экрана. Рекомендую: `/providers` становится алиасом `/accounts`, а в старых строках остаётся только то, чего нет в новом (адрес `base_url`) — в `Enter`.

---

## 7. Ограничения

- Контраст оценён глазами по кадрам, не расчётом (расчёт — в `01-ui-critique.md`).
- Глифы в реальном conhost не проверял (в HTML они из Cascadia/Consolas): B5 из раунда 1 может по-прежнему ломать `◔ ▒ ★ ▌ ▶` в макетах §6; в ASCII-режиме нужны замены (`◔` → `~`, `▒` → `+`, `★` → `*`, `▌` → `>`) — добавить в `glyphs.rs`.
- Макеты выровнены скриптом по ширине в символах (ровно 120 и 80), но реальная ширина ячейки у `★ ● ◔ ▶ ↻ ✔` зависит от шрифта.
- Цвета и анимации в ASCII-макетах не показаны, описаны текстом.
