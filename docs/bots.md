# Руководство для ботов

Страница для тех, кто пишет бота или переносит бота со старого пула: что вызывать, как получать события и как проверять подпись. Полный список маршрутов и полей — в [openapi.json](openapi.json); его же отдаёт регистратор по адресу `/api/v1/openapi.json`.

## Основы API

- Адрес: `https://pool.example.com/api/v1/...`. JSON, время в RFC 3339 UTC.
- Ошибки приходят как `application/problem+json` с постоянным полем `code`.
- Публичные ответы имеют `ETag` и отвечают 304 на повторный запрос с `If-None-Match`. Лимит частоты по IP: при превышении 429 и заголовок `Retry-After`.
- Публичные маршруты работают без токена. Адреса нод в них скрыты: `203.x.x.113`. У забаненной ноды записи нет: `GET /nodes` и `GET /me/nodes` её не показывают, `GET /nodes/{id}` отвечает 404, сведения о бане в `GET /bans`.
- Маршруты `/me` требуют токен бота: заголовок `Authorization: Bearer <токен>`. Токен выпускает администратор (`kind: "bot"`, владелец обязателен). Токен показывается один раз.

## Что вызывать

| Задача | Запрос |
|---|---|
| список нод, состояние, голоса последнего раунда | `GET /api/v1/nodes` |
| одна нода | `GET /api/v1/nodes/{id}` |
| доступность и клиенты ноды за сутки | `GET /api/v1/nodes/{id}/history` |
| домены, мастера, клиенты | `GET /api/v1/domains` |
| прокси-ссылки и QR-код | `GET /api/v1/links`, `GET /api/v1/links/{id}/qr.svg` |
| история банов | `GET /api/v1/bans`; с `active=true` только действующие |
| KPI и ряды | `GET /api/v1/stats/summary`, `GET /api/v1/stats/series` |
| свой профиль | `GET /api/v1/me` |
| свои ноды с полным IP | `GET /api/v1/me/nodes` |
| вывести свою ноду | `POST /api/v1/me/nodes/{id}/retire` |
| внеочередной раунд сенсоров | `POST /api/v1/me/nodes/{id}/recheck` (лимит: при превышении 429 `rate_limited`; 409 `not_recheckable`, если проверять сейчас нечего) |

Поля `clients` у нод и доменов, `traffic_*` в `/stats/*` и `/nodes/{id}/history` считают только пользователей пула. Пользователи владельца ноды идут отдельно: `other_clients` и `other_traffic_bytes` у ноды, `other_traffic_*` в `/stats/*`. Если этих полей нет, показывать нечего: их либо ноль, либо агент ноды их ещё не различает. В точках истории, записанных до обновления агента, осталась сумма по всем пользователям движка.

Состояние ноды в поле `state`: `pending`, `healthy`, `quarantine`, `dead`, `pruned`. Значение `banned` осталось в схеме API, но запись забаненной ноды удаляется сразу. Записи `dead` и `pruned` удаляются через 24 часа. Ноду в карантине сопровождает `quarantine_reasons`: `tcp`, `metrics`, `globalping`, `port`, `heartbeat_stale`, `report_stale` (нет подтверждённого замера дольше `report_max_age_sec`), `pool_user_taken` (у владельца ноды есть пользователь с именем из настроек пула и другим секретом). Поле `last_round` содержит вердикт (`confirmed`, `clean`, `undecided`) и голоса сенсоров (`blocked`, `clear`, `abstain` с причиной). Готовый вердикт и голоса не нужно пересчитывать на стороне бота.

Пример: свои ноды.

```bash
curl -s https://pool.example.com/api/v1/me/nodes \
  -H "Authorization: Bearer $MTPOOL_BOT_TOKEN" | jq '.[] | {id, state, ip, port}'
```

## Заменить сервер

1. Снимите старую ноду: `POST /api/v1/me/nodes/{id}/retire`. Её место сразу свободно.
2. Установите агент на новом сервере с токеном регистрации. Он зарегистрируется сам.

```bash
curl -s -X POST https://pool.example.com/api/v1/me/nodes/alpha-a1b2c/retire \
  -H "Authorization: Bearer $MTPOOL_BOT_TOKEN"
```

Без бота тоже можно. Если лимит нод занят, молчащая нода вытесняется автоматически: новая нода входит, когда старая молчит 60 секунд (`heartbeat_ttl_sec`). До этого новый сервер получает `403 owner_limit`, а его агент повторяет попытку с растущей паузой, не больше 5 минут. Если сервер остался на прежнем IP, старая нода уступает ему место сразу.

## События (webhooks)

Вместо опроса подпишитесь на события. События:

| Событие | Когда |
|---|---|
| `node.registered` | нода зарегистрирована |
| `node.quarantined` | нода ушла в карантин |
| `node.recovered` | нода вернулась из карантина |
| `node.banned` | нода забанена; её запись уже удалена, причины смотрите в `GET /api/v1/bans` |
| `node.dead` | нода признана мёртвой, снята или вытеснена; `detail` может содержать `replaced_by` и `reason` (`silent`: уступила место, молчала дольше всех) |
| `node.pruned` | нода убрана без решения |
| `ban.lifted` | бан снят |
| `master.changed` | сменился мастер домена |

Токен бота получает события своих нод. Подписку на весь пул (`scope: "pool"`) может создать только токен с областью `pool:read`; администратор указывает её при выпуске токена (`"scope": "pool:read"`).

### Создать подписку

```bash
curl -s -X POST https://pool.example.com/api/v1/me/webhooks \
  -H "Authorization: Bearer $MTPOOL_BOT_TOKEN" -H 'Content-Type: application/json' \
  -d '{"url":"https://hooks.example.com/mtpool","events":["node.banned","node.recovered"]}'
```

- `url`: только `https`, не длиннее 2048 символов, имя хоста (не IP-адрес), которое разрешается только в публичные адреса. Адрес проверяется при создании и снова при каждой доставке; редиректы не выполняются.
- `events`: из таблицы выше. Неизвестное имя отклоняется.
- `secret`: необязательно, от 16 до 256 символов без управляющих. Если не задан, пул создаёт его сам. **Секрет показывается только в ответе на создание.**
- `scope`: `owner` (по умолчанию) или `pool`.

Проверка: `POST /api/v1/me/webhooks/{id}/test` ставит в очередь событие `webhook.test`. Журнал доставок: `GET /api/v1/me/webhooks/{id}/deliveries` — статус (`pending`, `delivered`, `failed`), номер попытки, код ответа получателя, причина последней ошибки и время следующей попытки. Список подписок: `GET /api/v1/me/webhooks`; удаление: `DELETE /api/v1/me/webhooks/{id}`.

### Что приходит

`POST` на ваш URL с телом JSON одной формы для всех событий:

```json
{
  "id": "evt_0011223344556677",
  "event": "node.banned",
  "at": "2026-11-04T10:00:00Z",
  "node": "alpha-a1b2c",
  "domain": null,
  "detail": {}
}
```

`node` и `domain` могут быть `null`, если событие их не касается. `detail` — объект с подробностями события.

Заголовки:

| Заголовок | Значение |
|---|---|
| `X-MTPooL-Event` | имя события |
| `X-MTPooL-Delivery` | номер доставки |
| `X-MTPooL-Signature` | `t=<unix-время>,v1=<hex>` |

### Подпись

`v1` — это HMAC-SHA256 от строки `<t>.<тело запроса>` с ключом-секретом подписки, в шестнадцатеричной записи. Считайте по сырым байтам тела, до разбора JSON. Сравнивайте в постоянное время и отвергайте подпись, если `t` слишком далёк от текущего времени (в примере 5 минут): так старый перехваченный запрос нельзя воспроизвести.

### Доставка и повторы

- Успех — любой код 2xx. Таймаут запроса 10 секунд.
- При ошибке доставка повторяется с тем же телом и тем же `id`: через 1 минуту, 5 минут, 30 минут, 2 и 6 часов. После шестой неудачи доставка получает статус `failed`.
- Подпись пересчитывается при каждой попытке, а `id` остаётся прежним. Поэтому получатель отсекает повторы по `id`.
- Отвечайте быстро, тяжёлую работу делайте после ответа.

### Пример получателя

Python без внешних зависимостей. Проверяет подпись и возраст, пропускает повторы и печатает событие. Запускайте за HTTPS-прокси: пул доставляет только на `https`.

```python
import hashlib
import hmac
import json
import os
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

SECRET = os.environ["MTPOOL_WEBHOOK_SECRET"].encode()
TOLERANCE_SEC = 300
seen = set()  # в боевой версии храните id в базе


def signature_ok(header: str, body: bytes) -> bool:
    try:
        parts = dict(item.split("=", 1) for item in header.split(","))
        stamp = int(parts["t"])
        given = parts["v1"]
    except (KeyError, ValueError):
        return False
    if abs(time.time() - stamp) > TOLERANCE_SEC:
        return False
    signed = str(stamp).encode() + b"." + body
    expected = hmac.new(SECRET, signed, hashlib.sha256).hexdigest()
    return hmac.compare_digest(expected, given)


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        if not signature_ok(self.headers.get("X-MTPooL-Signature", ""), body):
            self.send_response(401)
            self.end_headers()
            return
        event = json.loads(body)
        if event["id"] not in seen:
            seen.add(event["id"])
            print(event["event"], event["node"], event["domain"], event["at"], flush=True)
        self.send_response(200)
        self.end_headers()


HTTPServer(("127.0.0.1", 8099), Handler).serve_forever()
```

Запуск: `MTPOOL_WEBHOOK_SECRET=<секрет> python3 receiver.py`. Пример проверен вручную: запрос с верной подписью получил 200, повтор того же `id` не напечатан повторно, неверная подпись и подпись старше 5 минут получили 401. Подпись для проверки считается так:

```bash
printf '%s.%s' "$TS" "$BODY" | openssl dgst -sha256 -hmac "$SECRET" -hex
```

## Перенос бота со старого пула

| Было | Стало |
|---|---|
| `GET /statistics/api/list` | `GET /api/v1/nodes` |
| `GET /statistics/api/node?id=` | `GET /api/v1/nodes/{id}` |
| `GET /proxylinks` | `GET /api/v1/links` |
| `GET /dashboard/api` | `GET /api/v1/stats/summary`, `GET /api/v1/stats/series` |
| `healthy`, `in_quarantine`, `globalping_ok` | `state`: `pending`, `healthy`, `quarantine`, `dead`, `pruned` |
| `ip` с маской, поиск по октетам | `GET /api/v1/me/nodes` с полным IP по токену бота |
| опрос раз в 45 секунд | webhook `node.banned` и `node.recovered` |
| 2 из 3 на стороне бота | готовый вердикт и голоса в `last_round` |
| `POST /register`, `install_node_web.sh` | `POST /api/v1/agent/enroll`, `install.sh` |
| юнит `sharedd-node-agent` | юнит `mtpool-agent` |

Порядок переноса:

1. Попросите администратора создать владельца и выпустить токен бота (`kind: "bot"`) и токен регистрации для нод.
2. Замените запросы по таблице. Поле `healthy` и ему подобные замените проверкой `state == "healthy"`.
3. Уберите собственное голосование «2 из 3»: пул присылает вердикт.
4. Вместо опроса раз в 45 секунд создайте подписку на `node.banned` и `node.recovered`. Опрос `GET /api/v1/me/nodes` оставьте как редкую сверку.
5. Ноды переустановите установщиком `install.sh` (см. [add-node.md](add-node.md)): старый агент и его юнит не совместимы с новым пулом. Старый юнит `sharedd-node-agent` на ноде остановите и отключите перед установкой `mtpool-agent`.

Поиск нод по октетам IP теперь делается по полным адресам в `GET /api/v1/me/nodes`: публичный API адреса скрывает. Подбор ротации нод (proxy-rotator) в MTPooL не входит.
