# REFERENCE: самообновление (`/update`, `/autoupdate`, каналы stable/dev)

> Обновлятор и его контракт с CI. Источники правды: `src/update/` (`mod.rs`,
> `manifest.rs`, `swap.rs`, `install_record.rs`), `.github/workflows/ci.yml`
> (`publish-dev`), `.github/workflows/release.yml`, `scripts/ci/make-manifest.sh`,
> `commands/defs/update.rs`, `app/keys/dispatch.rs::apply_update_action`.
> Переписан 2026-09-13 (`JOURNAL/2026-09-13-distribution.md`); прежняя схема —
> один rolling-тег `latest` и сравнение хэша — описана в `JOURNAL/2026-07-07.md`.
> Выпуск релизов и установщики — `reference/RELEASING.md`.

## ЧТО ДЕЛАЕТ

`update::run(channel)` скачивает `manifest.json` канала и решает, ставить ли сборку:

| Канал | Манифест | Ставит, если |
|---|---|---|
| `stable` (по умолчанию) | `releases/latest/download/manifest.json` — последний не-prerelease релиз | хэш файла на диске ≠ хэшу из манифеста **и** `manifest.version` > `CARGO_PKG_VERSION` (SemVer, `manifest::Version`) |
| `dev` | `releases/download/dev/manifest.json` — rolling prerelease | хэш файла на диске ≠ хэшу из манифеста |

Дальше: бинарник качается по `releases/download/<manifest.tag>/<asset>`,
сверяется с хэшем и подменяется через `swap::install`. Новая версия работает
со следующего запуска. На Windows после замены `install_record::sync_display_version`
правит `DisplayVersion` в записи установщика, если `InstallLocation` совпадает с папкой exe.

Исходы (`UpdateOutcome`):
- `UpToDate { channel_build }` — ставить нечего.
- `Updated { build }` — заменено, нужен перезапуск.
- `PendingRestart { build }` — этот процесс уже ставил сборку. Повторная замена
  сломалась бы: на Windows `.old` занят запущенным образом, на Linux
  `current_exe` указывает на удалённый inode. Флаг `INSTALLED_THIS_RUN`.
- `NoRelease` — манифест канала отдаёт 404 (например, до первого тега). Не ошибка,
  при автопроверке молчит.

Команды:
- **`/update`** — проверить сейчас. В debug-сборке сначала модалка (`ConfirmState::update_dev`).
- **`/update channel [stable|dev]`** — показать или сохранить `[update] channel`.
- **`/autoupdate [on|off]`** — `[update] auto` (по умолчанию off), проверка на каждом старте `App::new`.

Поток: команда → `CommandResult::Update(UpdateAction)` → `apply_update_action` →
`app::spawn_update_task(event_tx, in_flight, channel, quiet)` → `update::run` вне
цикла событий → `AppEvent::UpdateStatus { message, notable }`.

## ФОРМАТ `manifest.json`

Пишет `scripts/ci/make-manifest.sh`, читают `update::manifest::Manifest` (serde)
и `scripts/install.sh` (sed, **одна пара `"ключ": "значение"` на строку**).

```json
{
  "schema": 1,
  "version": "0.2.0",
  "tag": "v0.2.0",
  "commit": "<sha>",
  "assets": {
    "pooprusteek-linux-x86_64": "<sha256>",
    "pooprusteek-windows-x86_64.exe": "<sha256>"
  }
}
```

- `schema` ≠ 1 — клиент отказывается («update manually»). Новые поля добавлять
  можно (serde их игнорирует), менять смысл старых — только с новой схемой.
- `tag` должен начинаться с буквы или цифры и состоять из `[A-Za-z0-9._-]` —
  он идёт в URL.
- В `assets` только «сырые» бинарники: архивы, `-setup.exe`, `install.sh` сюда не попадают.
- `commit` показывается в сообщениях dev-канала: у dev-сборок одна версия на много сборок.

## ЗАМЕНА ФАЙЛА (`swap::install` / `promote`)

Запущенный бинарник **переименовывается**, а не перезаписывается.
1. Байты целиком пишутся в `<exe>.new` через `atomic_write`.
2. `promote`:
   - **Unix**: один атомарный `rename(.new, exe)`.
   - **Windows**: `rename(exe, .old)`, затем `rename(.new, exe)`, при сбое откат.
     Если `.old` не удаляется (его держит ещё работающий старый процесс),
     используется `.old.<pid>`.
3. `cleanup_stale_backup()` в `App::new` удаляет `.new`, `.old` и `.old.<pid>`
   (`swap::is_backup_name`).

## ТОЧКИ КОНТРАКТА — НЕ РАССИНХРОНИЗИРОВАТЬ

| Контракт | Приложение | CI / установщик |
|---|---|---|
| Имена целей `windows-x86_64`, `windows-arm64`, `linux-x86_64`, `linux-arm64`, `macos-arm64` | `update::platform_target` | матрица `build.yml` (`target`) |
| Имя ассета `pooprusteek-<target>[.exe]` | `update::platform_asset` | `scripts/ci/package.sh`, `install.sh::do_install` |
| Формат манифеста | `update::manifest` | `make-manifest.sh`, `install.sh::manifest_value` |
| Имя `manifest.json` | `manifest::MANIFEST_ASSET` | `make-manifest.sh`, `files:` в `ci.yml`/`release.yml` |
| Тег dev-канала `dev` | `update::DEV_TAG` | `ci.yml` `publish-dev` (`TAG: dev`), `install.sh` |
| stable = GitHub «Latest» | `update::manifest_url` | `release.yml`: `make_latest: "true"`; у `dev` и legacy `latest` — `false` + prerelease |
| Репозиторий `Aver005/pooprusteek` | `update::RELEASES_BASE` | реальный путь репозитория, `install.sh::REPO_URL` |
| `AppId` установщика | `install_record::UNINSTALL_KEY` | `packaging/windows/pooprusteek.iss` (`AppId`, `UninstallKey`) |
| Мьютекс `PooprusteekRunning` | `install_record::INSTANCE_MUTEX` (создаётся в `main` через `update::register_running_instance`) | `.iss` `AppMutex` |
| Порядок загрузки ассетов | — | `manifest.json` последним (`preserve_order`), чтобы новый манифест не опередил бинарники |

## LEGACY-МОСТ ДЛЯ СБОРОК ≤ 0.1.0

Старый обновлятор читает `releases/download/latest/SHA256SUMS` и ставит при несовпадении хэша.
Джоба `legacy-latest` в `release.yml` после каждого стабильного релиза переносит тег
`latest` и кладёт туда те же сырые бинарники и `SHA256SUMS`. Старые сборки получают
стабильную версию, а с ней и новый обновлятор. Develop-пуши `latest` больше не трогают.
**TODO(0.3.0): удалить джобу и тег.**

## РЕЖИМЫ ОТКАЗА

- **Канал пуст** (нет ни одного тегового релиза или dev ещё не собирался) → `NoRelease`.
- **Окно обновления релиза**: ассеты перезаливаются по одному. Клиент может увидеть
  старый манифест с новым бинарником → «checksum mismatch — try /update again».
  Манифест грузится последним, так что наоборот не бывает.
- **Папка установки без прав записи** (Program Files, root-owned `/usr/local/bin`) →
  замена падает чисто. Установщик Windows не даёт выбрать такую папку
  (`NextButtonClick`), `install.sh` ставит в `~/.local/bin`.
- **dev → stable с той же версией** → `UpToDate`: stable не откатывает dev-сборку,
  текущая остаётся до выхода более новой версии (об этом говорит `/update channel stable`).
- **`SHA256SUMS` и манифест лежат рядом с бинарником** → целостность, не подлинность.
  Доверие держится на TLS и аккаунте GitHub; подписи нет.
- **Переименование репозитория** → URL в `RELEASES_BASE` и `install.sh` протухнут.

## ТЕСТЫ

- `update::tests`: вектор sha256, имена ассетов, stable только вперёд, URL манифестов.
- `update::manifest::tests`: разбор, битый хэш против отсутствующей сборки,
  опасные теги, порядок SemVer вместе с пререлизами.
- `update::swap::tests`: суффиксы, распознавание `.old.<pid>`, замена и повторная замена.
- `update::install_record::tests`: сравнение путей.
- `config::tests::update_channel_round_trips_and_rejects_unknown`,
  `commands::defs::update::tests`.
