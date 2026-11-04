// Admin API shapes the panel uses, from the generated document. The settings document is a
// free-form object there, so it is described here by hand.
import type { components } from '../lib/api-types';

type S = components['schemas'];

export type AdminNode = S['AdminNode'];
export type AdminNodeDetail = S['AdminNodeDetail'];
export type AdminRound = S['AdminRound'];
export type AdminVote = S['AdminVote'];
export type AdminStatus = S['AdminStatus'];
export type EventEntry = S['EventEntry'];
export type AuditEntry = S['AuditEntry'];
export type Owner = S['Owner'];
export type TokenMeta = S['TokenMeta'];
export type NewTokenCreated = S['NewTokenCreated'];
export type RolloutView = S['RolloutView'];
export type SrmdOverview = S['SrmdOverview'];
export type SensorStatus = S['SensorStatus'];

/** The settings as the server returns them; `secrets` says which secret fields are set. */
export type SettingsDoc = Record<string, unknown> & {
  secrets: Record<string, boolean>;
};

/** The admin API calls RedFlag `red_flag`; the shared vote components say `redflag`. */
export function sensorKey(name: string): string {
  return name === 'red_flag' ? 'redflag' : name;
}
