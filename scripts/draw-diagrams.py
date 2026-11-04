#!/usr/bin/env python3
"""Draws the documentation diagrams into docs/images as SVG, one file per language and theme.

Run it from anywhere after changing a diagram: python3 scripts/draw-diagrams.py
Colours follow web/src/styles/tokens.css.
"""

from pathlib import Path
from xml.sax.saxutils import escape

OUT = Path(__file__).resolve().parent.parent / "docs" / "images"
FONT = "-apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, 'Helvetica Neue', Arial, sans-serif"

THEMES = {
    "light": {
        "bg": "#ffffff", "edge": "#d5d9e0", "box": "#f6f7f9", "group": "#f2f4f7",
        "fg": "#12161f", "fg2": "#475062", "line": "#616a7b",
        "accent": "#0b63e5", "accent_t": "#e7f0fd",
        "ok": "#09774f", "ok_t": "#e7f3ef", "warn": "#8a5600", "warn_t": "#f8f1e0",
        "bad": "#c52f3d", "bad_t": "#faebec",
    },
    "dark": {
        "bg": "#14171e", "edge": "#2c313c", "box": "#1a1e27", "group": "#191c24",
        "fg": "#eceef3", "fg2": "#aab1bf", "line": "#868e9d",
        "accent": "#5aa5ff", "accent_t": "#1a2940",
        "ok": "#43d294", "ok_t": "#17302a", "warn": "#f2b84b", "warn_t": "#2f2a1d",
        "bad": "#ff808a", "bad_t": "#35222a",
    },
}


class Canvas:
    def __init__(self, width, height, theme, title):
        self.t = THEMES[theme]
        self.width, self.height, self.title = width, height, title
        self.lines, self.boxes, self.texts = [], [], []

    def text(self, x, y, value, size=18, color="fg2", anchor="middle", weight=400):
        self.texts.append(
            f'<text x="{x}" y="{y}" font-size="{size}" font-weight="{weight}" '
            f'fill="{self.t[color]}" text-anchor="{anchor}">{escape(value)}</text>'
        )

    def label(self, x, y, lines, anchor="middle"):
        """Words for an arrow: they sit beside the line and never on it."""
        for i, line in enumerate([lines] if isinstance(lines, str) else lines):
            self.text(x, y + i * 24, line, anchor=anchor)

    def box(self, x, y, w, h, title, sub=None, tone=None):
        stroke = self.t[tone] if tone else self.t["line"]
        fill = self.t[f"{tone}_t"] if tone else self.t["box"]
        self.boxes.append(
            f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="10" fill="{fill}" '
            f'stroke="{stroke}" stroke-width="2"/>'
        )
        middle = x + w / 2
        if sub:
            self.text(middle, y + h / 2 - 5, title, size=24, color="fg", weight=600)
            self.text(middle, y + h / 2 + 23, sub, size=17)
        else:
            self.text(middle, y + h / 2 + 8, title, size=24, color="fg", weight=600)

    def dot(self, x, y):
        self.boxes.append(f'<circle cx="{x}" cy="{y}" r="6" fill="{self.t["line"]}"/>')

    def arrow(self, points, both=False):
        start = ' marker-start="url(#head)"' if both else ""
        self.lines.append(
            f'<path d="{rounded(points)}" fill="none" stroke="{self.t["line"]}" stroke-width="2" '
            f'marker-end="url(#head)"{start}/>'
        )

    def svg(self):
        parts = [
            f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {self.width} {self.height}" '
            f'width="{self.width}" height="{self.height}" role="img" font-family="{FONT}">',
            f"<title>{escape(self.title)}</title>",
            '<defs><marker id="head" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" '
            'markerHeight="7" orient="auto-start-reverse">'
            f'<path d="M0 0.8 L9 5 L0 9.2 Z" fill="{self.t["line"]}"/></marker></defs>',
            f'<rect x="0.75" y="0.75" width="{self.width - 1.5}" height="{self.height - 1.5}" rx="12" '
            f'fill="{self.t["bg"]}" stroke="{self.t["edge"]}" stroke-width="1.5"/>',
            *self.lines, *self.boxes, *self.texts, "</svg>\n",
        ]
        return "\n".join(parts)


def rounded(points, radius=10):
    """A path through the points with the corners rounded."""
    path = [f"M{points[0][0]} {points[0][1]}"]
    for before, corner, after in zip(points, points[1:], points[2:]):
        path.append(f"L{toward(corner, before, radius)}")
        path.append(f"Q{corner[0]} {corner[1]} {toward(corner, after, radius)}")
    path.append(f"L{points[-1][0]} {points[-1][1]}")
    return " ".join(path)


def toward(start, end, distance):
    length = abs(end[0] - start[0]) + abs(end[1] - start[1])
    share = min(distance / length, 0.5)
    return f"{start[0] + (end[0] - start[0]) * share:g} {start[1] + (end[1] - start[1]) * share:g}"


POOL_WORDS = {
    "ru": {
        "title": "Как устроен пул", "client": "Клиент Telegram", "dns": "Cloudflare DNS",
        "ask": "1. узнаёт адрес мастера", "records": "записи A и CNAME",
        "connect": "2. MTProto к мастеру",
        "nodes": "Ноды", "nodes_sub": "мастера и очередь", "reports": "отчёты",
        "settings": "настройки", "registrar": "Регистратор", "check": "проверка IP",
        "sensors": "Сенсоры", "sensors_sub": "бан от 2 голосов Blocked",
        "visitor": "Посетитель", "visitor_sub": "публичные страницы",
        "operator": "Оператор", "operator_sub": "панель",
        "bot": "Бот", "bot_sub": "API и webhooks",
    },
    "en": {
        "title": "How the pool is built", "client": "Telegram client", "dns": "Cloudflare DNS",
        "ask": "1. finds the master's address", "records": "A and CNAME records",
        "connect": "2. MTProto to the master",
        "nodes": "Nodes", "nodes_sub": "masters and queue", "reports": "reports",
        "settings": "settings", "registrar": "Registrar", "check": "IP check",
        "sensors": "Sensors", "sensors_sub": "ban on 2 Blocked votes",
        "visitor": "Visitor", "visitor_sub": "public pages",
        "operator": "Operator", "operator_sub": "panel",
        "bot": "Bot", "bot_sub": "API and webhooks",
    },
}


def pool(theme, lang):
    w = POOL_WORDS[lang]
    c = Canvas(960, 590, theme, w["title"])
    c.box(30, 28, 230, 90, w["client"])
    c.box(700, 28, 230, 90, w["dns"])
    c.arrow([(260, 58), (700, 58)])
    c.label(480, 46, w["ask"])
    c.arrow([(145, 118), (145, 250)])
    c.label(160, 190, w["connect"], anchor="start")
    c.box(30, 250, 230, 110, w["nodes"], w["nodes_sub"])
    c.box(380, 250, 200, 110, w["registrar"], "mtpool-registrar", tone="accent")
    c.box(700, 250, 230, 110, w["sensors"], w["sensors_sub"])
    c.arrow([(260, 282), (380, 282)])
    c.label(320, 270, w["reports"])
    c.arrow([(380, 328), (260, 328)])
    c.label(320, 316, w["settings"])
    c.arrow([(580, 305), (700, 305)])
    c.label(640, 293, w["check"])
    c.arrow([(480, 250), (480, 96), (700, 96)])
    c.label(590, 86, w["records"])
    c.box(100, 470, 240, 90, w["visitor"], w["visitor_sub"])
    c.box(360, 470, 240, 90, w["operator"], w["operator_sub"])
    c.box(620, 470, 240, 90, w["bot"], w["bot_sub"])
    c.arrow([(220, 470), (220, 415), (435, 415), (435, 360)])
    c.arrow([(480, 470), (480, 360)])
    c.arrow([(740, 470), (740, 415), (525, 415), (525, 360)], both=True)
    return c.svg()


def lifecycle(theme):
    # Only the main path: the page lists every transition in a table.
    c = Canvas(960, 420, theme, "Основной путь ноды")
    c.box(30, 40, 170, 90, "Pending", "ждёт отчёта")
    c.box(395, 40, 170, 90, "Healthy", "в работе", tone="ok")
    c.box(760, 40, 170, 90, "Quarantine", "идёт проверка", tone="warn")
    c.arrow([(200, 85), (395, 85)])
    c.label(297, 73, "условия верны")
    c.arrow([(565, 68), (760, 68)])
    c.label(662, 56, "условие нарушено")
    c.arrow([(760, 104), (565, 104)])
    c.label(662, 132, "снова в норме")
    # A node leaves quarantine down to one of three outcomes.
    c.box(30, 300, 170, 90, "Banned", "запись удалена", tone="bad")
    c.box(395, 300, 170, 90, "Dead", "не отвечает")
    c.box(760, 300, 170, 90, "Pruned", "нет решения")
    c.arrow([(845, 130), (845, 300)])
    c.arrow([(845, 205), (480, 205), (480, 300)])
    c.arrow([(845, 205), (150, 205), (150, 300)])
    c.label(164, 240, ["не меньше 2", "голосов Blocked"], anchor="start")
    c.label(494, 240, ["не жива", "10 минут"], anchor="start")
    c.label(831, 240, ["нет решения", "60 минут"], anchor="end")
    c.arrow([(70, 300), (70, 130)])
    c.label(84, 162, ["регистрация после", "reverify или разбана"], anchor="start")
    return c.svg()


def update(theme):
    c = Canvas(960, 690, theme, "Как обновляется регистратор")
    x, w, middle = 40, 880, 480
    c.box(x, 28, w, 76, "GitHub Releases")
    c.arrow([(middle, 104), (middle, 164)])
    c.label(middle + 16, 141, "опрос раз в 5 минут", anchor="start")
    c.box(x, 164, w, 92, "Регистратор", "проверяет подпись и кладёт релиз в /var/lib/mtpool/releases", tone="accent")
    c.arrow([(middle, 256), (middle, 316)])
    c.label(middle + 16, 293, "manifest.json изменился", anchor="start")
    c.box(x, 316, w, 76, "mtpool-registrar-update.path")
    c.arrow([(middle, 392), (middle, 436)])
    c.box(x, 436, w, 92, "mtpool-registrar-update.service",
          "от root: подпись, суммы, смена бинарника, перезапуск, 60 с наблюдения за /readyz")
    c.arrow([(middle, 528), (middle, 588)])
    c.label(middle + 16, 565, "не поднялся", anchor="start")
    c.box(x, 588, w, 76, "mtpool-registrar-rollback.service", tone="warn")
    return c.svg()


def main():
    for theme in THEMES:
        drawings = {
            f"diagram-pool-{theme}.svg": pool(theme, "ru"),
            f"diagram-pool-en-{theme}.svg": pool(theme, "en"),
            f"diagram-lifecycle-{theme}.svg": lifecycle(theme),
            f"diagram-update-{theme}.svg": update(theme),
        }
        for name, content in drawings.items():
            (OUT / name).write_text(content, encoding="utf-8")


if __name__ == "__main__":
    main()
