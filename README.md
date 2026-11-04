# MTPooL

[English](README.en.md)

MTPooL — пул MTProto-прокси. Серверы регистрируются в пуле по токену. Пул следит за их здоровьем, выбирает мастеров, ведёт DNS в Cloudflare и банит IP только после подтверждения независимыми сенсорами.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/panel-dark.png">
  <img alt="Панель пула: состояние нод, график клиентов, домены и их мастера" src="docs/images/panel-light.png">
</picture>

Благодарности автору исходной идеи — в файле [NOTICE](NOTICE).

## Что умеет

- **Подключение по токену.** Нода входит в пул одной командой, движок telemt, MTProxyL или MEKO определяется сам. Ваша конфигурация дополняется, а не перезаписывается.
- **Мастера и ротация.** Пул сам выбирает, на какие ноды указывают домены, и меняет их по очереди. Упавшего мастера заменяет следующая нода.
- **DNS в Cloudflare.** Записи пула помечены комментарием `MTPooL`, остальные записи зоны он не трогает. При росте очереди может сам добавлять домены.
- **Бан только по подтверждению.** IP блокируется, когда блокировку видят минимум два независимых сенсора из трёх. Неисправный сенсор не голосует.
- **Публичные страницы.** Статистика, прокси-ссылки с QR-кодами и описание API для всех, без входа. Адреса нод скрыты частично.
- **Панель.** Ноды, домены, журнал, настройки, владельцы и токены, обновления. На русском, в светлой и тёмной теме.
- **Боты и мониторинг.** REST API со спецификацией OpenAPI, webhooks с подписью HMAC, метрики Prometheus.
- **Подписанные обновления.** Регистратор и агенты сами ставят релизы, подпись проверяется `minisign`, при сбое версия откатывается.

## Как это работает

```
 нода (агент) ──heartbeat, отчёты──►  регистратор  ──A/CNAME──►  Cloudflare
 нода (агент) ──heartbeat, отчёты──►      │
                                          ├──► Globalping   ┐
                                          ├──► Cheburcheck  ├─ голоса сенсоров
                                          └──► RedFlag      ┘
 посетитель ◄── публичные страницы и API ─┤
 оператор   ◄── панель /panel/ и API ─────┤
 бот        ◄── API /api/v1/me и webhooks ┘
```

- **Нода** — сервер с MTProto-движком (telemt, MTProxyL или MEKO) и агентом `mtpool-agent`. Агент сообщает о здоровье и применяет настройки пула.
- **Регистратор** — один сервис `mtpool-registrar`: HTTP API, база SQLite, сенсоры, DNS, webhooks, публичные страницы и панель.
- **Мастер** — нода, на которую сейчас указывает DNS-запись домена. Пул выбирает их сам и по умолчанию меняет раз в 30 минут.
- **Бан** — только когда не меньше двух сенсоров проголосовали `Blocked`. Неисправность сенсора и незнакомый ответ дают `Abstain`, то есть не голос.

Подробнее: [жизненный цикл ноды и логика бана](docs/lifecycle.md), [сенсоры](docs/sensors.md).

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/public-dark.png">
  <img alt="Публичная страница: сводка пула, график клиентов, домены и мастера" src="docs/images/public-light.png">
</picture>

## Быстрый старт

Нужны: сервер с Ubuntu 24.04 для регистратора, имя вроде `pool.example.com`, указывающее на этот сервер (запись создаётся вручную), и зона в Cloudflare для доменов пула.

1. Склонируйте репозиторий и установите регистратор из каталога `deploy/` (параметры — в [install-registrar.md](docs/install-registrar.md)). Скрипт выдаст админ-токен один раз.

   ```bash
   sudo sh deploy/install-registrar.sh --domain pool.example.com
   ```

2. Откройте `https://pool.example.com/panel/`, войдите админ-токеном, задайте домены пула и ключи сервисов.
3. Выпустите токен регистрации для владельца и подключите ноду:

   ```bash
   curl -fsSL https://pool.example.com/install.sh | sudo env MTPOOL_TOKEN=<токен> sh -s -- --name node1
   ```

Пошагово: [установка регистратора](docs/install-registrar.md), [подключение ноды](docs/add-node.md).

## Документация

| Документ | О чём |
|---|---|
| [docs/install-registrar.md](docs/install-registrar.md) | установка регистратора, Cloudflare, ключи сервисов |
| [docs/add-node.md](docs/add-node.md) | подключение ноды, `install.sh`, `agent.toml` |
| [docs/sensors.md](docs/sensors.md) | Globalping, Cheburcheck, RedFlag и их лимиты |
| [docs/lifecycle.md](docs/lifecycle.md) | состояния ноды, бан, reverify, мастера, DNS, СРМД |
| [docs/bots.md](docs/bots.md) | API для ботов, webhooks, перенос со старого пула |
| [docs/operations.md](docs/operations.md) | обновления, откат, резервные копии, диагностика |
| [docs/development.md](docs/development.md) | сборка, тесты, устройство репозитория |
| [docs/openapi.json](docs/openapi.json) | спецификация API |

## Лицензия

Текст лицензии — в файле [LICENSE](LICENSE), сторонние компоненты — в [NOTICE](NOTICE).
