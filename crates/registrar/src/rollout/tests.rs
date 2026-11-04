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
