// The settings document and the edits to it. Several pages show parts of it (the settings page,
// the domain manager), each part saved on its own: a save sends only the fields of that part.
import { ApiError } from '../lib/api';
import { toast } from '../lib/toast.svelte';
import type { Panel } from './context.svelte';
import { t } from './i18n';
import { FIELDS, parse, put, seed, type Problem, type Value, type Values } from './settings-fields';
import type { SettingsDoc } from './types';

export function createSettings(panel: Panel) {
  let doc = $state.raw<SettingsDoc | null>(null);
  let failed = $state(false);
  let saving = $state(false);
  const values = $state<Values>({});
  const errors = $state<Record<string, Problem>>({});

  const original = $derived<Values>(doc ? seed(doc) : {});

  function reseed(keys?: string[]): void {
    if (!doc) return;
    const fresh = seed(doc);
    for (const k of keys ?? Object.keys(fresh)) {
      values[k] = fresh[k] as Value;
      delete errors[k];
    }
  }

  async function load(): Promise<void> {
    try {
      doc = (await panel.client.get('/api/v1/admin/settings')) as unknown as SettingsDoc;
      reseed();
    } catch {
      failed = true;
    }
  }

  async function write(patch: Record<string, unknown>): Promise<boolean> {
    try {
      doc = (await panel.client.patch('/api/v1/admin/settings', { body: patch as never })) as unknown as SettingsDoc;
      panel.refreshAll();
      return true;
    } catch (error) {
      toast(error instanceof ApiError ? `${error.message}${error.detail ? `: ${error.detail}` : ''}` : t('error.network'), 5000);
      return false;
    }
  }

  const changed = (keys: string[]): string[] => keys.filter((k) => values[k] !== original[k]);

  /** Saves the changed fields among `keys`; false if one is invalid or the server refuses. */
  async function save(keys: string[]): Promise<boolean> {
    const list = changed(keys);
    if (list.length === 0 || saving) return false;
    const patch: Record<string, unknown> = {};
    let bad = false;
    for (const key of list) {
      const parsed = parse(FIELDS.get(key)!, values[key]);
      if (parsed.ok) {
        put(patch, key, parsed.value);
        delete errors[key];
      } else {
        errors[key] = parsed.problem;
        bad = true;
      }
    }
    if (bad) return false;
    saving = true;
    const ok = await write(patch);
    if (ok) {
      reseed(keys);
      toast(t('settings.saved'));
    }
    saving = false;
    return ok;
  }

  /** Sets one field to a value and saves it at once, for switches that act on their own. */
  async function setNow(key: string, value: Value): Promise<boolean> {
    values[key] = value;
    return save([key]);
  }

  return {
    get doc() {
      return doc;
    },
    get failed() {
      return failed;
    },
    get saving() {
      return saving;
    },
    get original() {
      return original;
    },
    values,
    errors,
    load,
    write,
    changed,
    save,
    setNow,
    reseed,
  };
}

export type SettingsStore = ReturnType<typeof createSettings>;
