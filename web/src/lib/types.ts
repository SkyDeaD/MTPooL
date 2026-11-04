// Short names for the API shapes the pages use, from the generated document.
import type { components } from './api-types';

type S = components['schemas'];

export type PublicNode = S['PublicNode'];
export type PublicDomain = S['PublicDomain'];
export type NodeChecks = S['NodeChecks'];
export type LastRound = S['LastRound'];
export type NodeHistory = S['NodeHistory'];
export type ProxyLink = S['ProxyLink'];
export type BanRecord = S['BanRecord'];
export type ClientsSeries = S['ClientsSeries'];
export type ClientsPoint = S['ClientsPoint'];
export type LiveClients = S['LiveClients'];
export type ClientsRange = S['ClientsRange'];
export type StatsSummary = S['StatsSummary'];
export type StatsSeries = S['StatsSeries'];
export type StatsRange = S['StatsRange'];
export type Engine = S['Engine'];
