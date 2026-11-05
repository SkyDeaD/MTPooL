use std::sync::Arc;

use mtpool_core::rollout::{Phase, Stage, Status};
use mtpool_core::{NodeId, RolloutSettings, Ts};

use super::*;

const T0: Ts = Ts(1_800_000_000_000);

fn clock() -> Clock {
    Arc::new(|| T0)
}

fn id(n: usize) -> NodeId {
    NodeId::parse(&format!("n{n:02}-aaaaa")).unwrap()
}

fn service() -> (tempfile::TempDir, Store, RolloutService) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("pool.db")).unwrap();
    let svc = RolloutService::load(store.clone(), clock()).unwrap();
    (dir, store, svc)
}

#[test]
fn a_newer_release_starts_a_rollout_and_an_older_one_does_not() {
    let (_d, _s, svc) = service();
    assert!(svc.offer_release("0.2.0", T0, RolloutSettings::default()));
    assert!(!svc.offer_release("0.2.0", T0, RolloutSettings::default()));
    assert!(!svc.offer_release("0.1.9", T0, RolloutSettings::default()));
    assert!(!svc.offer_release("not-a-version", T0, RolloutSettings::default()));
    let all = svc.list().unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].id, 1);
}

#[test]
fn a_newer_release_cancels_the_open_rollout_and_keeps_it_listed() {
    let (_d, _s, svc) = service();
    svc.offer_release("0.2.0", T0, RolloutSettings::default());
    assert!(svc.offer_release("0.3.0", T0, RolloutSettings::default()));
    let all = svc.list().unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].version, "0.3.0");
    assert_eq!(all[0].id, 2);
    assert!(matches!(all[1].phase, Phase::Cancelled { .. }));
    assert_eq!(all[1].note.as_ref().unwrap().code, "superseded");
}

#[test]
fn the_rollout_survives_a_restart() {
    let (_d, store, svc) = service();
    svc.offer_release("0.2.0", T0, RolloutSettings::default());
    svc.poll(&id(1), "0.1.0", T0);
    svc.status(&id(1), "0.2.0", &Status::Started, T0);
    svc.status(&id(2), "0.1.0", &Status::Started, T0);
    svc.flush();
    let before = svc.get(1).unwrap().unwrap();
    let again = RolloutService::load(store, clock()).unwrap();
    assert_eq!(again.get(1).unwrap().unwrap(), before);
    // The same release does not start a second rollout after the restart.
    assert!(!again.offer_release("0.2.0", T0, RolloutSettings::default()));
    assert!(matches!(
        again.get(1).unwrap().unwrap().members[&id(1)].stage,
        Stage::Started { .. } | Stage::Idle
    ));
}

#[test]
fn resume_and_cancel_name_a_rollout_that_exists() {
    let (_d, _s, svc) = service();
    assert_eq!(svc.cancel(1, T0), None);
    svc.offer_release("0.2.0", T0, RolloutSettings::default());
    assert_eq!(svc.resume(1, &[], T0), Some(false));
    assert_eq!(svc.cancel(1, T0), Some(true));
    assert_eq!(svc.cancel(1, T0), Some(false));
    assert_eq!(svc.cancel(7, T0), None);
}

#[test]
fn a_closed_rollout_offers_nothing() {
    let (_d, _s, svc) = service();
    svc.offer_release("0.2.0", T0, RolloutSettings::default());
    svc.cancel(1, T0);
    assert!(!svc.poll(&id(1), "0.1.0", T0));
}

#[test]
fn a_poll_without_a_rollout_makes_the_version_visible_and_is_not_a_change() {
    let (_d, _s, svc) = service();
    assert!(!svc.poll(&id(1), "1.9.0", T0));
    assert_eq!(
        svc.agent_versions().get(&id(1)).map(String::as_str),
        Some("1.9.0")
    );
    assert!(!lock(&svc.inner).dirty);
    assert!(!svc.poll(&id(1), "1.9.1", T0));
    assert_eq!(svc.agent_versions()[&id(1)], "1.9.1");
}

#[test]
fn the_version_stays_after_the_rollout_is_over() {
    let (_d, _s, svc) = service();
    svc.offer_release("2.0.0", T0, RolloutSettings::default());
    svc.poll(&id(1), "1.9.0", T0);
    assert_eq!(svc.cancel(1, T0), Some(true));
    assert_eq!(svc.agent_versions()[&id(1)], "1.9.0");
}

fn v(text: &str) -> Version {
    Version::parse(text).unwrap()
}

#[test]
fn a_release_newer_than_the_registrar_starts_no_rollout() {
    let (_d, _s, svc) = service();
    svc.limit_to(v("0.2.0"), T0);
    assert!(!svc.offer_release("0.3.0", T0, RolloutSettings::default()));
    assert!(svc.list().unwrap().is_empty());
    assert_eq!(svc.offered_version(), None);
    // The refused release leaves no mark: the release the registrar does understand still goes.
    assert!(svc.offer_release("0.2.0", T0, RolloutSettings::default()));
    assert_eq!(svc.offered_version().as_deref(), Some("0.2.0"));
}

#[test]
fn a_registrar_rolled_back_stops_the_rollout_of_the_agent_it_may_not_understand() {
    let (_d, store, svc) = service();
    // The new registrar starts the rollout of its own agent and saves it.
    svc.limit_to(v("0.3.0"), T0);
    assert!(svc.offer_release("0.3.0", T0, RolloutSettings::default()));
    svc.poll(&id(1), "0.2.0", T0);
    svc.flush();

    // The rollback: the old build reads the same database, the manifest is still in the directory.
    let old = RolloutService::load(store.clone(), clock()).unwrap();
    assert_eq!(old.offered_version().as_deref(), Some("0.3.0"));
    old.limit_to(v("0.2.0"), T0);
    assert_eq!(old.offered_version(), None);
    assert!(!old.poll(&id(1), "0.2.0", T0));
    assert!(!old.offer_release("0.3.0", T0, RolloutSettings::default()));
    let stopped = old.get(1).unwrap().unwrap();
    assert!(matches!(stopped.phase, Phase::Cancelled { .. }));
    assert_eq!(stopped.note.as_ref().unwrap().code, "registrar_behind");

    // The stop is saved: the old build restarted does not take the rollout up again.
    let again = RolloutService::load(store.clone(), clock()).unwrap();
    again.limit_to(v("0.2.0"), T0);
    assert_eq!(again.offered_version(), None);
    assert!(matches!(
        again.get(1).unwrap().unwrap().phase,
        Phase::Cancelled { .. }
    ));

    // The registrar gets to that version after all: its agent goes out in a rollout of its own.
    let back = RolloutService::load(store, clock()).unwrap();
    back.limit_to(v("0.3.0"), T0);
    assert!(back.offer_release("0.3.0", T0, RolloutSettings::default()));
    assert_eq!(back.list().unwrap()[0].id, 2);
    assert_eq!(back.offered_version().as_deref(), Some("0.3.0"));
}

#[test]
fn a_rollback_keeps_the_memory_of_the_versions_already_rolled_out() {
    let (_d, store, svc) = service();
    svc.offer_release("0.2.0", T0, RolloutSettings::default());
    svc.offer_release("0.3.0", T0, RolloutSettings::default());
    svc.flush();
    let old = RolloutService::load(store, clock()).unwrap();
    old.limit_to(v("0.2.0"), T0);
    assert_eq!(
        old.get(2).unwrap().unwrap().note.unwrap().code,
        "registrar_behind"
    );
    // Still the same process: 0.2.0 has had its rollout and does not get a second one.
    assert!(!old.offer_release("0.2.0", T0, RolloutSettings::default()));
}

#[test]
fn a_rollout_the_registrar_understands_goes_on_after_a_restart() {
    let (_d, store, svc) = service();
    svc.offer_release("0.2.0", T0, RolloutSettings::default());
    svc.flush();
    let before = svc.get(1).unwrap().unwrap();
    let again = RolloutService::load(store, clock()).unwrap();
    again.limit_to(v("0.2.0"), T0);
    assert_eq!(again.get(1).unwrap().unwrap(), before);
    assert_eq!(again.offered_version().as_deref(), Some("0.2.0"));
    // The version it has rolled out is not started a second time.
    assert!(!again.offer_release("0.2.0", T0, RolloutSettings::default()));
}

#[test]
fn a_finished_rollout_is_left_as_it_ended_after_a_rollback() {
    let (_d, store, svc) = service();
    svc.offer_release("0.3.0", T0, RolloutSettings::default());
    assert_eq!(svc.cancel(1, T0), Some(true));
    svc.flush();
    let before = svc.get(1).unwrap().unwrap();
    let old = RolloutService::load(store, clock()).unwrap();
    old.limit_to(v("0.2.0"), T0);
    assert_eq!(old.get(1).unwrap().unwrap(), before);
}
