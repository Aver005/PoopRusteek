# Локальное «чтение картинок» — исследование и план

> Статус: **исследование, код не менялся**. Дата: 2026-08-26.
> Задача: полностью локальный опциональный инструмент, который описывает
> содержимое изображения (подробное описание + текст с картинки) и работает
> как пре-обработка *до* отправки модели.
> Читать вместе с: `.memories/ARCHITECTURE.md`, `.memories/reference/TOOLS.md`,
> `CLAUDE.md` (инварианты 3, 4, 5, 9, 10, 12).

---

## 1. Что происходит с изображениями сегодня

Три места, где картинка теряется. Все три — точки будущего подключения.

### 1.1 `/attach` молча роняет файл

`src/app/keys/chat.rs` (`submit_input`, блок инлайна вложений):

```rust
let content = std::fs::read_to_string(&f.path).ok()?;   // PNG → Err(InvalidData)
```

`filter_map` возвращает `None`, и файл исчезает целиком: `attached_names.push`
стоит *после* чтения, поэтому имя не попадает даже в строку `📎 attached: …`.
Пользователь видит «1 file attached» в статус-баре, модель не получает ничего
и не знает, что вложение было.

Флаг `AttachedFile.is_image` (`src/provider/mod.rs`) выставляется в
`src/commands/defs/attach.rs` и в `src/app/keys/autocomplete.rs`, но
используется **только для иконки 🖼** в `src/tui/render/status.rs`. Точка
расширения уже размечена и пуста.

### 1.2 MCP-скриншоты схлопываются в заглушку — самый ценный кейс

`src/mcp/client.rs`, `flatten_content`:

```rust
"image" => { … result.push_str(&format!("[Image: {mime}]")); }
```

Причём структура `ContentItem` даже не десериализует поле `data`, так что
base64 отбрасывается на уровне serde. **Каждый скриншот от playwright /
chrome-devtools MCP сейчас доходит до модели как пустая строка
`[Image: image/png]`.** Это, вероятно, более ценный потребитель описателя,
чем ручной `/attach`.

### 1.3 Режим `/serve` слеп по построению

`src/provider/openai_compat.rs` осознанно роняет non-text content parts при
конвертации. Входящие мультимодальные запросы к нашему gateway теряют
картинки. Не блокер, но при появлении описателя чинится тем же вызовом.

---

## 2. Главный рычаг: ONNX-рантайм уже слинкован

Транзитивно через `fastembed` в `Cargo.lock` уже есть:

| Крейт | Версия | Фичи |
|---|---|---|
| `ort` (ONNX Runtime) | `2.0.0-rc.12` | `download-binaries`, `tls-rustls`, `ndarray`, `std`, `api-24` |
| `tokenizers` | 0.22.2 | — |
| `ndarray` | 0.17.2 | — |
| `hf-hub` | 0.5.0 | rustls |
| `image` | **отсутствует** | `fastembed/image-models` выключен намеренно |

Вывод: локальная модель зрения **не тянет ни новый нативный тулчейн, ни
второй ML-рантайм**. Нужны только `image` (декод/ресайз) и прямая
зависимость `ort` — обязательно `=2.0.0-rc.12` с теми же фичами, иначе cargo
разведёт два несовместимых `ort-sys`.

Плюс готовый шаблон подсистемы — `src/semantic/`:

- фоновая инициализация `spawn_init` → `tokio::task::spawn_blocking`;
- кэш моделей в `Config::data_dir()/models`, скачивание один раз;
- `AppEvent::SemanticStatus` для прогресса в UI;
- деградация вместо блокировки (инвариант 12);
- `intra_threads = (cores/2).clamp(1,4)` в `semantic/embedder.rs` — чтобы ONNX
  не душил event loop.

`VisionService` пишется как копия этой формы. Ни одного нового
архитектурного приёма изобретать не нужно.

---

## 3. Варианты и сравнение

| | Что даёт | Свой код | Модель | CPU-задержка | Русский |
|---|---|---|---|---|---|
| **A. OCR: `ocrs` 0.12.2 + `rten` 0.25** | текст + координаты строк | ~150 строк | ~20–30 МБ `.rten` | 0.1–0.5 с | ❌ латиница |
| **B. VLM в процессе: Florence-2-base-ft ONNX через уже слинкованный `ort`** | `<MORE_DETAILED_CAPTION>`, `<OD>`, `<OCR_WITH_REGION>` — описание + OCR + объекты одной моделью | ~600–800 строк (generation loop с KV-cache) | int8 ≈ **275 МБ**, q4f16 ≈ 220 МБ | 1–3 с | ❌ ответ на английском |
| **C. `candle-transformers` 0.11 (`quantized_moondream`, `blip`, `paddleocr_vl`)** | готовые реализации, generation loop писать не надо | ~200 строк | moondream q4 ≈ 1.8 ГБ | 20–60 с на 1.8B | частично |
| **D. Сайдкар: Ollama / llama.cpp через существующий `/providers`** | лучшее качество описания | ~100 строк поверх `CompatClient` | внешняя | 3–15 с | ✅ |

Размеры Florence-2-base-ft (`onnx-community/Florence-2-base-ft`, лицензия MIT)
для int8-набора: `vision_encoder` 94 МБ + `encoder` 44 МБ +
`decoder_model_merged` 98 МБ + `embed_tokens` 39 МБ ≈ 275 МБ.

**Вариант C отбрасывается**: `candle-transformers` компилирует ~100 моделей
ради одной (заметный удар по времени сборки), а 1.8B на CPU — это не
пре-обработка, это отдельный ход агента.

---

## 4. Рекомендация

**B как встроенный дефолт, A как дешёвый обязательный слой, D как
опциональный бэкенд за конфигом.**

Обоснование:

- **B** — единственный вариант, где «подробное описание» стоит 1–3 секунды и
  275 МБ, не требует от пользователя вообще ничего и переиспользует уже
  присутствующий `ort`.
- **A** нужен рядом с B, потому что Florence галлюцинирует символы в мелком
  шрифте — а для скриншота кода/стектрейса важен точный текст, а не пересказ.
  OCR даёт его за 300 мс.
- **D** закрывает русский язык и сценарий «хочу максимум качества», не ломая
  ничего: `CompatClient` уже умеет говорить с локальным OpenAI-совместимым
  эндпоинтом.

Самый быстрый путь к работающему «понимает, что на картинке» — это D (часы
работы). Единственный, который даёт то же самое из коробки, — B.

---

## 5. Архитектура интеграции

### 5.1 Новый модуль `src/vision/` — ровесник `src/semantic/`, ниже `app/`

```
src/vision/
  mod.rs         VisionService: Arc-handle, AtomicBool enabled/init_running,
                 Mutex<Inner { describer: Option<Box<dyn Describer>> }>
  describer.rs   trait Describer {
                     fn describe(&mut self, img: &DecodedImage, mode: Mode)
                         -> Result<Description, String>;
                 }
  florence.rs    ort-сессии: vision_encoder → encoder → decoder_merged + KV-cache
  ocr.rs         бэкенд на ocrs
  remote.rs      бэкенд D поверх CompatClient
  preprocess.rs  декод, ресайз под max_pixels, метаданные (размер, формат, alpha)
```

`Description` — структурированный тип (`caption`, `ocr_lines: Vec<(String,
Rect)>`, `objects`, `meta`), а рендер в текст для модели — отдельная функция,
по образцу `semantic::render_hint`. Один результат обслуживает все три точки
подключения.

### 5.2 Точки подключения, в порядке ценности

1. **Tool `image_read`** — собственно запрошенный внутренний инструмент.
   `Tool` trait + одна строка регистрации (инвариант 7), по образцу
   `register_semantic_tools` / `src/tools/tool_search.rs`. Аргументы: `path`,
   `mode: caption|ocr|full`, опционально `question`. Внутри — `spawn_blocking`
   (инвариант 9), ровно как `ToolSearchTool::execute`.
2. **MCP-скриншоты** (`src/mcp/client.rs`): добавить `data: Option<String>` в
   `ContentItem`, в ветке `"image"` сохранять байты во временный файл и
   возвращать путь. **Описывать сразу не надо**: `flatten_content` синхронная,
   а тратить секунды на каждый скриншот, который модель могла бы и не
   смотреть, расточительно. Пусть модель сама зовёт `image_read` по пути —
   дешевле и честнее.
3. **`/attach` картинки** (`src/app/keys/chat.rs`): развилка по `f.is_image`
   вместо текущего молчаливого дропа.

### 5.3 Конфиг и команда

`[vision]` в `src/config/mod.rs` по образцу `SemanticConfig`:

```toml
[vision]
enabled = false          # 275 МБ не качаем тем, кто не просил
backend = "florence"     # florence | ocr | remote | off
mode = "full"            # caption | ocr | full
max_pixels = 1600000
remote_endpoint = ""     # для backend = "remote"
```

Команда `/vision on|off|status` — один файл в `src/commands/defs/` +
регистрация в `commands/mod.rs` (инвариант 5), зеркало `defs/rag.rs`.

### 5.4 Затрагиваемые инварианты

| # | Требование |
|---|---|
| 3 | кэш/индексы — только через `util::atomic_write` |
| 4 | обрезка описаний — `util::truncate_at_char_boundary`, не байтовые срезы |
| 5 | `/vision` — один файл в `defs/` + регистрация, `name()` без слеша |
| 6 | `vision/` не зовёт `app/` напрямую — только `AppEvent` |
| 9 | весь ONNX/декод — только `spawn_blocking`; мьютекс не держать через `.await` |
| 10 | `ort` пишет в stderr; на Windows он уже перенаправлен в `errors.log` — новая сессия покрыта автоматически |
| 12 | сервис деградирует: выключен / не готов / упал → отдаём только метаданные, никогда не блокируем ход |

---

## 6. План по этапам

| Этап | Объём | Результат |
|---|---|---|
| **0** | ~0.5 дня | Починить молчаливый дроп в `chat.rs`: `[Image: name.png, 1920×1080, PNG, 340 KB — содержимое не прочитано]`. Уже полезно — модель перестаёт делать вид, что вложения не было |
| **1** | ~1 день | `+image`, `+ocrs`; `VisionService` + OCR-бэкенд + tool `image_read` (mode=`ocr`). Скриншоты кода, ошибок, терминала — ~80 % реальных кейсов кодового агента — закрыты за 300 мс |
| **2** | ~3–5 дней | `+ort = "=2.0.0-rc.12"`; Florence-2-base-ft int8 через `hf-hub` в `data_dir/models`; generation loop с KV-cache; `mode = caption \| full` |
| **3** | ~1 день | Бэкенд `remote` поверх `CompatClient` + `[vision] remote_endpoint`; закрывает русский и «максимум качества» |
| **4** | ~0.5 дня | MCP-скриншоты: `data` в `ContentItem` → временный файл → путь в тексте результата |

Этапы независимы после 1: `Describer` — это точка подмены, каждый бэкенд
добавляется отдельно и включается конфигом.

---

## 7. Грабли, зафиксированные заранее

- **Версия `ort` жёстко `=2.0.0-rc.12`**, `default-features = false`, фичи
  `["ndarray", "std", "api-24"]` — ровно как у fastembed. Разъезд версий даёт
  два `ort-sys` и падение на линковке.
- **Бюджет системного промпта**: в `src/tools/registry.rs` есть тест
  `builtin_tool_definitions_stay_within_byte_budget` (< 10 000 байт). Описание
  `image_read` съест ~400–600 байт. Либо осознанно двигаем бюджет, либо
  регистрируем инструмент только при `[vision] enabled` — отдельным методом,
  как `register_semantic_tools`.
- **Сборка**: в `CLAUDE.md` уже задокументировано, что параллельный rustc +
  линковка `ort` выжирает pagefile на Windows (`os error 1455`). Новые
  ONNX-сессии это не усугубят, но `image` + `ocrs`/`rten` добавят минуты к
  чистой сборке — замерить и записать в журнал.
- **Русский текст на скриншотах**: `ocrs` — латиница, Florence отвечает
  по-английски. Если кейс важен — `paddleocr_vl` или PaddleOCR-ONNX через тот
  же `ort` отдельным OCR-бэкендом. Постпроцессинг DB-детектора без OpenCV
  (threshold → контуры → unclip) — заметная работа, закладывать отдельным
  этапом.
- **Тестирование поведения, а не кода**: описание картинки меняет промпт — это
  ровно то, что ловит `src/harness/`. Сценарий в `sandbox/scenarios/` с
  фикстурой-скриншотом и `mock-provider` даёт детерминированную регрессию;
  юнит-тесты здесь почти бесполезны.
- **Лицензии**: Florence-2 — MIT; `ocrs` / `rten` — MIT OR Apache-2.0.
  Конфликтов с MIT репозитория нет.
- **Приватность**: описатель полностью локальный, но результат уходит в промпт
  провайдеру. На скриншоте могут быть токены и пароли — стоит предусмотреть,
  что `image_read` проходит обычный tool-approval и не попадает в
  `/whitelist` по умолчанию.

---

## 8. Проверено фактами (на 2026-08-26)

- `ort 2.0.0-rc.12`, `tokenizers 0.22.2`, `ndarray 0.17.2`, `hf-hub 0.5.0` —
  присутствуют в `Cargo.lock`; `image` — отсутствует.
- `fastembed` фича `image-models` = `dep:image`; `ImageEmbeddingModel` даёт
  только эмбеддинги (CLIP / ResNet50 / Unicom / nomic-vision), **описаний не
  даёт** — для задачи не подходит.
- `ocrs` 0.12.2, `rten` 0.25.0 — MIT OR Apache-2.0.
- `candle-transformers` 0.11.0 содержит `moondream`, `quantized_moondream`,
  `blip`, `quantized_blip`, `paddleocr_vl`, `qwen3_vl`, `paligemma`.
- `onnx-community/Florence-2-base-ft` — ONNX-экспорт существует, лицензия MIT,
  размеры файлов сверены с HF API.
