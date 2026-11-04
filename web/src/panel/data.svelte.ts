// The data the frame and several pages read at once, polled once: the nodes, the status and the
// rollouts. The Shell makes it; a page asks for it, so a path is requested one time only.
import { getContext, setContext } from 'svelte';
import { usePolled, type Polled } from './live.svelte';
import type { AdminNode, AdminStatus, RolloutView } from './types';

export interface PanelData {
  nodes: Polled<AdminNode[]>;
  status: Polled<AdminStatus>;
  rollouts: Polled<RolloutView[]>;
}

const KEY = Symbol('panel-data');

export function provideData(): PanelData {
  const data: PanelData = {
    nodes: usePolled<AdminNode[]>(() => '/api/v1/admin/nodes'),
    status: usePolled<AdminStatus>(() => '/api/v1/admin/status'),
    rollouts: usePolled<RolloutView[]>(() => '/api/v1/admin/rollouts'),
  };
  setContext(KEY, data);
  return data;
}

export const useData = () => getContext<PanelData>(KEY);
