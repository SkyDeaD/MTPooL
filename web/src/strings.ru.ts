// Every text of the interface lives here, in Russian. Pages read it through `t(key)` from
// `lib/i18n.ts`; another language is another file with the same keys.

export const messages = {
  'app.name': 'MTPooL',
  'app.tagline': 'Пул MTProto-прокси',

  'common.loading': 'Загрузка…',
  'common.retry': 'Повторить',
  'common.save': 'Сохранить',
  'common.cancel': 'Отмена',
  'common.yes': 'Да',
  'common.no': 'Нет',
  'common.none': 'Нет данных',

  'error.network': 'Нет связи с сервером',
  'error.server': 'Сервер ответил ошибкой {status}',
  'error.unauthorized': 'Нужно войти',

  'theme.light': 'Светлая',
  'theme.dark': 'Тёмная',
  'theme.system': 'Как в системе',

  'public.stub.title': 'Статистика пула',
  'public.stub.text': 'Страница собирается. Здесь будут состояние пула, график клиентов и таблица нод.',
  'public.stub.status': 'Сервис работает',

  'panel.stub.title': 'Панель управления',
  'panel.stub.text': 'Вход и разделы панели появятся здесь.',
} as const;

export type MessageKey = keyof typeof messages;

// Words that change with the number, as [one, few, many]: 1 день, 2 дня, 5 дней.
export const plurals = {
  'unit.day': ['день', 'дня', 'дней'],
  'unit.hour': ['час', 'часа', 'часов'],
  'unit.minute': ['минута', 'минуты', 'минут'],
  'unit.second': ['секунда', 'секунды', 'секунд'],
} as const;

export type PluralKey = keyof typeof plurals;

// Short names of the byte units, in order from bytes up.
export const byteUnits = ['Б', 'КБ', 'МБ', 'ГБ', 'ТБ'] as const;
