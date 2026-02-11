# Dual Mode (Conan + Clear) Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Добавить второй режим работы CLI (`init-clear`) для подключения Conan-пакетов без установленного Conan, сохранив текущий режим `init` (через conanfile/spec/CMake) и единые команды `add/remove`.

**Architecture:** Вводим явный `project mode` (`conan`/`clear`) и маршрутизацию `add/remove` по режиму. В `clear`-режиме CLI сам резолвит граф зависимостей через JFrog, скачивает `conan_package.tgz`, распаковывает в локальный vendor-store проекта, генерирует локальные `.pc` и правит `CMakeLists.txt`/`.spec` под локальные пути. Для Conan-режима текущая логика сохраняется.

**Tech Stack:** Rust + существующие модули `app.rs`, `files.rs`, `conan.rs`; JFrog HTTP API; `tar`/архивный распаковщик в Rust; генерация `pkg-config` файлов.

---

## Контекст и выводы перед реализацией

1. По рецептам из `MyAuroraConan/recepies/*/conanfile.py` видно, что canonical-метаданные завязаны на `cpp_info` (`libs`, `requires`, `includedirs`, `system_libs`).
2. В бинарных `conan_package.tgz` обычно есть `include/`, `lib/`, `licenses/`, но не гарантированно есть `lib/pkgconfig`.
3. Header-only пакеты (например `ms-gsl`) содержат только headers/licenses; в `conaninfo` у них часто нет `arch`, поэтому для них нужен специальный тип `package`.
4. Пример `OnnxRunner` подтверждает текущий conan-путь: `PkgConfigDeps` + `pkg_check_modules` + `%build conan-install-if-modified` + `%install conan-deploy-libraries`.
5. Документация `docs/use-conan-packages.md` явно фиксирует CMake+pkg-config путь и требования к RPATH и `%{_datadir}/%{name}/lib`.

## Архитектурные решения

### 1) Явный режим проекта

Вводим mode-state файл в корне проекта: `.aurora-conan-cli-mode.json`.

Формат:
- `mode`: `"conan" | "clear"`
- `version`: schema version
- `store_dir`: относительный путь до локального хранилища (для clear)

Правило:
- `init` пишет `mode=conan`.
- `init-clear` пишет `mode=clear`.
- `add/remove` работают по mode-state.
- Если mode-state отсутствует: fallback-детект по наличию `conanfile.py` и managed-блоков.

### 2) Local store для clear-режима

Путь по умолчанию: `vendor/aurora-conan/` (в проекте).

Структура:
- `vendor/aurora-conan/archives/<name>/<version>/<arch>.tgz`
- `vendor/aurora-conan/prefix/<name>/<version>/<arch>/...` (распаковка)
- `vendor/aurora-conan/pkgconfig/*.pc`
- `vendor/aurora-conan/lock.json` (direct deps + resolved graph + installed artifacts)

Почему так:
- предсказуемый путь для `%build/%install`
- нет зависимости от Conan в SDK/PSDK
- легко пересчитать/очистить при `remove`

### 3) Подключение в CMake без Conan

Сохраняем общий принцип из текущего инструмента:
- `find_package(PkgConfig REQUIRED)`
- `pkg_check_modules(<ALIAS> REQUIRED IMPORTED_TARGET <module>)`
- `target_include_directories(... PkgConfig::<ALIAS>)`
- `target_link_libraries(... PkgConfig::<ALIAS>)`

Для clear-режима CLI генерирует `.pc` файлы сам:
- `Name`, `Version`, `Cflags: -I<prefix>/include`
- `Libs: -L<prefix>/lib -l<...>` (по найденным `lib*.so`)
- `Requires:` по резолвленному графу

Для header-only:
- `.pc` без `Libs`, только `Cflags` и `Requires`.

### 4) Упаковка RPM без Conan

В `.spec` (clear mode):
- убрать `BuildRequires: conan`
- в `%build` добавить только `PKG_CONFIG_PATH` на `vendor/aurora-conan/pkgconfig`
- в `%install` копировать `.so*` из `vendor/aurora-conan/prefix/**/lib` в `%{_datadir}/%{name}/lib`
- `%define __provides_exclude_from` оставить
- `%define __requires_exclude` вычислять из реально поставляемых `lib*.so*`

RPATH и CMake-блок оставляем как в текущей схеме.

## Детальные задачи реализации

### Task 1: Ввести модель режима проекта

**Files:**
- Create: `src/mode.rs`
- Modify: `src/main.rs`
- Modify: `src/app.rs`

Шаги:
1. Добавить enum `ProjectMode { Conan, Clear }`.
2. Добавить чтение/запись `.aurora-conan-cli-mode.json`.
3. Добавить команду `init-clear` в CLI.
4. В `run()` сделать единый роутер `add/remove` по mode.

Проверки:
- unit-tests для сериализации mode-state
- `cargo test`

### Task 2: Реализовать clear-manifest и store API

**Files:**
- Create: `src/clear_store.rs`
- Modify: `src/model.rs`
- Modify: `src/conan.rs` (переиспользование JFrog-методов)

Шаги:
1. Ввести структуры `ClearManifest`, `InstalledPackage`, `InstalledArtifact`.
2. Реализовать API:
- `load_manifest/save_manifest`
- `install_package_artifact` (скачать tgz + распаковать)
- `scan_shared_libs`
- `generate_pc_file`
3. Ввести правила выбора артефакта по arch:
- приоритет `armv8` для `aarch64`
- fallback `package` для header-only
- явная ошибка, если бинарник для целевого arch отсутствует.

Проверки:
- unit-tests на генерацию `.pc`
- unit-tests на выбор arch/`package`

### Task 3: Реализовать `init-clear`

**Files:**
- Modify: `src/app.rs`
- Modify: `src/files.rs`
- Modify: `README.md`

Шаги:
1. `init-clear` создаёт mode-state + `vendor/aurora-conan/*` + пустой `lock.json`.
2. `init-clear` патчит CMake/.spec clear-блоками (без Conan-команд).
3. `init` оставляет существующий Conan-flow без изменений.

Проверки:
- unit/integration тест: `init-clear` на фикстуре проекта

### Task 4: Реализовать `add` в clear-режиме

**Files:**
- Modify: `src/app.rs`
- Modify: `src/conan.rs`
- Modify: `src/files.rs`
- Modify: `src/clear_store.rs`

Шаги:
1. Разрешить/проверить версию пакета через JFrog (как сейчас).
2. Построить граф зависимостей без conan (`deps`-резолвер).
3. Для всех узлов графа установить артефакты в local store.
4. Перегенерировать `.pc` для всех установленных пакетов.
5. Пересчитать CMake managed block и `.spec` (`requires_exclude`).
6. Сохранить lock-manifest.

Проверки:
- integration тест на фикстуре: `init-clear -> add onnxruntime 1.18.1`
- проверка наличия `.pc`, `.so*`, и корректных managed-блоков

### Task 5: Реализовать `remove` в clear-режиме

**Files:**
- Modify: `src/app.rs`
- Modify: `src/clear_store.rs`
- Modify: `src/files.rs`

Шаги:
1. Удалить direct dependency из lock-manifest.
2. Полностью пересчитать closure по оставшимся direct deps.
3. Удалить orphan artifacts из store (или пометить и выполнить clean-pass).
4. Перегенерировать `.pc`, CMake-блоки, `.spec`.
5. Сохранить lock-manifest.

Проверки:
- integration тест: shared-transitive кейс (A->B, C->B, remove C, B остаётся)
- integration тест: remove последней зависимости => пустые managed-блоки

### Task 6: Обновить file-patching слой под dual mode

**Files:**
- Modify: `src/files.rs`
- Modify: `src/app.rs`

Шаги:
1. Разделить managed-block ключи: `conan-*` и `clear-*`.
2. Обеспечить идемпотентность переходов `init <-> init-clear`.
3. При смене режима удалять чужие managed-blocks.

Проверки:
- unit-tests для upsert/remove managed blocks
- golden tests на `.spec` и `CMakeLists.txt`

### Task 7: Документация и UX

**Files:**
- Modify: `README.md`
- Optionally create: `docs/clear-mode.md`

Шаги:
1. Описать `init-clear`, layout store, и поведение `add/remove`.
2. Добавить секцию ограничений (поддержка CMake, qmake пока не поддерживается).
3. Добавить troubleshooting по arch/proxy/network.

## Тестовая матрица

1. Unit:
- mode-state parsing
- manifest merge/recompute
- `.pc` generation
- scan shared libs

2. Integration (фикстурный проект):
- `init` + `add/remove` (регрессия текущего пути)
- `init-clear` + `add/remove`
- конфликтный режим/отсутствие mode-state

3. Real smoke (manual):
- `search onnx`
- `init-clear` в тест-проекте
- `add onnxruntime 1.18.1`
- сборка через `%cmake/%ninja_build` в SDK окружении

## Риски и контрмеры

1. Медленный/нестабильный JFrog/прокси:
- ретраи + короткие timeout + информативные ошибки
- кэш tgz в `archives/`

2. Неуниверсальность `cpp_info` без Conan:
- используем practical subset: headers + shared libs + requires
- фиксируем ограничения для редких пакетных компонентов

3. Рост репозитория из-за vendor-бинарников:
- хранить только нужный arch
- опционально в будущем поддержать внешний cache-dir

## qmake (опционально позже)

В текущий scope не включать. Возможный путь:
- генерация `.pri` на основе lock-manifest
- добавление `INCLUDEPATH`, `LIBS`, `QMAKE_RPATHDIR`
- отдельный режим patching для `.pro`

