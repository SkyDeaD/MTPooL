# Разработка

Как устроен репозиторий, как собрать и проверить изменения. Поведение пула описывает спецификация: [docs/specs/2026-11-03-mtpool-design.md](specs/2026-11-03-mtpool-design.md). Если реализация требует отступить от неё, сначала согласуйте изменение и обновите документ.

## Устройство

```
crates/core        логика без ввода-вывода: apply(state, command, now) -> effects
crates/protocol    типы API и OpenAPI
crates/registrar   HTTP, SQLite, сенсоры, DNS, webhooks, раздача UI
crates/agent       цикл ноды, адаптеры движков, antiscan, самообновление
crates/selfupdate  проверка подписи и манифеста релиза, общая у регистратора и агента
crates/fakes       заглушки внешних сервисов для E2E
crates/e2e         сквозные сценарии
crates/loadgen     нагрузочный генератор
web/               Svelte 5: публичные страницы и панель
e2e/               docker compose для сквозных и нагрузочных сценариев
deploy/            установщики, systemd, Caddy
```

## Что понадобится

- Rust нужной версии: она задана в `rust-toolchain.toml`.
- Node.js и npm для `web/`.
- `jq`: его вызывает `scripts/make-manifest.sh`, а этот скрипт проверяют тесты `mtpool-selfupdate`.
- Docker для сценариев в `e2e/`.

## Команды

Тесты всех крейтов:

```bash
cargo test --workspace
```

Линтер без предупреждений:

```bash
cargo clippy --all-targets -- -D warnings
```

Описание API. Изменили `crates/protocol` — обновите `docs/openapi.json`:

```bash
cargo run -p mtpool-registrar -- openapi > docs/openapi.json
```

Сквозные сценарии (долгие, помечены `ignore`):

```bash
cargo test -p mtpool-e2e -- --ignored
```

Веб-часть:

```bash
cd web && npm ci && npm test && npm run build && npm run check:budget
```

Браузерные проверки веб-части идут против стенда в `web/e2e/stand/`: он запускает настоящий регистратор и заглушки внешних сервисов из `crates/fakes`. Проверки сборки на CI описаны в `.github/workflows/`: `ci.yml` (в том числе `shellcheck` для `deploy/*.sh`), `e2e.yml` и `load.yml`.

Лицензии зависимостей проверяет `cargo deny` по `deny.toml`, если он у вас установлен.

## Правила

- **Секреты.** Репозиторий публичный. Токены хранятся только в `.env` (он не в git) и в секретах GitHub. Не выводите значения токенов в чат, логи и тесты. Домены и адреса реальных серверов в git не добавляйте. Каталог `tmp/` не коммитится.
- **`core` чистый.** Никакой сети, диска и системного времени. Время передаётся параметром.
- **Владелец состояния не ждёт сеть.** Внешние вызовы делаются в воркерах, результат возвращается командой. Перед применением проверяется Generation.
- **Бан только при двух и более голосах `Blocked`.** Неисправность сенсора или незнакомый ответ всегда дают `Abstain`. Быстрый флаг `blocked` у cheburcheck не голос.
- **Внешние форматы** проверяются записанными настоящими ответами в `tests/fixtures`, а не предположениями.
- **API.** Изменили `crates/protocol` — обновите `docs/openapi.json`. Ломающие изменения только через новую версию пути.
- **Интерфейс.** Все тексты на русском: общие и публичные в `web/src/strings.ru.ts`, панель в `web/src/strings.panel.ru.ts`. Никаких внешних CDN. Бюджет публичной страницы — 50 КБ JS в сжатом виде (`npm run check:budget`). Цвета и размеры — в `web/src/styles/tokens.css`.
- **Тесты вперёд.** Сначала падающий тест, затем код.
- **Коммиты** в стиле Conventional Commits: `feat(dns): ...`, `fix(web): ...`, `docs: ...`.

## Документация

Тексты для пользователей лежат в `README.md`, `README.en.md` и `docs/*.md`. Ссылки проверяются так:

```bash
lychee README.md README.en.md docs/
```

Схемы лежат в `docs/images/diagram-*.svg`, светлая и тёмная версии рисуются одним скриптом. Поправьте его и запустите:

```bash
python3 scripts/draw-diagrams.py
```

Команды и настройки в документах сверяйте с кодом: значения по умолчанию — `crates/core/src/settings.rs`, маршруты — `docs/openapi.json`, параметры установщиков — `deploy/`.
