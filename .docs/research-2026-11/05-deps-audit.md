# Аудит зависимостей (2026-11-06)

Читательский аудит: `Cargo.toml`, `Cargo.lock` и код не менялись. Версии «последняя» —
crates.io API на 2026-11-06. `cargo outdated` / `machete` / `udeps` / `audit` не
установлены и не ставились; уязвимости проверены через OSV (`querybatch` по 593
registry-пакетам из `cargo metadata`) — это тот же RustSec-фид, но не `cargo audit`.

Обозначения: **[ф]** — проверено инструментом/исходником, **[о]** — оценка по числам,
**[п]** — предположение, требует проверки сборкой.

Сборка в этой оболочке невозможна: `link.exe` не находит `kernel32.lib` (Windows SDK
не в `PATH`), поэтому размер бинарника и время сборки не замерены — только число
крейтов (замерено), см. §2.

## 0. Снимок дерева

| Метрика | Значение |
|---|---|
| Крейтов в `Cargo.lock` (все платформы) | 593 **[ф]** |
| Крейтов в графе текущей платформы (`cargo tree --prefix none`, уникальных) | 402 **[ф]** |
| Имён с более чем одной версией | 43 **[ф]** |
| Прямых зависимостей | 42 (включая target-таблицы) **[ф]** |
| MSRV в `Cargo.toml` / локальный rustc | 1.91 / 1.99.0 **[ф]** |

Главный ограничитель почти всех крупных апгрейдов — объявленный MSRV 1.91: он же
зашит в `sandbox/Dockerfile` (`rust:1.91-trixie`), то есть нарушение ломает не только
обещание, но и песочницу CI. Цепочка MSRV у ключевых крейтов **[ф]**:

```
wasmtime: 27→1.80  36→1.86  41→1.90  42,43→1.91  44→1.92  48→1.95  50→1.97
ort:      rc.12,rc.13→1.88  rc.14,rc.15→1.92
keyring:  4.1.3..4.2.0→1.88
```

## 1. Таблица прямых зависимостей

Столбец «правки» — файлы, где зависимость реально используется (по `grep`).

### 1.1 TUI

| Зависимость | Сейчас | Последняя | Тип | Breaking / что даёт | Риск и объём правок |
|---|---|---|---|---|---|
| `ratatui` | 0.30.2 | **0.30.2** | — | Обновляться некуда. 0.30.2 — патч к 0.30: Termina-бэкенд, thread-safe shadows, «uncovered cells» в `Buffer`. Наш код (19 файлов, `tui/**`, `app/themes.rs`) не затрагивает | Нет |
| `crossterm` | 0.29.0 | **0.29.0** | — | 0.29 — последняя. Следующий релиз (в апстриме «Unreleased») — крупный breaking: `Event` теряет `Copy`, `Event`/`KeyCode`/`MouseEventKind`/`MouseButton`/`KeyEvent`/`MouseEvent` становятся `#[non_exhaustive]`, MSRV 1.85 **[ф]**. Наш `match` по `Event` в `app/mod.rs`, `app/keys/**`, `app/paste_burst.rs` (18 файлов) тогда потребует `_ =>` и `.clone()` | Сейчас — нет. К следующему релизу — механическая правка 18 файлов; планировать отдельным шагом |
| `textwrap` | 0.16.2 | 0.16.4 | patch | Патч ничего не даёт. См. §2.1 — предлагается убрать | 1 вызов |

Bracketed paste и мышь уже реализованы (`tui/mod.rs`, `app/paste_burst.rs`,
`app/keys/mouse.rs`) **[ф]** — из новых API crossterm нам ничего не нужно.

### 1.2 HTTP/сервер

| Зависимость | Сейчас | Последняя | Тип | Breaking / что даёт | Риск и объём правок |
|---|---|---|---|---|---|
| `reqwest` | 0.13.4 | **0.13.5** | patch | `Error::is_dns()` (классификация сетевых сбоев), `http1_max_headers`, TLS-версия в `TlsInfo`, фикс proxy-auth при нескольких прокси **[ф: changelog]**. Остальные фичи (pool/keepalive/http2) появились раньше и уже доступны в 0.13.4 | 1 строка в классификаторе ошибок провайдера (см. §3.2). Компиляция без правок: 18 файлов, API не менялся |
| `hyper` | 1.10.1 | **1.12.0** | minor | 1.12: `max_header_size` для сервера/клиента, серия фиксов http1 (content-length vs transfer-encoding, keep-alive/close, hop-by-hop) и http2 **[ф: changelog]**. Прямо применимо к `/serve` | Опционально 1 строка в `server/http.rs:165` (`.max_header_size(...)`). Фиксы — бесплатно |
| `hyper-util` | 0.1.20 | 0.1.21 | patch | — | Нет |
| `http-body-util` | 0.1.3 | 0.1.5 | patch | — | Нет |

**Важно:** `cookies`, `multipart`, `form` в `reqwest` **нельзя** урезать — все три
используются: `mcp/http.rs:25` (`cookie_store(true)`), `mcp/oauth.rs:455` (`.form`),
`provider/deepseek/endpoints.rs:368-401` (`multipart::Form` для `/attach`) **[ф]**.
Замер: урезание этих трёх фич экономит 18 крейтов **[ф]** — но ломает три механики.

`reqwest` 0.12.28 в `Cargo.lock` — **не в графе сборки** (`cargo tree -i` пуст):
это `hf-hub` 0.5 (ветка `reqwest` вместо `ureq`), фича не включена. Только вес lock-файла **[ф]**.

### 1.3 wasmtime (PoW)

Решение «PoW через upstream wasm, нативный SHA-3 отвергнут» не пересматриваю: оно
зафиксировано (`.memories/STATE.md:90`, `AUDIT-2026-07-02.md:66`) и новых аргументов
против него нет. Но версия 27 — единственная в 27-й линии и **вся линия EOL** **[ф]**.

| Кандидат | MSRV | Крейтов (min-фичи) | RUSTSEC на эту версию |
|---|---|---|---|
| 27.0.0 (сейчас, default-фичи) | 1.80 | 183 | **36** **[ф: OSV]** |
| 27.0.0 с `default-features = false` | 1.80 | 110 | 36 (те же) |
| **36.0.20** | 1.86 | **96** | **0** **[ф]** |
| 43.0.2 | 1.91 | 93 | 10 (линия уже не патчится) |
| 50.0.1 | 1.97 | 104 | 0, но MSRV 1.97 > 1.91 |

`36.0.20` — единственная линия, которая одновременно укладывается в MSRV 1.91 и не
имеет открытых advisories: wasmtime бэкпортит фиксы в 36 (последний — 2026-10-29 **[ф]**),
а 37–47 фиксов не получают **[ф: ranges в OSV: `fixed=48.0.8` для 37.x-47.x]**.

API-совместимость проверена по исходникам 36.0.20 и 43.0.0 **[ф]**: `Engine::default`,
`Module::from_binary`, `Store::new`, `Linker::new/func_wrap/instantiate`,
`Instance::get_memory/get_typed_func`, `Memory::read/write` — все на месте, сигнатуры
те же. То есть `src/provider/pow.rs` (единственное место, 250 строк) при переходе на
36 или 43 не меняется.

Что даёт кроме безопасности **[о]**: 183 → 96 крейтов (−87), уходят
`wasmtime-cache` (а с ним `toml` 0.8, `base64` 0.21, `zstd`+`zstd-sys` — 12 advisories,
`fxprof-processed-profile` → `fxhash`, `wasmparser` 0.219), `component-model`,
`async`, `gc`, `coredump`, `debug-builtins`, `profiling`, `backtrace`
(а `backtrace` тянет `addr2line`/`gimli`/`object`/`miniz_oxide` — самая тяжёлая часть
бинарника после cranelift). Риск: был бы «проверить, что wasm-bindgen-блоб
инстанцируется»; сейчас такого теста нет (§6, шаг 0).

**Отдельно:** `Cargo.toml` использует `wasmtime = "27"` с дефолтными фичами ради
одного синхронного вызова. Даже без смены версии `default-features = false,
features = ["std", "cranelift", "runtime"]` даёт те же 110 крейтов и работает
(все нужные API входят в `runtime`; `std` в `runtime` не входит — проверено по
`Cargo.toml` 43) **[ф]**.

### 1.4 keyring и обвязка

**Баг 4.1.3 исправлен вышестоящими — обход можно убирать** **[ф]**:

- 4.1.3 **yanked** апстримом (release notes: «DOA bug #335»).
- 4.1.4 (2026-07-06) — фикс: сравнение стало `== Ok(false)` вместо `== Ok(true)`;
  в исходнике 4.1.4/4.1.5 именно так.
- 4.1.6 (2026-08-01) — переписали на `LazyLock<Result<()>>` (фикс гонки
  инициализации #343) и добавили `Entry::store_status()`.
- 4.2.0 (2026-08-29) — текущая; из функционального только зависимости `cli`-фичи.
  Фича `v1` и API `Entry` (`new/set_password/get_password/set_secret/get_secret/
  delete_credential`) не менялись **[ф: исходники 4.1.3 vs 4.2.0]**.

Что убирается вместе с обходом: `keyring-core = "1"` и три target-зависимости
(`windows-native-keyring-store`, `apple-native-keyring-store`,
`zbus-secret-service-keyring-store`) — их и так тянет фича `v1` самого `keyring`
**[ф: `keyring-4.2.0/Cargo.toml`]**. Плюс в `src/mcp/oauth_store.rs` удаляются
`ensure_default_store()` (19 строк) и его вызов в `entry()` (строка 56); шапка-эссе
на 15 строк в `Cargo.toml` и 19 строк doc-комментария тоже уходят.

Порядок безопасен: `keyring_core::set_default_store` просто заменяет стор под
`RwLock` и не паникует при повторном вызове **[ф: `keyring-core-1.0.0/src/lib.rs`]**,
так что даже переходный период «обход + исправленный keyring» не ломается.

### 1.5 Остальное

| Зависимость | Сейчас | Последняя | Тип | Что важно | Правки |
|---|---|---|---|---|---|
| `tokio` | 1.52.3 | **1.54.1** | minor | **Безопасность:** RUSTSEC-2026-0386 (`AsyncFd::new` нарушает I/O safety), наша версия попадает в диапазон `1.52.0..1.53.3`. `AsyncFd` мы не используем (unix-only), но фикс бесплатный. 1.54: MSRV 1.85, `Builder::max_io_events_per_busy_tick`, `Semaphore::blocking_acquire`, `watch::Receiver::is_closed` | Нет |
| `toml` | 1.1.2 | 1.2.1 | minor | Только производительность сериализации/десериализации и фиксы (`1.1.7`: дубли ключей в `Table`) **[ф: changelog]**. API (`from_str`, `to_string_pretty`, `toml::de::Error`) не менялся | Нет. 3 файла: `config/mod.rs`, `harness/mock.rs`, `harness/scenario.rs` |
| `pulldown-cmark` | 0.13.4 | **0.13.6** | patch | **Безопасность:** RUSTSEC-2026-0414 (0.13.5) и -0433 (0.13.6) — рост/квадратичность на `ENABLE_WIKILINKS`. Мы эту опцию не включаем (`tui/markdown.rs` включает только `STRIKETHROUGH`+`TABLES`) → не эксплуатируется **[ф]**, но бамп бесплатный | Нет |
| `syntect` | 5.3.0 | 5.3.0 | — | Обновляться некуда. Фичи урезаются, см. §2.1 | 1 файл, `tui/markdown.rs` |
| `async-trait` | 0.1.89 | 0.1.92 | patch | **Убрать нельзя.** Нативные `async fn` в трейтах не dyn-совместимы, а у нас `Arc<dyn LLMProvider>` (15+ мест) и `Arc<dyn Tool>` (3 места: `tools/registry.rs`) **[ф]**. Без `async-trait` пришлось бы вручную боксировать future в каждом методе | Нет |
| `dirs` | 6.0.0 | **7.0.0** | major | API тот же для всего, что мы зовём (`data_dir`, `config_dir`, `home_dir`), Windows-бэкенд тот же (`known_folder_roaming_app_data`) **[ф: исходники 7.0.0]**. Ломающих изменений для нас не видно | Ожидаемо 0 строк. 6 файлов |
| `base64` | 0.22.1 | **0.23.1** | breaking (0.x) | Аддитивно: новые const-движки, SIMD за дефолтной фичей `simd-unsafe`, кастомный padding, MSRV 1.71 **[ф: RELEASE-NOTES]**. `general_purpose::STANDARD/USER_SAFE_NO_PAD.encode` без изменений | 0 строк ожидаемо. 3 файла: `mcp/oauth.rs`, `provider/pow.rs`, `semantic/history.rs` |
| `sha2` | 0.10.9 | **0.11.1** | breaking (0.x) | Переезд на `digest` 0.11: «type aliases → newtypes», `std`-фичи больше нет, MSRV 1.88 **[ф: changelog]**. `Sha256::digest/new/update/finalize` сохраняются, но `finalize()` возвращает новый тип-обёртку | 2 файла, 4 места: `mcp/oauth.rs:257` (PKCE, `encode(digest)`), `update/mod.rs:208-234` (hex). Риск: `AsRef<[u8]>` у нового типа или `hex`-форматирование. Тест-вектор `sha256_hex_matches_known_vector` уже есть — он и поймает |
| `windows-sys` | 0.60.2 | **0.61.2** | breaking (0.x) | Проверено по исходникам 0.61.2: все используемые символы на месте в тех же модулях (`GlobalMemoryStatusEx`, `MEMORYSTATUSEX`, `SetStdHandle`, `STD_ERROR_HANDLE`, `CreateJobObjectW`, `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, `CreateMutexW`, `CREATE_NO_WINDOW`, `DETACHED_PROCESS`, `RegOpenKeyExW`/`RegQueryValueExW`/`RegSetValueExW`/`RegCloseKey`, `HKEY_CURRENT_USER`, `KEY_QUERY_VALUE`/`KEY_SET_VALUE`, `REG_SZ`) **[ф]** | 0 строк ожидаемо, 5 файлов. Одна копия `windows-sys` в дереве схлопывается |
| `fastembed` | 5.17.2 | **7.6.1** | 2 major | См. §1.6 | 1 файл |
| `portable-pty` | 0.9.0 | 0.9.0 | — | Обновляться некуда; тянет `filedescriptor` → `thiserror` 1 (второй экземпляр в дереве, не исправить) | Нет |
| `uuid` | 1.23.4 | 1.29.0 | minor | Только `v4`, API стабилен | Нет |
| `regex` | 1.12.4 | 1.13.1 | minor | Ничего нужного; 18 файлов парсера тул-коллов | Нет |
| `chrono` | 0.4.45 | 0.4.45 | — | Актуальна. Фича `serde` **похоже не нужна**: `DateTime`-поля (`tools/timer.rs:35`, `agent/tools_step.rs`) не сериализуются через serde, время пишется строками `to_rfc3339()` **[п — проверить, убрав фичу]** | 0 |
| `color-eyre` | 0.6.5 | 0.6.5 | — | Актуальна, но кандидат на замену `eyre`, см. §2.2 | 3 файла |
| `thiserror` | 2.0.18 | 2.0.21 | patch | Ядро `AppError` (`error.rs`) | Нет |
| `clap` | 4.6.1 | 4.6.7 | patch | `main.rs`, `harness/mod.rs` | Нет |
| `open` | 5.3.6 | 5.4.5 | minor | 8 вызовов в 3 файлах; **новых крейтов не добавляет** (все 5 транзитивных уже в дереве) **[ф]** | Нет |
| `serde`/`serde_json`/`tracing`/`tracing-subscriber` | 1.0.228 / 1.0.150 / 0.1.44 / 0.3.23 | 1.0.229 / 1.0.154 / 0.1.44 / 0.3.23 | patch/none | — | Нет |
| `futures` | 0.3.32 | 0.3.34 | patch | Заменяется на `futures-util`, см. §2.1 | 12 файлов, механически |
| `unicode-width` | 0.2.0 | 0.2.3 | patch | 2 файла | Нет |
| `rust-stemmers` | 1.2.0 | 1.2.0 | — | — | Нет |

### 1.6 fastembed / ort

| Версия | ort | MSRV | Что ломается |
|---|---|---|---|
| 5.17.2 (сейчас) | `=2.0.0-rc.12` | 1.88 | — |
| 5.17.4 | rc.12 | 1.88 | ничего (патчи) |
| 6.1.0 | `=2.0.0-rc.13` | 1.88 (через ort rc.13) | `fastembed::Error`: был `anyhow::Error`, стал enum **[ф: release notes 6.0.0]**. У нас только `map_err(|e| format!(...))` → Display работает |
| 7.1.0 | `=2.0.0-rc.13` | 1.88 | `InitOptions` → `TextInitOptions` (мы уже на нём) **[ф]** |
| 7.2.0+ | `=2.0.0-rc.15` | **1.92** | **MSRV-конфликт** с 1.91 |
| 7.4.0+ | rc.15, ONNX стал **опциональным** | 1.92 | при `default-features = false` обязателен явный `"onnx"` в фичах **[ф]** |

Итог: до 7.1.0 путь открыт, 7.2+ упирается в MSRV (сначала решение по 1.92).
`tokenizers` 0.22 → 0.23 едет транзитивно, наш код его не трогает.

## 2. От чего избавиться / что заменить

Все числа крейтов **замерены**: временный крейт в `E:\tmp\prs-depcount\<вариант>`,
`cargo metadata` и подсчёт уникальных пакетов. «Уникальных» — сколько из них
исчезнет именно из нашего дерева (пересечение с текущими 402).

### 2.1 Что реально даёт экономию

| Действие | Крейтов сейчас → станет | Уникальных из дерева | Что теряем | Замерено? |
|---|---|---|---|---|
| `wasmtime` без дефолтных фич (`std`,`cranelift`,`runtime`) | 183 → 110 | **−73** | component-model, async, cache, gc, coredump, profiling, backtrace, demangle, addr2line, wat, pooling-allocator, parallel-compilation. Ничего из этого мы не используем | да |
| `wasmtime` → 36.0.20 с теми же фичами | 183 → **96** | ≈−87 | то же + уходят 36 advisories | да |
| `syntect`: `default-fancy` → `["parsing","default-syntaxes","default-themes","regex-fancy"]` | 49 → 32 | ≈−17 | `html` (не рендерим HTML), `plist-load`, `yaml-load`. Заодно уходят `plist`→`quick-xml` (2 advisories), `yaml-rust` (unmaintained), `time`/`time-core`/`time-macros`/`deranged`/`num-conv`/`powerfmt`/`zmij`, `linked-hash-map` | да |
| `tokio`: `full` → `["rt-multi-thread","macros","fs","net","process","signal","sync","time","io-util","io-std"]` | 24 → 16 | **−8** | `parking_lot` + `lock_api`/`scopeguard`/`smallvec`/`redox_syscall`/`bitflags`/`cfg-if`. Все используемые `tokio::*` (проверено grep: `sync`, `time`, `net`, `process`, `signal`, `fs`, `io::AsyncRead`, `task`) входят | да |
| `futures` → `futures-util` | 16 → 5 | **−6** | `futures`, `futures-executor`, `futures-io`, `futures-sink`, `futures-channel`, `futures-macro`. Все 18 мест (`join_all`, `StreamExt`, `FutureExt`, `FuturesUnordered`, `stream::iter/poll_fn`) есть в `futures-util` **[ф: список вызовов]** | да |
| убрать `textwrap` | 34 → 0 | **−5** | только перенос строк в одном месте (`tui/render/modals.rs:593`, текст вопроса). Уникальные: `icu_segmenter`, `icu_segmenter_data`, `icu_locale_fallback`, `icu_locale_fallback_data`, `utf_types` — ICU-данные, это ещё и вес в бинарнике | да |
| `color-eyre` → `eyre` | 27 → 4 | ≈−5..−11 | цветные отчёты паник и `color-spantrace` (у нас stderr всё равно уводится в `errors.log`, `logging.rs`). Уходят `color-eyre`, `color-spantrace`, `owo-colors`, `tracing-error`, и — если `wasmtime` перестанет тянуть `backtrace` — вся цепочка `backtrace`/`addr2line`/`gimli`/`object`/`miniz_oxide` | да (число крейтов), размер — [о] |

Суммарно по верхним пяти строкам: **402 → ≈290** (−28%) **[о, по замеренным дельтам]**.

### 2.2 Что НЕ стоит трогать

- **`clap`**: 21 → 10 крейтов при `default-features = false` без `color`/`suggestions`
  **[ф]**, но теряются подсказки «did you mean» и цветной `--help`. Для CLI, куда
  ходят руками, 11 крейтов не стоят этого.
- **`open`**: 5 крейтов, но **0 уникальных** — замена на `std::process::Command`
  даёт ноль экономии и требует трёх platform-specific веток.
- **`reqwest` фичи** (`cookies`/`multipart`/`form`): используются все три (§1.2).
- **`regex`**: 18 файлов парсера тул-коллов, замена на ручной разбор — регресс.
- **`chrono`**: `std`-замены нет (нужны календарные расчёты, локальная зона, парсинг
  `%Y-%m-%dT%H-%M-%S`). `time` вместо `chrono` — латеральная замена без выгоды.
- **`color-eyre` → `thiserror`**: не дубликаты. `thiserror` — типы ошибок приложения,
  `color-eyre` — только `install()` и `Result` на верхнем уровне. Заменять надо
  `color-eyre → eyre`, а `thiserror` оставить.
- **`async-trait`**: см. §1.5, dyn обязателен.
- **`portable-pty` / `syntect` / `rust-stemmers` / `fastembed` / `wasmtime`**:
  безальтернативны в своих задачах (последние три — сознательные решения).

### 2.3 Дубли версий в дереве (43 имени)

Действия, которые реально схлопывают копии:

| Дубль | Кто тянет | Что делать |
|---|---|---|
| `windows-sys` 0.52 / 0.59 / 0.60.2 / 0.61.2 | `ring`, `wasmtime` 27, мы, `hf-hub`+`anstream` **[ф]** | бампнуть наш до 0.61 → 3 копии; `wasmtime` 36/43 уносит 0.59 |
| `toml` 0.8.23 + 1.1.2 | 0.8 — только `wasmtime-cache` **[ф]** | уходит с урезанием фич wasmtime |
| `base64` 0.13.1 / 0.21.7 / 0.22.1 | `spm_precompiled` (tokenizers), `wasmtime-cache`, мы **[ф]** | 0.21 уходит с урезанием wasmtime; 0.13 остаётся (чужой) |
| `thiserror` 1.0.69 + 2.0.18 | `filedescriptor` (portable-pty) и `wasmtime-cranelift` | portable-pty 0.9 — последняя; wasmtime 36 может убрать свою копию |
| `hashbrown` 0.14/0.15/0.16/0.17 | 0.14 — `wasmparser` 0.219 (wasmtime 27) | уходит с бампом wasmtime |
| `syn` 1.0.109, `reqwest` 0.12.28, `windows-sys` 0.52 | только в lock, не в графе сборки **[ф]** | ничего |
| `rand` 0.8+0.9, `getrandom` 0.2/0.3/0.4, `nix` 0.28+0.29, `bitflags` 1.3+2.x | zbus/hf-hub/portable-pty | не наши, не трогать |

### 2.4 Мёртвые feature-флаги (проверено grep)

- `windows-sys`: **`Win32_Security` не используется нигде** **[ф]** — убрать.
  Остальные семь фич подтверждены вызовами.
- `chrono`/`serde`: сериализуемых `DateTime` не найдено **[п]** — проверить снятием фичи.
- `crossterm`/`event-stream`: используется (3 ссылки) — оставить.
- `ratatui`/`unstable-rendered-line-info`: используется (`Paragraph::line_count`
  в `tui/widgets/chat.rs:50,140`) — оставить, это осознанное решение из `Cargo.toml`.
- `tracing-subscriber`/`env-filter`: используется (`RUST_LOG`) — оставить.

## 3. Новые API, которые улучшают существующие механики

### 3.1 Пул соединений и keep-alive — `reqwest::ClientBuilder`

Механика: клиенты создаются в четырёх местах — `provider/deepseek/mod.rs:73`,
`provider/compat_client.rs:183`, `mcp/http.rs:24`, `server/http.rs:75`; форки
провайдеров делят один клиент (`fork_session` клонирует `client`) **[ф]**, так что
пул уже общий и утечки нет. Но дефолты не настроены: `tcp_keepalive` в reqwest
**выключен по умолчанию** **[ф: changelog]**, `pool_idle_timeout` — 90 с
**[ф: `reqwest-0.13.4/src/async_impl/client.rs:301`]**, `pool_max_idle_per_host`
не ограничен.

Применить: `.tcp_keepalive(Duration::from_secs(30))` + `.pool_max_idle_per_host(4)`
+ `.pool_idle_timeout(Duration::from_secs(90))` в билдерах DeepSeek и compat.
Выгода **[о]**: для прокси-шлюза с несколькими аккаунтами — один TLS-handshake на
аккаунт вместо одного на запрос, ограниченный набор висящих сокетов; для одиночного
чата — ничего не ломает.

### 3.2 Классификация сетевых ошибок — `reqwest::Error::is_dns()` (0.13.5)

Механика: сейчас ретраи/бэкофф в провайдере не различают «нет DNS» от «500 от
сервера». `is_dns()` позволяет не тратить 3 попытки с бэкоффом на заведомо мёртвую
сеть и отдать внятное сообщение. Применить в маппинге ошибок провайдера
(`provider/mod.rs`, `error.rs` рядом с `AppError::Http`).

### 3.3 Лимит заголовков на `/serve` — `hyper` 1.12 `max_header_size`

Механика: `server/http.rs:165` создаёт `http1::Builder::new().serve_connection(...)`
без ограничения на размер заголовков. `/serve` может быть выставлен в сеть
(`[server] host`), значит недоверенный клиент может прислать гигантские заголовки.
Применить: `.max_header_size(16 * 1024)` (и/или `max_headers`). Выгода — устойчивость
к мусорным запросам, 1 строка.

### 3.4 Сглаживание event loop — `tokio` 1.54 `Builder::max_io_events_per_busy_tick`

Механика: один `select!` на 120 мс + рендер. При шторме сокетов (много параллельных
чатов/MCP) событийный бюджет IO за тик никем не ограничен. Новый билдер-метод
позволяет ограничить число IO-событий на «занятый» тик — рендер получает управление
предсказуемее. Применить в `main.rs` (построение runtime). Выгода **[о]**: меньше
рывков UI под нагрузкой; требует замера `render_chat_perf_probe`.

### 3.5 Прочее — без выгоды (чтобы не искали заново)

- `fastembed` 7.6 «automatic device selection» — бесполезно: у нас CPU-сборка
  ONNX Runtime, GPU-фичи (`cuda`/`directml`) не включены.
- `base64` 0.23 SIMD (`simd-unsafe`, дефолт) — ускоряет только `encode` PoW-ответа
  (десятки байт) и PKCE-хэш; выигрыш на уровне шума.
- `ratatui` 0.30 `Termina`/теневые эффекты — про их собственный фреймворк, нам не нужно.
- `pulldown-cmark` 0.13.6, `toml` 1.2, `uuid` 1.29 — новых API, применимых у нас, нет.
- `async-trait` → нативные AFIT: не вариант, пока трейты используются как `dyn`.

## 4. Что понадобится для будущего

**Прокси-шлюз с несколькими аккаунтами.** Ничего нового тянуть не нужно **[ф]**:

- Изоляция аккаунтов — `reqwest` с фичей `cookies` (уже включена) + отдельный
  `Client`/`Jar` на аккаунт; сейчас cookie-стор используется в `mcp/http.rs:25`.
- Состояние сессии на аккаунт — существующий `SessionState` + `fork_session()`
  (`provider/deepseek/mod.rs`), лимитер `Arc` уже шарится между форками.
- Таблица аккаунтов — `tokio::sync::RwLock` (уже в проекте), без `arc-swap`.
- Маршрутизация по аккаунту — `server/catalog.rs` + `resolve_backend` уже выбирают
  провайдера по id модели; добавляется только «аккаунт» в ключ.

**Опрос лимитов.** `tokio::time::interval` + существующий `rate_limit_*`
(`provider/deepseek/mod.rs`), `AppEvent::Tick` для UI. Новых крейтов нет.

**TUI-обновления.** `crossterm` `event-stream` + `ratatui` 0.30 покрывают всё;
единственное, что впереди — breaking-релиз crossterm (§1.1), это работа, а не зависимость.

**Чего не хватает, но лучше не тянуть.** `tower`/`axum` для шлюза — не нужны,
`hyper` 1 + `hyper-util` + `http-body-util` уже есть (осознанное решение в `Cargo.toml`).
`tracing-opentelemetry` / метрики — если понадобится, только по явному запросу.

## 5. Безопасность (OSV / RustSec на `Cargo.lock`)

`cargo audit` не установлен → **проверено не `cargo audit`, а OSV `querybatch`** по 593
registry-пакетам (RustSec- и GHSA-записи). Найдено 23 пакета с advisories **[ф]**.
Разбор по применимости:

| Пакет | Advisory | Применимо к нам? | Действие |
|---|---|---|---|
| `wasmtime` 27.0.0 | **36 записей** | Да: линия 27 EOL, фиксов не будет. Большинство — JIT-песочница и component-model; мы исполняем чужой (upstream) wasm с полным доверием, эскапей из песочницы для нас не критичен, но 36 записей — плохой фон | Апгрейд на 36.0.20 (0 advisories, MSRV 1.86) |
| `rustls` 0.23.41 | 4 RUSTSEC + 4 GHSA, в т.ч. -0285 (TLS 1.3 cross-level, C: LOW) и -0346 (Debug печатает секреты) | Да, транспорт | `cargo update -p rustls` до ≥0.23.46 |
| `rustls-webpki` 0.103.13, `webpki-roots`, `webpki-root-certs` | -0382, -0381, -0383 | Да | тем же `cargo update` |
| `pulldown-cmark` 0.13.4 | -0414, -0433 | Нет: нужен `ENABLE_WIKILINKS`, мы его не включаем **[ф]** | всё равно бамп до 0.13.6 |
| `tokio` 1.52.3 | -0386 (`AsyncFd`, unix) | Нет (`AsyncFd` не используется) | бамп до 1.54.1 |
| `lru` 0.18.0 | -0253 (UAF в `LruCache::pop` при панике) | Да, через `ratatui-core` 0.1.2 | `cargo update -p lru` до 0.18.2 |
| `event-listener` 5.4.1 | -0221 (`!Send` tags) | Да, транзитивно (zbus/reqwest) | `cargo update -p event-listener` |
| `zstd`/`zstd-sys` | 13 записей (-0391..-0410) | Только через `wasmtime-cache` | уходит с урезанием фич wasmtime |
| `quick-xml` 0.38.4 | -0194, -0195 | Только через `syntect`→`plist` | уходит с урезанием фич syntect |
| `anyhow` 1.0.102 | -2026-0190 | косвенно (wasmtime/hf-hub) | `cargo update` |
| `bincode` 1.3.3, `yaml-rust`, `fxhash`, `paste` | unmaintained | `syntect` (bincode — неустранимо, `yaml-rust` — уходит с фичей), `wasmtime` (`fxhash`) | см. §2.1 |
| `quinn-proto` 0.11.15 | 5 записей | Только в lock: HTTP/3-фича не включена, в графе сборки пакета нет **[ф]** | ничего |
| `zbus_names`/`zvariant` | -0370..-0372 | Linux-only ключница | обновится вместе с keyring 4.2.0 |

## 6. План обновления (маленькими независимыми шагами)

**Шаг 0 (подготовка, отдельно, никаких бампов).** Добавить `#[test]` в
`provider/pow.rs`: фиксированный `PowChallengeData` (challenge/salt/expire_at/
difficulty), прогон встроенного wasm, проверка `answer`. Сейчас у PoW **нет ни одного
теста** **[ф]** — без него любой бамп wasmtime проверяется только живым запросом.

**Волна A — только lock-файл, без правок кода** (можно параллельно с B):
`cargo update -p tokio -p pulldown-cmark -p rustls -p rustls-webpki -p webpki-roots
-p lru -p event-listener -p reqwest -p hyper -p hyper-util -p http-body-util
-p anyhow -p serde -p serde_json -p thiserror -p clap -p toml -p uuid -p regex`.
Гейт: `cargo test --bin pooprusteek`, `cargo clippy -D warnings`, `cargo fmt --check`,
повторный OSV-скан.

**Волна B — keyring 4.2.0 и снос обхода** (независимо от A и C):
1. `keyring = "4.2"`, удалить `keyring-core` и три target-зависимости стор-бэкендов.
2. Удалить `ensure_default_store()` и вызов в `mcp/oauth_store.rs::entry`, переписать
   шапку `Cargo.toml` и doc-комментарий модуля.
3. Гейт: компиляция + ручная проверка OAuth MCP-сервера на Windows (автотестов у
   `oauth_store` нет **[ф]**); на Linux — только компиляция (Secret Service в CI нет).

**Волна C — урезание фич, каждый пункт независим и параллелен:**
- C1 `wasmtime = "36"` + `default-features = false, features = ["std","cranelift","runtime"]`.
  Гейт: тест из шага 0 + один живой запрос DeepSeek (`scenario live`/`exec`).
  Отдельно проверить, что `assets/*.wasm` инстанцируется (тот же тест).
- C2 `syntect` без `default-fancy`. Гейт: 5 тестов в `tui/markdown.rs` + визуальная
  проверка подсветки (`/themes`).
- C3 `tokio` без `full` (список фич в §2.1). Гейт: компиляция — забытая фича падает
  на `unresolved import`, дальше полный прогон тестов.
- C4 `futures` → `futures-util` (12 файлов). Механически, компилятор найдёт все места.
- C5 удалить `textwrap`, заменить `modals.rs:593` на локальный перенос по словам
  (`Paragraph::wrap` недоступен для уже собранных `Line`, поэтому проще свой хелпер
  в `tui/render/util.rs`, как `truncate`).
- C6 `windows-sys` → 0.61 + убрать `Win32_Security`.
- C7 (опционально) `color-eyre` → `eyre`; выигрыш в основном в размере бинарника.

**Волна D — breaking-версии, по одной, после A–C:**
- D1 `base64` 0.23 (0 правок ожидается).
- D2 `sha2` 0.11 — гейт: `sha256_hex_matches_known_vector` в `update/manifest.rs` и
  PKCE-тест, если есть; иначе сверить `code_challenge` с эталоном RFC 7636.
- D3 `dirs` 7 (0 правок).
- D4 `fastembed` 5.17.4 → 6.1.0 → 7.1.0 (два отдельных шага). Гейт каждый раз:
  `cargo test --bin pooprusteek semantic::eval -- --ignored --nocapture` и запись MRR
  в журнал (базлайн: skills 0.927, MCP tools 0.836).
- D5 `wasmtime` 36 → 43/50 — **только после решения по MSRV** (43 требует 1.91, то
  есть уже сейчас влезает, но линия 43 сама EOL: 10 advisories; 50 требует 1.97 и
  поднимает MSRV, а с ним и `sandbox/Dockerfile`).

**Что вместе, что отдельно:**
- Вместе можно: A + B + C1…C7 + D1 + D3 (разные подсистемы, конфликтов нет).
- Отдельно обязательно: D2 (`sha2`, крипто-векторы), D4 (`fastembed`, MRR-гейт),
  D5 (`wasmtime`, MSRV + PoW-тест).
- Порядок внутри C произвольный; C1 и C2 дают самую большую экономию.

**Какие тесты что ловят:**

| Поломка | Гейт |
|---|---|
| wasmtime API/фичи | новый PoW-тест (шаг 0) + `exec` с живым DeepSeek |
| подсветка/рендер | 5 тестов `tui/markdown.rs`, `render_chat_perf_probe` |
| tokio-фичи | компиляция + `cargo test` (сокет-тесты сервера, PTY-тесты) |
| futures-util | компиляция |
| keyring | ручной OAuth-флоу (автотестов нет) |
| fastembed | `semantic::eval` MRR + harness `scenario mock` |
| sha2 | тест-вектор SHA-256, PKCE |
| hyper/serve | сокет-тесты сервера (`server/*`) |
| PoW-блоб | тест из шага 0 (детерминированный, без сети) |

**Не проверено / открытые вопросы:**
1. Ни один бамп не собран: в этой оболочке нет Windows SDK (`kernel32.lib`).
2. `chrono`/`serde` — нужность фичи не доказана, только не найдена.
3. Применимость advisories `wasmtime` 43 к нашей конфигурации (без WASI/component
   model) — по описаниям да, но точный разбор каждого из 10 не делался.
4. `windows-sys` 0.61 — символы на месте, но типы (`HANDLE`, `BOOL`) не сверялись
   построчно; ловится компиляцией.
