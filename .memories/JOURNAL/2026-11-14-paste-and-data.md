# 2026-11-14 — вставка в консоли Windows, потерянные сессии, разные токены

Поводы: живое пользование релизной сборкой ветки `feature/gateway-release`
после удаления v0.1.1 из `System32`.

## Вставка (исправлено)

- Симптом: в окне токена Ctrl+V и правый клик ничего не вставляют, токен
  приходится набирать руками. Было в v0.1.1, осталось и в ветке.
- Причина: в crossterm на Windows нет bracketed paste. Консоль conhost в
  raw-режиме не вставляет по Ctrl+V сама и отдаёт его приложению как клавишу,
  а `EnableMouseCapture` забирает правый клик. Своего чтения буфера не было —
  `clipboard.rs` умел только писать. Windows Terminal Ctrl+V перехватывает сам,
  поэтому там бага не видно.
- Проверка до правки: настоящий conhost, временные `--config`/`--data-dir`,
  SendKeys `^v` и правый клик — поле пустое; набор `typed123` — 8 точек.
- Правка: `clipboard::read` (WinAPI `CF_UNICODETEXT`; на unix — `wl-paste`,
  `xclip`, `xsel`, `pbpaste`) и `clipboard::is_paste_shortcut` (Ctrl+V,
  Shift+Insert). В TUI сочетание и правый клик зовут
  `App::paste_from_clipboard`: чтение в `spawn_blocking`, текст уходит
  `AppEvent::Paste` в общий путь `handle_paste`, сбой — `AppEvent::PasteFailed`
  в строку статуса. В `pooprusteek secrets set` Ctrl+V раньше дописывал букву
  `v` — теперь читает буфер.
- `windows-sys`: добавлены features `Win32_System_DataExchange`,
  `Win32_System_Memory`, `Win32_System_Ole` (новых зависимостей нет).

## Сессии из старой папки (данные пользователя поправлены руками)

- Перенос `Roaming\pooprusteek` → `Local\pooprusteek` сделала 6.11 тестовая
  сборка; её метка `.migrated-from-legacy` в старом формате, без `sessions` и
  `started_ms`. `top_up_sessions` без списка намеренно ничего не копирует.
- 13.11 пользователь запускал v0.1.1, она завела 2 сессии в старой папке —
  в новую они не попали. Скопированы руками (19 = 19). Релизных пользователей
  не касается: метку старого формата писали только тестовые сборки.

## Токены (данные пользователя поправлены руками)

- Конфиг из старого места копируется один раз (`config.toml.pre-gateway`) и
  дальше не перечитывается. Токен, который v0.1.1 сохранила 13.11, новая
  версия не видела и брала токен 6.11 из хранилища.
- По просьбе пользователя запись `global/deepseek-token` в Credential Manager
  заменена токеном 13.11 (формат `psec2`, поколение 2, сумма сверена;
  `secrets list` — `ok`). Прежнее значение — `global/deepseek-token-backup-2026-11-06`.

## Сборка

- Только из PowerShell/cmd с `H:\VS Build Tools\...\vcvars64.bat` (MSVC 14.50).
  Git Bash подставляет GNU `link`; VS 2022 (14.41) не линкует ort.
- Релизный бинарь — `target\release`, папка добавлена в пользовательский PATH.
