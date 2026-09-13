# REFERENCE: выпуск релизов, установщики, CI

> Как код становится установленной программой. Добавлено 2026-09-13
> (`JOURNAL/2026-09-13-distribution.md`). Обновлятор — `reference/AUTO-UPDATE.md`.

## ВЫПУСК СТАБИЛЬНОГО РЕЛИЗА

```sh
bash scripts/release.sh patch|minor|major|X.Y.Z [--dry-run] [--yes]
```

Что делает `scripts/release.sh`:
1. **Проверки**: ветка `develop`, чистое дерево, не отстаёт от `origin`, тега нет
   ни локально, ни на `origin` (`git ls-remote`), под `## [Unreleased]` в CHANGELOG что-то есть.
2. Показывает версию и начало заметок; спрашивает подтверждение (`--yes` пропускает).
3. Правит `version` в `[package]` `Cargo.toml` и обновляет запись пакета в `Cargo.lock`
   (`cargo update --workspace --offline`: другие зависимости не трогает).
4. Вставляет `## [X.Y.Z] - дата` под `## [Unreleased]`.
5. Если pre-commit хук не включён (`core.hooksPath != .githooks`), сам гоняет fmt, clippy и тесты.
6. Коммит `chore(release): 🔖 Release vX.Y.Z`, аннотированный тег `vX.Y.Z`,
   `git push --atomic origin develop vX.Y.Z`.

При ошибке до коммита файлы откатываются. При ошибке пуша скрипт печатает, как повторить
или переделать релиз поверх сдвинувшейся ветки.

Грабли, уже обойдённые:
- `git fetch --tags` нельзя: CI переносит теги `dev` и `latest` с `--force`, и fetch
  отказывается перезаписать локальные («would clobber existing tag»).
- `git checkout HEAD -- a b c` падает целиком, если хоть одного файла нет в HEAD.
  Поэтому откат идёт по файлу.

## ПАЙПЛАЙН

```
push/PR ──► ci.yml ──► checks.yml (тесты win/linux/macos + fmt/clippy)
                  └─(push в develop)─► build.yml ──► publish-dev  → релиз `dev` (prerelease)
tag v* ──► release.yml ──► verify ──► checks.yml ──► build.yml ──► publish → «Latest»
                                                              └──► legacy-latest (TODO 0.3.0)
```

- **`checks.yml`** (reusable): `cargo build` + `cargo test` на трёх ОС, `fmt --check`, `clippy -D warnings`.
  На macOS выбирается stable Xcode: на свежем нет `clang_rt.osx` для `aws-lc-sys`.
- **`build.yml`** (reusable):
  - `binaries` — матрица целей → артефакты `bin-<target>` (сырой бинарник + zip/tar.gz
    с LICENSE и README), см. `scripts/ci/package.sh`.
  - `windows-arm64` (`windows-11-arm`) и `linux-arm64` (`ubuntu-24.04-arm`) — `experimental`:
    `continue-on-error`, их падение не блокирует релиз. Сделать обязательными после первых зелёных прогонов.
  - `installer` — ставит Inno Setup **7.1.0** с GitHub jrsoftware (на раннере через
    Chocolatey только 6.x), компилирует `packaging/windows/pooprusteek.iss` → `installer-windows`.
- **`ci.yml` `publish-dev`**: `collect-assets.sh`, заметки из `dev-release.template.md`,
  перенос тега `dev`, обновление релиза **на месте** (без delete → create, чтобы канал не пропадал).
  Отмена параллельных прогонов только для PR.
- **`release.yml`**:
  - `verify`: версия — ровно `X.Y.Z` (пререлизам в «Latest» нельзя), тег == `v` + версия
    из `Cargo.toml`, в CHANGELOG есть раздел версии.
  - `publish`: заметки = `stable-release.template.md` + раздел CHANGELOG
    (`changelog-section.sh`), `make_latest: "true"`.
  - `legacy-latest`: мост для сборок ≤ 0.1.0 (см. AUTO-UPDATE.md), отдельная джоба —
    перезапускается без повторной публикации.

`scripts/ci/collect-assets.sh` проверяет обязательные ассеты (`windows-x86_64.exe`,
`linux-x86_64`, `macos-arm64`, `setup.exe`), кладёт `install.sh` и зовёт `make-manifest.sh`.

`scripts/render-release-notes.sh` — однопроходная подстановка `{{VAR}}`. Замена
`${text//tok/val}` не годится: в bash 5.2 `&` в замене подставляет найденное,
а `{{TAG}}` из текста коммита подставлялся бы повторно.

## АССЕТЫ РЕЛИЗА

| Файл | Для чего |
|---|---|
| `pooprusteek-setup.exe` | установщик Windows (x64; arm64-бинарник внутри, если собрался) |
| `install.sh` | установщик macOS/Linux |
| `pooprusteek-<target>[.exe]` | сырые бинарники — самообновление и `install.sh` |
| `pooprusteek-<target>.zip` / `.tar.gz` | ручная загрузка |
| `manifest.json` | версия, тег, коммит, хэши сырых бинарников |
| `SHA256SUMS` | хэши всех файлов (и legacy-обновлятор) |

## УСТАНОВЩИК WINDOWS (`packaging/windows/pooprusteek.iss`)

- Per-user, без UAC (`PrivilegesRequired=lowest`), `{autopf}\Pooprusteek` = `%LOCALAPPDATA%\Programs\Pooprusteek`.
- Страницы:
  - первая установка — выбор папки (кнопка «Установить») → прогресс → финал с галочкой запуска;
  - переустановка (`DisableDirPage=auto`) — прежняя папка и один экран Ready. Смена папки
    при переустановке оставила бы старую копию первой в PATH.
- `WizardStyle=modern dynamic windows11` (тема по системе), языки en/ru по языку системы без диалога.
- PATH пользователя через Pascal (`AddToUserPath` / `RemoveFromUserPath`), `ChangesEnvironment=yes`.
- Ярлык в «Пуске» и запуск после установки — с рабочей папкой `%USERPROFILE%`: агент работает в текущей папке.
  После установки запускается в Windows Terminal, если есть `wt.exe`.
- `AppMutex=PooprusteekRunning`: мьютекс создаёт приложение, установщик и деинсталлятор просят его закрыть.
- `InitializeSetup` предупреждает, если установлена более новая версия (`DisplayVersion`).
- `NextButtonClick` проверяет запись в выбранную папку: туда же пишет `/update`.
- Деинсталлятор убирает PATH и хвосты `.old*`/`.new`, спрашивает об удалении `%APPDATA%\pooprusteek`.
- Сборка локально: `ISCC /DAppVersion=0.2.0 /DBinDir=<папка с pooprusteek-windows-*.exe> /O<out> packaging\windows\pooprusteek.iss`.
- **Не проверено запуском** (только компиляцией): сам мастер, запуск через `wt.exe`, AppMutex.

## УСТАНОВЩИК macOS/Linux (`scripts/install.sh`)

POSIX sh (работает под dash). `curl -fsSL …/releases/latest/download/install.sh | sh [-s -- опции]`.
- Цели: `linux-x86_64`, `linux-arm64`, `macos-arm64`. Под Rosetta определяется arm64,
  Intel Mac получает отказ: для него нет готового ONNX Runtime.
- Качает манифест и бинарник, сверяет SHA-256, **проверяет `--version` до замены**
  (иначе несовместимый glibc затёр бы рабочую копию), атомарно ставит в `--dir`
  (по умолчанию `~/.local/bin`).
- PATH: строка с маркером `# added by pooprusteek installer` в rc текущей оболочки;
  при смене `--dir` старая строка заменяется. Rc переписывается через `cat >`, чтобы сохранить симлинк.
- `--uninstall`: бинарник (или найденный через PATH), строки PATH, данные — с вопросом через `/dev/tty`.

## ОГРАНИЧЕНИЯ ПЛАТФОРМ

- **Linux**: glibc ≥ 2.39 (Ubuntu 24.04 / Debian 13): prebuilt ONNX Runtime у `ort`
  собран на свежем glibc. musl не поддерживается.
- **macOS Intel**: нет prebuilt ONNX Runtime ни у `ort`, ни у апстрима.
- **Подписи нет**: SmartScreen на Windows («Подробнее → Выполнить в любом случае»).
  Через `curl` и `install.sh` Gatekeeper не срабатывает, в браузере скачанный бинарник блокируется.

## ОТКРЫТО

- Иконка приложения и установщика: у exe нет ресурса иконки, мастер со стандартной картинкой.
- Подпись (SignPath Foundation бесплатно для OSS), winget/scoop/Homebrew tap.
- Сделать arm64-цели обязательными после первых зелёных прогонов.
- Удалить `legacy-latest` в 0.3.0.
