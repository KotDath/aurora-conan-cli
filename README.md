# aurora-conan-cli

CLI для управления Conan-зависимостями в CMake Qt-проектах под ОС Аврора.

## Команды

- `aurora-conan-cli connect [--mode sdk|psdk] [--dir <path>]`
- `aurora-conan-cli disconnect`
- `aurora-conan-cli init`
- `aurora-conan-cli init-clear`
- `aurora-conan-cli add <dependency> [version]`
- `aurora-conan-cli remove <dependency>`
- `aurora-conan-cli search <dependency>`
- `aurora-conan-cli download <dependency> <version>`
- `aurora-conan-cli deps <dependency> <version>`

## Ожидаемая структура проекта

CLI должен запускаться из корня Qt-проекта и использует фиксированные пути:

- `CMakeLists.txt`
- `conanfile.py`
- `rpm/*.spec` (ожидается ровно один `.spec` файл)
- `~/.config/aurora-conan-cli/connection.json` (глобальное состояние `connect`)

## Что делает CLI

- `init`:
  - создаёт `conanfile.py` с пустым `requires` (декларативный список зависимостей)
  - добавляет CMake/.spec интеграцию по шаблону Conan-режима (как в OnnxRunner)
- `init-clear`:
  - создаёт `thirdparty/aurora/manifest.lock.json`
  - удаляет `conanfile.py`, если он был ранее создан
  - добавляет CMake/.spec интеграцию clear-режима (без `BuildRequires: conan`)
- `add`:
  - в режиме `init`: добавляет/обновляет зависимость в `conanfile.py`
  - в режиме `init-clear`: сохраняет зависимость в `thirdparty/aurora/manifest.lock.json`,
    загружает архивы в `thirdparty/aurora/<arch>/packages/...`, генерирует `.pc` в `thirdparty/aurora/<arch>/pkgconfig`
    (архитектура определяется автоматически: `AURORA_CONAN_ARCH`/`RPM_ARCH`, иначе готовятся `armv7`, `armv8`, `x86_64`)
  - в обоих режимах пересчитывает блоки `pkg_check_modules`, `target_include_directories`, `target_link_libraries`
    и `%define __requires_exclude`
- `remove`:
  - удаляет зависимость из соответствующего источника (conanfile/manifest)
  - пересобирает CMake/.spec и локальный clear-store для выбранной архитектуры
- `search`:
  - получает список пакетов из JFrog (`https://conan.omp.ru`)
  - фильтрует пакеты по подстроке из `<dependency>`
  - получает версии для каждого совпавшего пакета из Artifactory API
  - возвращает строки вида `<package>/<version>@aurora`
- `download`:
  - получает данные пакета из JFrog/Artifactory API
  - скачивает все доступные архивы этой версии (например, `armv7`, `armv8`, `x86_64`, `package`)
  - сохраняет в `./downloads/<dependency>/<version>/`
- `deps`:
  - получает зависимости пакета без использования `conan`
  - использует данные JFrog (`conaninfo.txt`, `conanfile.py`) и Artifactory API
  - поддерживает версии `exact`, шаблоны `*.Z`, а также семейство `cci`
  - если не удалось определить версию пакета, возвращает строку `<package>/error@aurora`
  - выводит итоговый список строками `<package>/<version>@aurora`

## Connect / Disconnect

- `connect`:
  - интерактивно выбирает `psdk` или `sdk`
  - принимает путь через `--dir` (или запрашивает интерактивно)
  - валидирует путь:
    - `sdk`: должен существовать `<dir>/vmshare/ssh/private_keys/sdk`
    - `psdk`: должен существовать `<dir>/sdk-chroot`
  - сохраняет состояние в `~/.config/aurora-conan-cli/connection.json`
- `disconnect`:
  - удаляет сохранённое состояние

## Важное ограничение

CLI не запускает `conan` напрямую. Все операции `search/download/deps/add/remove`
разрешают версии и зависимости через JFrog API.

В режиме `init` пакетирование в `.spec` остаётся совместимым со сценарием,
где в SDK доступен `conan-install-if-modified`/`conan-deploy-libraries`.
