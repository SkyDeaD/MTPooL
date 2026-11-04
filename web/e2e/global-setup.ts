import { startStand } from './stand/stand.ts';

// Starts the stand once for the whole run and hands what the specs need to their workers through
// the environment. The token is made at this moment and exists nowhere else.
export default async function globalSetup(): Promise<() => Promise<void>> {
  const stand = await startStand();
  process.env.MTPOOL_STAND_BASE = stand.base;
  process.env.MTPOOL_STAND_FAKES_BASE = stand.fakes;
  process.env.MTPOOL_STAND_ADMIN_TOKEN = stand.adminToken;
  process.env.MTPOOL_STAND_ENROLL_TOKEN = stand.enrollToken;
  process.env.MTPOOL_STAND_IDS = JSON.stringify(stand.ids);
  process.env.MTPOOL_STAND_ROSTER = JSON.stringify(stand.roster);
  return async () => {
    await stand.stop();
  };
}
