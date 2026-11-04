// Runs a change the reader asked for: tells the result in a toast and reads the data again.
import { ApiError } from '../lib/api';
import { t } from '../lib/i18n';
import { toast } from '../lib/toast.svelte';
import type { Panel } from './context.svelte';

/** True if it went through. A refusal is shown with its own title, anything else as a lost link. */
export async function act(panel: Panel, run: () => Promise<unknown>, done: string): Promise<boolean> {
  try {
    await run();
  } catch (error) {
    toast(error instanceof ApiError ? `${error.message}${error.detail ? `: ${error.detail}` : ''}` : t('error.network'), 5000);
    return false;
  }
  toast(done);
  panel.refreshAll();
  return true;
}
