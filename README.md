# aurora-conan-cli

CLI для управления Conan-зависимостями в CMake Qt-проектах под ОС Аврора.

## Команды

- `aurora-conan-cli connect [--mode sdk|psdk] [--dir <path>]`
- `aurora-conan-cli disconnect`
- `aurora-conan-cli init`
- `aurora-conan-cli add <dependency> [version]`
- `aurora-conan-cli remove <dependency>`

## Ожидаемая структура проекта

CLI должен запускаться из корня Qt-проекта и использует фиксированные пути:

- `CMakeLists.txt`
- `conanfile.py`
- `rpm/*.spec` (ожидается ровно один `.spec` файл)
- `~/.config/aurora-conan-cli/connection.json` (глобальное состояние `connect`)

## Что делает CLI

- `init`:
  - создаёт `conanfile.py` с пустым `requires`
  - добавляет Conan-интеграцию в `CMakeLists.txt`
  - добавляет Conan-интеграцию в `.spec`
- `add`:
  - добавляет/обновляет зависимость в `conanfile.py`
  - пересчитывает блоки `pkg_check_modules`, `target_include_directories`, `target_link_libraries`
  - пересчитывает `%define __requires_exclude` по полному графу зависимостей
- `remove`:
  - удаляет зависимость из `conanfile.py`
  - пересчитывает CMake/.spec по оставшимся зависимостям

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

## Conan-интеграция

Для работы `add/remove` требуется доступный исполняемый файл Conan:

- перед каждым Conan-вызовом:
  - читается connect-state
  - выполняется `sdk-assistant list` внутри выбранного окружения
  - берётся первый target по шаблону `AuroraOS-*-aarch64`
  - затем выполняется `sb2 -t <target> <conan-exec> ...`
- запуск команд внутри окружения:
  - `sdk`: `ssh -p 2232 -i <sdk-dir>/vmshare/ssh/private_keys/sdk mersdk@localhost`
  - `psdk`: `<psdk-dir>/sdk-chroot <command>`
- `conan-exec` выбирается как:
  - `conan-with-aurora-profile`, если доступен
  - иначе `conan`

Для определения версии команда `add` использует страницу
`https://developer.auroraos.ru/conan/<dependency>`:

- если страница пакета не найдена (`404`) — команда завершится с ошибкой
- если версия не задана — берётся первая версия из `select` в блоке `Версия`
- если версия задана, но отсутствует в списке — команда завершится с ошибкой
- если версия задана и найдена — используется указанная версия

Для расчёта списка `.so` шаблонов используется `conan graph info ... --format json`.
