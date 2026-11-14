# Аудит использования TUI-движка (ratatui 0.30.2 + crossterm 0.29)

Только чтение кода. Приложение не запускалось, тесты не прогонялись, ничего не изменено.

Версии сверены по `Cargo.lock`: `ratatui 0.30.2`, `ratatui-core 0.1.2`, `ratatui-widgets 0.3.2`,
`crossterm 0.29.0` (в дереве одна версия crossterm — классической поломки «два crossterm в
зависимостях» нет), `unicode-width 0.2.0` (одна версия, та же, что у ratatui — расхождения
мер ширины между приложением и рендерером по этой причине нет).

Документация сверялась по docs.rs / ratatui.rs (highlights v0.30, 0.30.2), а не по памяти.

## Легенда

- **critical** — залипание/поломка интерфейса в обычном сценарии;
- **major** — нарушение инварианта проекта или заметный визуальный/скоростной дефект;
- **minor** — мелочь, чинится попутно.

Пометка **проверено** — вывод следует из кода, который прочитан целиком (и, где нужно, из
документации API). **предположение** — следствие рассуждения, которое не подтверждено ни
кодом, ни документацией.

---

## 1. Что из ratatui делается руками, хотя есть готовое

| Готовое API | Что сейчас | Где | Вердикт |
|---|---|---|---|
| `ratatui::init()` / `ratatui::run()` + паника-хук | свой `init`/`restore` без хука | `tui/mod.rs` | **надо взять** (находка 2) |
| `Rect::layout(&Layout)` / `Layout::areas()` (0.30) | `Layout::default().constraints(vec![..]).split(area)` + индексы `chunks[i]`, ручные счётчики `ci`/`attach_idx` | `render/mod.rs`, `render/landing.rs` | **надо взять** (находка 13) |
| `Frame::set_cursor_position` | используется в `render_input`, но потом перебивается `MoveTo`/`show_cursor` | `widgets/input.rs` + `render/mod.rs` | **выбрать одно** (находка 5) |
| `Scrollbar` + `ScrollbarState` | скролл чата без индикатора, позиция считается руками | `widgets/chat.rs` | **стоит взять** (находка 11) |
| `Rect::centered` / `centered_horizontally` / `Rect::outer` (0.30) | `centered_h`, `center_popup` | `render/util.rs`, `render/popup.rs` | по желанию, дублируют друг друга |
| `Span::width()` / `Line::width()` / `width_cjk()` (0.30) | `unicode-width` вручную (это нормально, но местами через `ch.to_string()`) | `render/util.rs`, `widgets/input.rs` | частично (находка 10) |
| `Stylize` (`.fg().bold()`) | `Style::default().fg(..).bg(..)` везде | все виджеты | не трогать: стиль проекта задан `Style` явно, выгода косметическая |
| `Block::title(Into<Line>)` | уже так | `render/popup.rs` | ок |
| `Clear` перед попапом | уже так | `render/popup.rs:draw_popup`, `widgets/panel.rs` | ок, не трогать |
| `Paragraph::line_count` (unstable) | используется осознанно | `widgets/chat.rs` | ок, но см. находку 20 |
| `StatefulWidget` / `List` / `Table` | списки рисуются вручную окном + маркеры `↑ more` | `render/mcp.rs`, `render/search.rs` | рабочий вариант, менять не обязательно |
| `ratatui-widgets` как отдельный крейт | — | — | не нужно: приложение, а не библиотека виджетов |
| Синхронный вывод (CSI `?2026`) | не используется | `render/mod.rs` | стоит добавить (находка 12) |

Что появилось в 0.30 после «старых» версий и здесь не задействовано: `ratatui::run`,
эргономичные `Rect::layout`/`try_layout`/`layout_vec`, `Layout::areas`/`try_areas`,
`Rect::centered*`, `Rect::outer`, `Text/Line/Span::width()/width_cjk()`,
`Color::from((r,g,b))`, слияние рамок `Block`, `ScrollbarState::get_position`,
разделение на `ratatui-core` / `ratatui-widgets` / `ratatui-crossterm` (для приложения
неактуально), `no_std`, бэкенд Termina. Код уже написан под 0.30, поэтому «после версии»
здесь означает именно «доступно, но не взято».

---

## 2. Находки

### 1. Блокирующее чтение каталога на каждом нажатии клавиши — critical, проверено

`src/app/keys/autocomplete.rs` → `App::refresh_autocomplete`.

`refresh_autocomplete()` вызывается в конце **каждого** нажатия в чате
(`keys/chat.rs:handle_chat_key`, последняя строка перед `Ok(false)`). Если в буфере есть `@`
с непустым «путём» после него, функция делает `std::fs::read_dir(...)` синхронно на потоке
цикла событий и для каждой подходящей записи ещё `entry.path()`, `is_dir()`,
`metadata()`.

Сценарий: пользователь пишет обычный текст с собакой — «напиши на foo@bar.com» — и начиная
с `@b` каждый следующий символ делает сканирование каталога + N `stat`. В каталоге с
тысячами файлов (или на сетевом/облачном пути, у OneDrive-плейсхолдеров) это десятки-сотни
миллисекунд на клавишу: экран перестаёт отзываться, при этом ничего не рисуется, потому что
`handle_event` не вернулся.

Это прямое нарушение инварианта 1 («никогда не блокировать и не `.await` file I/O внутри
`handle_event`»).

Предложение: считать каталог вне цикла и присылать результат событием.
`tokio::task::spawn_blocking(move || std::fs::read_dir(dir)...)` → собрать
`Vec<CommandSuggestion>` → `event_tx.send(AppEvent::AutocompleteReady { generation, items })`;
в `AppEvent` добавить монотонный `generation`, чтобы ответ на устаревший запрос
отбрасывался. Плюс дебаунс: не чаще одного сканирования на 150–200 мс (тот же приём, что
`tick_interval`), а `metadata()` не вызывать вовсе для файлов — размер показывать лениво или
через `DirEntry::metadata` только для показанных 8 элементов.

### 2. Нет паника-хука: паника оставляет терминал сломанным — major, проверено

`src/tui/mod.rs` → `init` / `restore`; вызовы в `app/mod.rs::run`.

`init()` руками включает raw mode, alt-screen, mouse capture, bracketed paste. Хук паники не
ставится нигде (`grep set_hook` по `src/` — пусто; `catch_unwind` есть только в
`app/runtime.rs` для ходов агента, не для главного цикла).

Сценарий: любая паника на главном потоке (например `expect("entry inserted just above")` в
`widgets/chat.rs`, индексация в новом виджете, деление на ноль в арифметике прокрутки)
печатает отчёт в stderr, оставляя пользователя в alt-screen с выключенным эхом и без курсора.
Помогает только `reset`. На Windows conhost это ещё и мусор поверх экрана.

Документация: `ratatui::init()` — «enables raw mode and alternate screen buffer, and installs
a panic hook to restore the terminal before panicking»; `ratatui::run()` появился в 0.30 и
делает init+restore вокруг замыкания.

Предложение: `let mut terminal = ratatui::init();` (это ровно `Terminal<CrosstermBackend<Stdout>>`,
то есть текущий `TuiTerminal` — подстановка без изменения типов; `init()` паникует при сбое,
если нужен текущий `Result` — `ratatui::try_init()`), затем добрать то, чего `init()` не
делает:
`execute!(terminal.backend_mut(), DisableBlinking, EnableMouseCapture, EnableBracketedPaste)`.
На выходе — `ratatui::restore()` плюс снятие своих режимов. Если хочется сохранить свой
`init`, минимально добавить
`std::panic::set_hook(Box::new(|info| { let _ = crate::tui::emergency_restore(); ... }))`
до входа в raw mode и вызывать старый хук после восстановления.

### 3. Чтение с диска прямо в кадре — major, проверено

а) `src/tui/widgets/panel.rs` → `cached_session_tag` → `session::load_local(session_id, config)`
раз в 5 секунд на каждый session id (TTL `SESSION_TAG_TTL`). `load_local` читает **и
разбирает JSON всего файла сессии**.

б) `src/tui/render/landing.rs` → `landing_sessions_cached` → `session::list_sessions()`
раз в 3 секунды: обход каталога сессий и разбор каждого файла.

Оба вызова находятся внутри замыкания `terminal.draw(...)`, то есть на главном потоке
цикла. Сценарий: 40 сессий по несколько сотен КБ на сетевом/медленном диске — раз в 3 секунды
кадр стоит десятки миллисекунд; при первом показе лендинга — заметная пауза. Нарушение духа
инварианта «render читает состояние и ничего не делает», и формально — «render ничего не
мутирует» (мутируется thread-local кэш, а не `AppState`, но I/O в кадре от этого не исчезает).

Предложение: убрать и то и другое из рендера. Тег сессии обновлять по событию, которое уже
приходит после сохранения хода (`reduce::turn_tail` → `auto_save_session`), и класть его в
`Conversation`/`AppState`; список сессий держать в `AppState` и обновлять при `/sessions`,
`/load`, старте и возврате на лендинг. В рендере остаётся чтение готового поля.
Промежуточный вариант — `spawn_blocking` + событие, а в рендере только `try_read`.

### 4. Кэш отрисованных сообщений не знает про тему — major, проверено

`src/tui/widgets/chat.rs` → `CacheKey` / `with_assistant_cached`.

Ключ: `(conversation_id, message_index, content_len, wrap_width)`. В `CachedMsg::lines`
лежат `Line<'static>` со **вшитыми цветами темы** (`markdown::render_markdown(content, theme)`
и `Span::styled` с `theme.*`). Тема в ключе отсутствует.

Сценарий: `/themes` → Enter (тема сохраняется в конфиг) → Esc обратно в чат. Пользовательские,
tool- и system-сообщения перекрашиваются сразу (`build_segment_lines` зовётся каждый кадр), а
markdown-ответы ассистента остаются в старой палитре до тех пор, пока не изменится длина
контента или кэш не переполнится (`MAX_CACHE_ENTRIES`). На светлой теме это выглядит как
тёмные пятна на бумажном фоне. Сам экран `/themes` этим не страдает — он рисуется с нуля;
портится именно возврат в транскрипт.

Предложение: добавить в `CacheKey` отпечаток темы. Дешевле всего — `theme_hash: u64`,
считаемый один раз на кадр (по `bg`, `fg`, `accent`, `text_dim`, `tool_bg`, `selection` —
этого достаточно, чтобы отличить любые две темы из `PRESETS` и кастомные поверх них) и
хранимый рядом с `theme` в `render()`; либо сравнивать тему с `thread_local LAST_THEME` в
`widgets/chat.rs` и при отличии очищать все три кэша разом (для этого понадобится
`pub(crate) fn invalidate_render_caches()`, сам `MSG_CACHE` приватный). Очистка случается
один раз на смену темы и дешевле, чем лишний компонент в ключе. Заодно убирается повторный
`Theme::resolve(&config.ui)` в каждом кадре (находка 16).

### 5. Курсор ставится двумя несовместимыми способами — minor, проверено

`src/tui/widgets/input.rs::render_input` (правильно: `frame.set_cursor_position((x, y))`) и
`src/tui/render/mod.rs::render` (после `terminal.draw` — `crossterm::execute!(MoveTo(cx, cy))`,
затем `terminal.hide_cursor()` / `terminal.show_cursor()`).

Документация `Frame::set_cursor_position`: «The cursor is applied after Ratatui flushes the
frame's buffer diff… this will interfere with calls to `Terminal::hide_cursor`,
`Terminal::show_cursor`, and `Terminal::set_cursor_position`. Pick one of the APIs and stick
with it». Общие рекомендации `Terminal` отдельно предупреждают, что ручной `MoveTo` вне
рендер-прохода может быть перебит следующим кадром или разъехаться с diff.

Сценарий: на медленном терминале/через SSH видно, как курсор за кадр прыгает дважды (ratatui
поставил → `MoveTo` поставил ещё раз). Комментарий в коде («prevent flicker onto (0,0)»)
описывает ровно тот симптом, который `set_cursor_position` и убирает: если он поставлен,
ratatui сам и показывает, и позиционирует курсор, и прячет при его отсутствии.

Предложение: убрать блок `if let Some((cx, cy)) = cursor_cell.get() { MoveTo }` и весь
блок `hide_cursor()/show_cursor()` — **но сначала** перевести онбординг на тот же API:
`render_onboarding` сейчас только пишет в `cursor_cell`
(`cursor_cell.set(Some((input_inner.x + cursor_col, input_inner.y)))`) и
`frame.set_cursor_position` не вызывает вовсе, то есть для экрана онбординга ручной `MoveTo`
несущий. Порядок правки: (1) в `render_onboarding` заменить запись в `Cell` на
`frame.set_cursor_position((input_inner.x + cursor_col, input_inner.y))`; (2) удалить
`MoveTo` и `hide/show_cursor` из `render`; (3) выкинуть `cursor_cell` из сигнатур
`render`, `render_input`, `render_landing`, `render_onboarding`. Итог: один API, курсор
ставится до flush и не прыгает.

### 6. Подсветка syntect пересчитывается на каждом токене стрима — major, проверено (без замеров)

`src/tui/markdown.rs` → `highlight_code`; вызов из `render_markdown` при `TagEnd::CodeBlock`.

Пока код-блок открыт, каждый кадр парсится заново весь текст сообщения, и `highlight_code`
получает **весь** растущий блок (pulldown-cmark закрывает незакрытый фенс на конце документа —
так требует CommonMark; поэтому блок виден ещё до закрывающей тройки обратных кавычек и
подсвечивается каждый кадр целиком). Ключ кэша — `(code.to_string(), lang, theme.warning)`, то есть
новый на каждом токене: попадания нет, `syntect` заново проходит весь блок. Дополнительно
`code.to_string()` копирует весь блок даже когда он больше `HIGHLIGHT_CACHE_MAX_BYTES` (64 КБ)
и заведомо не кэшируется.

Сценарий: модель стримит ответ с кодом на 150 строк. На каждый токен — полный rechighlight
этих 150 строк на главном потоке, плюс копия блока под ключ. Это самая дорогая часть кадра и
прямая причина «подтормаживает, пока пишется код». Замеров не делал (запуск запрещён),
величина — предположение, механизм — проверен по коду.

Предложение: инкрементальный кэш вместо «весь блок заново».
`syntect::parsing::ParseState` + `HighlightLines` умеют продолжать с состояния: хранить
`(lang, Vec<Line<'static>>, ParseState, HighlightState, processed_bytes)` в `HashMap` под
ключом `(lang, первые N байт блока)` и дорисовывать только новые строки. Более дешёвый
шаг того же эффекта: не перерисовывать стримящееся сообщение чаще, чем раз в 40–60 мс
(сейчас `dirty` ставится на каждый `Agent`-event, а `settle_frame` рисует один раз на пачку —
но пачки приходят часто), и не копировать `code` в ключ, а хэшировать (`DefaultHasher` по
`&str` + длина).

### 7. Двойное глубокое копирование сообщения на промахе кэша — minor, проверено

`src/tui/widgets/chat.rs` → `with_assistant_cached`.

На промахе: `render_markdown` строит `Vec<Line<'static>>`, затем
`wrapped_row_count(rendered_lines.clone(), …)` (копия целиком ради `line_count`), затем
`cached.lines.clone()` в материализующем проходе. То есть на каждый новый токен — два полных
клона всех `String`/`Span` сообщения. Плюс `compact_lines` удаляет ведущие пустые строки через
`lines.remove(0)` в цикле (O(n) на каждую).

Предложение: хранить в `CachedMsg` уже собранный `Text<'static>` (или `Paragraph`) и звать
`line_count` на нём; для отдачи наружу держать `Rc<[Line<'static>]>`/`Arc`, чтобы
материализация была дешёвой ссылкой, а не копией. `compact_lines` → `drain(..n)`.

### 8. Ввод: обёртка считается дважды за кадр и режет слова — minor, проверено

`src/tui/widgets/input.rs` → `render_input` + `cursor_pos_inner`.

`logical`, `line_starts_from_logical` и `build_visual_lines` считаются в `render_input`, а
затем **ещё раз** в `cursor_pos_inner` на тот же кадр. Внутри — `Vec<char>` на каждую логическую
строку и `UnicodeWidthStr::width(ch.to_string().as_str())` на каждый символ, то есть аллокация
`String` на символ, дважды за кадр.

Второе: перенос делается по границе «сколько влезло символов», без учёта пробелов — слово
рвётся посередине (`Paragraph` с `Wrap` рвёт по словам). На длинном сообщении это заметно:
текст в поле ввода переносится иначе, чем в чате.

Третье: `.max(1)` в цикле ломает нулевую ширину — комбинирующие знаки (тайский, деванагари,
диакритика) считаются за 1 клетку, поэтому строка обрывается раньше, чем нужно.

Предложение: разложить один раз — `struct InputLayout { lines: Vec<VisualLine>, cursor: (usize, u16) }`,
функция `measure_input(&str, width) -> InputLayout`, вызывать её в `render_input` и передавать
готовое в `cursor_pos_inner` (или возвращать `(Rect, cursor)` из одного прохода). Ширину
символа брать через `unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0)` без `.max(1)`
(единица — только для шага курсора по нулевому символу). Перенос — по границам слов, как
`Paragraph::wrap(Wrap { trim: false })`: проще всего отдать перенос самому `Paragraph`
(`line_count` даёт число строк), а `cursor_pos_inner` считать по той же раскладке.

### 9. `Modifier::REVERSED` вместе с явными fg/bg отменяет задуманные цвета — minor, проверено

`src/tui/widgets/input.rs::render_input` (`sel_style`: `fg(theme.fg).bg(theme.selection).add_modifier(REVERSED)`)
и `src/tui/render/popup.rs::push_text_box_lines` (курсор: `fg(theme.bg).bg(theme.accent).add_modifier(REVERSED)`).

`REVERSED` уходит в терминал как SGR 7 и меняет местами уже применённые fg/bg. Итог: выделение
рисуется не «фоном theme.selection», а инверсией (светлый фон из `theme.fg`), а блочный курсор
в модалках — не залитым блоком, а акцентным символом на фоне `theme.bg`. Проверено чтением:
`Modifier::REVERSED` → `crossterm::Attribute::Reverse`.

Предложение: убрать `REVERSED` там, где цвет задан явно (`bg(theme.selection)` достаточно),
или наоборот — задать только `REVERSED` без fg/bg.

### 10. Мерки ширины: `.chars().count()` и `.max(1)` ломают CJK/эмодзи — minor, проверено

Места:
- `render/status.rs::render_attach_bar` — `entry.chars().count()` против `max_w`;
- `widgets/panel.rs::mcp_row_layout`, `section_header`, `data_row` — `.chars().count()`;
- `render/modals.rs::render_autocomplete` — `max_name_width`/`padding` по `.chars().count()`;
- `render/util.rs::truncate` / `fit_col` — посимвольная ширина через `UnicodeWidthStr::width(ch.to_string())`.

Почему это дефект: `unicode-width 0.2` считает **строку** иначе, чем сумму её символов — ZWJ-эмодзи
(«👨‍👩‍👧») имеет ширину 2, а по символам получается 6; комбинирующие знаки имеют ширину 0, а
`.max(1)` делает из них 1. Приложение считает одно, `Paragraph`/`Span` рисует другое →
колонки таблицы сессий на лендинге, строка вложений, ряды MCP и автодополнение разъезжаются
на кириллице с диакритикой, CJK и эмодзи.

Предложение: одна функция `display_width(&str) -> usize` = `UnicodeWidthStr::width(s)` на
всей строке и одна `fit_col`/`truncate`, использующая её на префиксе (резать по границам
графем через `unicode-segmentation`, которая уже есть в дереве через `textwrap`); в
остальных местах заменить `.chars().count()` на `.width()` у `Span`/`Line` (0.30) или на
`util::truncate`/`fit_col`.

### 11. У чата нет индикатора прокрутки, а колесо не ограничено содержимым — minor, проверено

`src/tui/widgets/chat.rs::render_chat` считает `total_rows`/`max_scroll`, но наружу это не
выходит: пользователь не видит, что он не внизу.

`src/app/keys/mouse.rs::handle_mouse` делает `scroll_offset.saturating_add(WHEEL_STEP)` без
знания `max_scroll`, а `render_chat` клампит только локально
(`max_scroll.saturating_sub(scroll_from_bottom)`).

Сценарий: короткий диалог, пользователь крутит колесо вверх 30 раз — `scroll_offset` = 90, на
экране ничего не меняется. Потом нужно 30 прокруток вниз, чтобы сдвинулось хоть что-то.

Предложение: (а) `Scrollbar::new(ScrollbarOrientation::VerticalRight)` +
`ScrollbarState::new(total_rows).position(top_row).viewport_content_length(visible_height)`,
`frame.render_stateful_widget(...)` справа от области чата — рисуется только когда
`total_rows > visible_height`; (б) дать приложению знать `max_scroll`, не ломая чистоту
рендера: вынести подсчёт строк в `ChatLayout::measure(&AppState, width) -> (total_rows, row_starts)`
(кэш `message_rows` уже делает это дешёвым) и звать его из обработчика колеса/PageUp/PageDown
для клампа `scroll_offset = min(scroll_offset, max_scroll)`. Альтернатива без изменения
инварианта: маленький `Cell<u32>`-хэндл `viewport` в `AppState`, который рендер только
заполняет (как сейчас делает `cursor_cell`).

### 12. Нет синхронного вывода (DEC 2026) — minor, предположение о заметности

`src/tui/render/mod.rs::render` → `terminal.draw`.

Прокрутка чата меняет почти все клетки экрана; при обычном diff-based выводе терминал может
показать промежуточное состояние (полосы/рваньё при быстрой прокрутке и на стриме). В
crossterm 0.29 есть `terminal::BeginSynchronizedUpdate` / `EndSynchronizedUpdate` (CSI `?2026h/l`),
и документация `ratatui_crossterm::CrosstermBackend` показывает пример через
`SynchronizedUpdate::sync_update`.

Предложение (нужен небольшой спайк, точная интеграция не проверена): обернуть кадр
`execute!(terminal.backend_mut(), BeginSynchronizedUpdate)?; terminal.draw(...)?;
execute!(terminal.backend_mut(), EndSynchronizedUpdate)?;` — с учётом того, что бэкенд
буферизует вывод и `Backend::flush` вызывается внутри `draw`, начало и конец должны попасть в
разные flush'и, иначе эффекта нет. Если это окажется невозможно без своего `Backend`-обёртки,
цена выше пользы — тогда пропустить.

### 13. Раскладка собирается `Vec<Constraint>` + индексами, и один индекс уже врёт — minor, проверено

`src/tui/render/mod.rs::render` (ветка чата) и `src/tui/render/landing.rs::render_landing`.

В чате индексы считаются вручную: `attach_idx = 2`, `input_idx = 3 или 2`,
`border_idx`, `status_idx`, при этом `chunks[2]` в конце используется как «область ввода» для
автодополнения:

```rust
if state.autocomplete.visible {
    modals::render_autocomplete(frame, chunks[2], state, &theme);
}
```

С вложениями `chunks[2]` — это **полоса вложений**, а ввод лежит в `chunks[3]`. Значит
`@`-автодополнение при прикреплённых файлах рисуется на строку выше, чем нужно (и накрывает
полосу вложений). Проверено: `render_autocomplete` использует из аргумента только
`x`, `y`, `width`, а `popup_y = input_area.y - popup_h`.

В лендинге то же самое в другой форме: ручной счётчик `ci` после `Vec<Constraint>`; при
`show_sessions == false` индексы сдвигаются, и это держится только на дисциплине.

Предложение: перейти на 0.30-API с проверкой на этапе компиляции:

```rust
let [_, sep, attach, input, border, status] = area.layout(&Layout::vertical([
    Constraint::Fill(1), Constraint::Length(1), Constraint::Length(1),
    Constraint::Length(4), Constraint::Length(1), Constraint::Length(1),
]));
```

для двух вариантов (с вложениями / без) — две ветки с разной арностью массива; либо
`layout.areas::<N>(area)` / `Rect::layout_vec`. Автодополнение привязать к `input` явно.

### 14. Маленький терминал: нет минимального размера, лендинг рисует за границы области — minor, проверено

Нет ни одной проверки «слишком мало», ни в `render/mod.rs`, ни в виджетах.

`render/landing.rs`: `let inp_area = Rect::new(x, chunks[ci].y, input_width, 4);` — высота
жёстко 4, независимо от того, сколько дал `Layout`. На окне высотой 8–10 поле ввода выезжает
на строку шорткатов/статуса и перекрывает их (клип по буферу кадра не даёт паники, но
картинка разваливается). В чате при высоте < 8 `Constraint::Min(1)` получает 0 — чат
исчезает, остаются только рамки.

Предложение: в начале `render()` ранний выход на «слишком мало»:
`if area.width < 40 || area.height < 10 { frame.render_widget(Paragraph::new("terminal too small").alignment(Alignment::Center), area); return; }`
(или отдельная мини-подсказка с текущим размером). И в лендинге брать высоту как
`chunks[ci].height.min(4)`.

### 15. Цвета: только TrueColor, без деградации и без NO_COLOR — major при поддержке Win7/8.1, иначе minor; проверено по документации

`src/tui/theme.rs` — все роли это `Color::Rgb`. Ни `NO_COLOR`, ни `TERM`, ни `Color::Indexed`
в путях отрисовки нет (`grep NO_COLOR|COLORTERM|Color::Indexed` по `src/` даёт единственное
совпадение — `COLORTERM=truecolor` для дочерних процессов, см. ниже).

README crossterm 0.29: «RGB color support (Windows 10 and UNIX only)» при том, что сам crossterm
заявляет поддержку консолей вплоть до Windows 7. То есть на Win7/8.1 conhost 24-битный SGR не
поддерживается, и палитра из `Color::Rgb` деградирует не по нашему выбору, а как решит консоль
(обычно — в дефолтные цвета, из-за чего тёмный фон с тёмным текстом становится нечитаемым).
Как именно поведёт себя конкретный conhost — **предположение**, по документации подтверждено
только «RGB не поддерживается вне Win10+/Unix».

Предложение: один раз при старте определить уровень цвета (`NO_COLOR` → монохром;
`COLORTERM=truecolor|24bit` или Windows 10+ → `Rgb`; иначе → `Indexed` по ближайшему цвету
палитры xterm-256) и хранить в `Theme` уже квантованные цвета. Квантование — маленькая
функция `fn quantize(color: Color) -> Color` в `theme.rs` (пресеты остаются `Rgb`, деградация
одна на всю программу). Это же чинит тему на `TERM=xterm` в ssh-сессиях. Попутно: сейчас
приложение само требует от дочерних процессов TrueColor
(`tools/background/spawn.rs` ставит `COLORTERM=truecolor`), а своя отрисовка при этом никак
не деградирует — несогласованность стоит закрыть вместе с этой правкой.

### 16. `Theme::resolve` на каждый кадр — minor, проверено

`src/tui/render/mod.rs::render`: `Theme::resolve(&config.ui)` вызывается всегда (17 цветов),
а для кастомной темы `Theme::from_custom` ещё и проходит `BTreeMap` с `parse_hex` и
`String`-ами. Плюс `state.themes.preview_theme(config)` для `/themes`.

Предложение: держать разрешённую `Theme` в `AppState` (или в `App`), пересчитывать при
`apply_theme`/загрузке конфига/изменении превью. Побочно это даёт готовый `theme_hash` для
находки 4.

### 17. Шум в `EventStream`: ошибки проглатываются, `None` глушит ввод — minor, проверено (семантика tokio по докам)

`src/app/mod.rs::run_loop`:

```rust
Some(Ok(event)) = event_stream.next() => { ... }
```

При `Some(Err(_))` паттерн не совпадает — ветка отключается на эту итерацию `select!`, ошибка
никуда не пишется. При `None` (поток завершился) ветка отключается навсегда: клавиатура
перестаёт работать молча, остаётся только тик и `AppEvent`.

Предложение:

```rust
maybe_event = event_stream.next() => match maybe_event {
    Some(Ok(event)) => { /* как сейчас */ }
    Some(Err(error)) => crate::debug_log::log("tui.event_stream", format!("{error:?}")),
    None => { /* пересоздать EventStream или вернуть ошибку */ }
}
```

### 18. Пауза 25 мс на каждое текстовое нажатие под Windows — minor, проверено

`src/app/mod.rs::gather_paste_burst`: на Windows первое текстовое нажатие ждёт следующее
событие с таймаутом `paste_burst::GAP` = 25 мс (`timeout(wait, stream.next()).await`).
Пока таймаут не истечёт, нажатие не обработано и кадр не нарисован.

Сценарий: обычный набор текста — +25 мс к задержке каждой буквы (стрелки, Esc, Ctrl-сочетания
задержки не получают: они не «текстовые нажатия» и выходят из `gather_paste_burst` сразу).

Предложение: это осознанный размен (иначе Enter внутри вставки отправит сообщение), но его
можно сузить: сначала обработать первое нажатие и нарисовать кадр, а уже потом «догонять»
серию; либо уменьшить `GAP` до 8–12 мс (вставка приходит в один `poll`, 25 мс — с большим
запасом); либо включать коалесцер только когда терминал не умеет bracketed paste
(`EnableBracketedPaste` уже отправляется и на Windows — если conhost её поддерживает,
коалесцер вообще не нужен, но это **предположение**, нужна проверка на реальном conhost).

### 19. Mouse capture включён, а drag-выделения и копирования текста нет — minor, проверено

`tui/mod.rs::init` включает `EnableMouseCapture`; `app/keys/mouse.rs::handle_mouse` реагирует
только на `ScrollUp/ScrollDown`, остальные виды событий сразу `return` (в комментарии — «Clicks,
drags and moves are not wired to anything yet»). Внутри приложения выделение есть только в поле
ввода (`InputState::selection_anchor`), копирование транскрипта не реализовано
(`crate::clipboard::copy` используется лишь в онбординге), команды `/copy` нет.

Сценарий: пользователь хочет скопировать ответ модели мышью — получает либо ничего, либо
зависит от того, поддерживает ли терминал Shift+drag. На conhost это худший случай.

Предложение: либо реализовать выделение в чате (нужна обратная карта «экран → (message, char)»,
удобно строить её из того же прохода, что и `row_starts`), либо добавить `/copy [N]` через
`clipboard::copy` (буфер уже есть, `spawn_blocking` внутри) и написать об этом в подсказке
статуса. Как минимум — `/copy` дешевле и закрывает 90% потребности.

### 20. `Paragraph::line_count` — unstable-фича в зависимости — minor, проверено; решение оставить

`Cargo.toml`: `features = ["unstable-rendered-line-info"]`, с комментарием зачем. Это
оправдано: точная арифметика прокрутки без него была бы приблизительной (тесты на CJK это
фиксируют). Риск: API помечен unstable («no stability guarantees»), сломается на минорном
апгрейде ratatui.

Предложение: оставить, но закрепить версию (`ratatui = "=0.30.2"` или ручной апгрейд с
прогоном тестов `wrapped_row_count_*`) и добавить в `.memories` строку «при апгрейде ratatui
проверить `Paragraph::line_count` и тесты wrap».

### 21. Мелочи, чинятся попутно — minor, проверено

- `render/mod.rs`: фон кадра красится `Paragraph::new(String::new())` — аллокация `String` +
  виджет на каждый кадр; `Block::default().style(Style::default().bg(theme.bg))` делает то же
  без строки-пустышки (`Clear` не подходит: он сбрасывает стиль клетки в дефолт).
- `render/mod.rs::sync_window_title`: `window_title()` строит `String` и сканирует сообщения
  каждый кадр, и тут же пишет OSC в бэкенд из рендера. Считать заголовок по событиям
  (смена чата/первое сообщение) и хранить в состоянии.
- `widgets/chat.rs`: `local_offset as u16` в `Paragraph::scroll` — для одного сообщения выше
  65 535 строк (≈8 МБ текста при 120 колонках) смещение молча обрежется. Практически
  недостижимо, но проверка `u16::try_from(local_offset)` с фолбэком на «без culling» стоит
  одну строку.
- `widgets/chat.rs`: `CachedMsg::meta_rows` считается в момент сборки кэша, а `meta_line`
  материализуется с текущим числом токенов; если `total_tokens` приезжает уже после
  последнего дельта-токена, число строк мета-строки может разойтись на 1 и низ чата
  «поедет» на строку. **предположение** (нужен сценарий с узкой областью и большим числом
  токенов), лечится добавлением `tokens` в `CacheKey` либо пересчётом `meta_rows` при
  смене `total_tokens`.
- `render/util.rs::truncate` при `max == 0` возвращает `"…"` (1 колонка) — выход за лимит.

---

## 3. Что сделано хорошо, не трогать

- **Цикл и кадры.** `run_loop` + `settle_frame`: один `select!`, дренаж до 256 событий,
  потом один кадр за флагом `dirty`; простой тик ничего не рисует (`tick_is_visual`).
  Это правильная архитектура для immediate-mode: 8 fps в покое — 0 кадров.
- **Точная арифметика прокрутки.** `Paragraph::line_count` с тем же
  `Wrap { trim: false }`, что и у отрисовки; кэш числа строк на сообщение; culling по
  `row_starts`; прокрутка внутрь первого видимого сообщения, а не всего транскрипта
  (заодно не переполняет `u16`). Тесты на CJK/длинные слова фиксируют именно то, что нужно.
- **Кэши.** Три уровня (сообщение, число строк, токены), ограничены по размеру
  (`MAX_CACHE_ENTRIES`, `HIGHLIGHT_CACHE_CAP/BYTES`), с отпечатком контента против коллизий
  ключей при удалении сообщений. Комментарии объясняют, почему ключ такой.
- **Отрисовка поверх.** `Clear` перед каждым попапом (`draw_popup`), фон кадра красится
  первым слоем, панель статистики сама чистит область и резервирует ровно ту ширину, которую
  рисует (`panel_width` — с тестами). Дубль «панель перекрыла чат» закрыт.
- **Передача терминала дочернему процессу** (`needs_terminal_restore`): LeaveAlternateScreen →
  работа → EnterAlternateScreen → `terminal.clear()`. Это ровно то, что документация
  `Terminal` советует делать после ручных escape-последовательностей (сброс previous buffer →
  полная перерисовка). Не упрощать.
- **Считающая арифметика без паник.** `saturating_sub` везде, включая `mcp_row_layout`
  (бывший underflow) и `status_bar_gap`; на это есть тесты.
- **Тема как семантические роли**, без хардкода RGB в виджетах; пресеты, кастомные темы с
  мягкой деградацией на неизвестное имя/цвет; `to_hex`/`parse_hex` с round-trip тестами.
- **Клавиатура/вставка.** Char-boundary-safe утилиты (`util::truncate_at_char_boundary`),
  bracketed paste + Windows-коалесцер + sanitize, AltGr как текст, Shift+Enter как перевод
  строки. Это продумано лучше, чем в большинстве TUI.
- `ratatui 0.30` + `crossterm 0.29` подобраны согласованно (одна версия crossterm в дереве),
  `event-stream` включён — правильная база.

---

## 4. План улучшений по порядку

| # | Что | Объём | Эффект |
|---|---|---|---|
| 1 | `refresh_autocomplete`: I/O в `spawn_blocking` + `AppEvent`, дебаунс, без `metadata()` на файл (находка 1) | ~60 строк, 1 новое событие | убирает единственное реальное залипание ввода |
| 2 | Паника-хук / `ratatui::init`+`restore` (находка 2) | ~15 строк | терминал не остаётся сломанным |
| 3 | Диск из рендера: тег сессии и список сессий в состояние по событиям (находка 3) | ~80 строк, 2 события | убирает I/O из кадра |
| 4 | `theme_hash` в `CacheKey` (или сброс кэша по смене темы) + кэш `Theme` (находки 4, 16) | ~25 строк | чинит смену темы, минус аллокации |
| 5 | Кэш подсветки: не копировать `code` в ключ; дроссель отрисовки стрима (находка 6) | ~30 строк | ощутимо ровнее стрим кода |
| 6 | Автодополнение к `input`-области, `Rect::layout`/`Layout::areas` вместо индексов (находка 13) | ~50 строк | убирает баг с вложениями, компилятор ловит сдвиг |
| 7 | Один курсорный API: онбординг на `set_cursor_position`, убрать `MoveTo`/`show_cursor` (находка 5) | ~25 строк | нет двойного прыжка курсора |
| 8 | Ввод: один проход раскладки, `UnicodeWidthChar`, перенос по словам (находка 8) | ~80 строк | ровнее перенос, меньше аллокаций |
| 9 | `Scrollbar` + кламп колеса через `measure` (находка 11) | ~60 строк | видно, где ты в диалоге |
| 10 | Минимальный размер окна + `chunks[..].height.min(4)` (находка 14) | ~20 строк | не разваливается на узком окне |
| 11 | Ширина: `display_width`/`fit_col` на графемах, убрать `.chars().count()` (находка 10) | ~50 строк + тесты | колонки не разъезжаются |
| 12 | `REVERSED` vs явные цвета (находка 9) | ~5 строк | выделение/курсор как задумано |
| 13 | Деградация цвета (`NO_COLOR`, `Indexed`) (находка 15) | ~70 строк + 1 функция | читаемо на xterm/старом conhost |
| 14 | `/copy` или выделение мышью (находка 19) | 40–200 строк | закрывает дыру с копированием |
| 15 | Синхронный вывод (находка 12) — спайк, потом решить | 1–2 часа на проверку | меньше рванья при прокрутке |
| 16 | Логирование ошибок `EventStream`, мелочи из 21 (находки 17, 21) | ~30 строк | не молчит при сбое ввода |

Порядок — по отношению «риск/польза»: 1–4 обязательны (инварианты и видимые дефекты), 5–9
дают заметную плавность, 10–16 — полировка.

## 5. Чего не проверял

- Замеров производительности не делал (запуск запрещён): все оценки стоимости — по коду.
- Поведение конкретного Windows conhost (bracketed paste, RGB, DEC 2026, kitty keyboard) —
  не проверено на живом терминале; в тексте это помечено как предположение.
- Kitty keyboard protocol (`PushKeyboardEnhancementFlags` + `supports_keyboard_enhancement`)
  в коде не включается: Shift+Enter полагается на то, что терминал различит модификатор.
  На Windows Terminal без CSI-u это может не сработать, но проверки не было — поэтому в
  находки не вынес, только сюда.
