import { describe, expect, it } from 'vitest';
import { formatBytes, formatDuration } from '../src/lib/format';
import { plural } from '../src/lib/i18n';

const NB = ' ';

describe('plural', () => {
  const forms = ['день', 'дня', 'дней'] as const;
  it.each([
    [0, 'дней'],
    [1, 'день'],
    [2, 'дня'],
    [4, 'дня'],
    [5, 'дней'],
    [11, 'дней'],
    [12, 'дней'],
    [14, 'дней'],
    [21, 'день'],
    [22, 'дня'],
    [25, 'дней'],
    [101, 'день'],
    [111, 'дней'],
    [112, 'дней'],
  ])('%i -> %s', (n, expected) => {
    expect(plural(n, forms)).toBe(expected);
  });
});

describe('format_duration_ru', () => {
  it.each([
    [0, '0 секунд'],
    [1, '1 секунда'],
    [2, '2 секунды'],
    [5, '5 секунд'],
    [11, '11 секунд'],
    [21, '21 секунда'],
    [45, '45 секунд'],
    [60, '1 минута'],
    [61, '1 минута 1 секунда'],
    [150, '2 минуты 30 секунд'],
    [300, '5 минут'],
    [3600, '1 час'],
    [3 * 3600 + 2 * 60, '3 часа 2 минуты'],
    [5 * 3600, '5 часов'],
    [86400, '1 день'],
    [2 * 86400 + 3 * 3600 + 59 * 60, '2 дня 3 часа'],
    [11 * 86400 + 12 * 3600, '11 дней 12 часов'],
    [21 * 86400, '21 день'],
  ])('%i s -> %s', (seconds, expected) => {
    expect(formatDuration(seconds)).toBe(expected);
  });

  it('rounds fractions down and treats bad input as zero', () => {
    expect(formatDuration(59.9)).toBe('59 секунд');
    expect(formatDuration(-5)).toBe('0 секунд');
    expect(formatDuration(Number.NaN)).toBe('0 секунд');
  });
});

describe('format_bytes', () => {
  it.each([
    [0, `0${NB}Б`],
    [1, `1${NB}Б`],
    [1023, `1023${NB}Б`],
    [1024, `1${NB}КБ`],
    [1536, `1,5${NB}КБ`],
    [10 * 1024, `10${NB}КБ`],
    [123 * 1024 * 1024, `123${NB}МБ`],
    [5.25 * 1024 ** 3, `5,3${NB}ГБ`],
    [3 * 1024 ** 4, `3${NB}ТБ`],
    [2048 * 1024 ** 4, `2048${NB}ТБ`],
  ])('%i -> %s', (bytes, expected) => {
    expect(formatBytes(bytes)).toBe(expected);
  });

  it('treats bad input as zero', () => {
    expect(formatBytes(-1)).toBe(`0${NB}Б`);
    expect(formatBytes(Number.NaN)).toBe(`0${NB}Б`);
  });
});
