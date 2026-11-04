# MTPooL

[English](README.en.md)

MTPooL — пул MTProto-прокси. Серверы регистрируются в пуле по токену. Пул следит за их здоровьем, выбирает мастеров, ведёт DNS в Cloudflare и банит IP только после подтверждения независимыми сенсорами.

Благодарности автору исходной идеи — в файле [NOTICE](NOTICE).

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

## Быстрый старт

Нужны: сервер с Ubuntu 24.04 для регистратора, имя вроде `pool.example.com`, указывающее на этот сервер (запись создаётся вручную), и зона в Cloudflare для доменов пула.

1. Склонируйте репозиторий и установите регистратор из каталога `deploy/` (параметры — в [install-registrar.md](docs/install-registrar.md)). Скрипт выдаст админ-токен один раз.

   ```bash
   sudo sh deploy/install-registrar.sh --domain pool.example.com --pubkey <ключ>
   ```

2. Откройте `https://pool.example.com/panel/`, войдите админ-токеном, задайте домены пула и ключи сервисов.
3. Выпустите токен регистрации для владельца и подключите ноду:

   ```bash
   curl -fsSL https://pool.example.com/install.sh | MTPOOL_TOKEN=<токен> sh -s -- --name node1
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
