//! The Cloudflare provider against the real API. Run by hand:
//!
//! ```text
//! ( set -a; . ./.env; set +a; cargo test -p mtpool-registrar --test cloudflare_live -- --ignored )
//! ```
//!
//! It needs `CLOUDFLARE_API_TOKEN` and `MTPOOL_TEST_ZONE` (a zone the token may edit). The test
//! makes one record named `mtpool-test-<8 random chars>.<zone>`, reads it back, deletes it and
//! checks that it is gone. It touches no other record. Neither the token nor the zone is ever
//! printed: what it says about a failure is the generated label and counts.

use std::sync::Arc;

use mtpool_core::{NewRecord, RecordKind};
use mtpool_registrar::dns::cloudflare::CloudflareDns;
use mtpool_registrar::dns::{DnsProvider, MANAGED_COMMENT};

fn required(name: &str) -> String {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => value.trim().to_owned(),
        _ => panic!(
            "{name} is not set: this test needs both CLOUDFLARE_API_TOKEN and MTPOOL_TEST_ZONE"
        ),
    }
}

fn random_label() -> String {
    let mut bytes = [0u8; 8];
    rand::fill(&mut bytes);
    let alphabet = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let chars: String = bytes
        .iter()
        .map(|b| char::from(alphabet[usize::from(*b) % alphabet.len()]))
        .collect();
    format!("mtpool-test-{chars}")
}

#[tokio::test]
#[ignore = "talks to the real Cloudflare API; needs CLOUDFLARE_API_TOKEN and MTPOOL_TEST_ZONE"]
async fn a_record_is_created_listed_and_deleted() {
    let token = required("CLOUDFLARE_API_TOKEN");
    let zone_name = required("MTPOOL_TEST_ZONE");
    let label = random_label();
    let name = format!("{label}.{zone_name}");
    let dns = CloudflareDns::new(
        CloudflareDns::default_base(),
        Arc::new(move || Some(token.clone())),
    )
    .expect("client");

    let mut created: Option<String> = None;
    let mut zone: Option<String> = None;
    let outcome = async {
        let zone_id = dns
            .zone_id(&name)
            .await
            .map_err(|e| format!("zone lookup failed: {}", e.reason()))?
            .ok_or("the test zone was not found for this token")?;
        zone = Some(zone_id.clone());

        let before = dns
            .list(&zone_id, &name)
            .await
            .map_err(|e| format!("first list: {}", e.reason()))?;
        if !before.is_empty() {
            return Err(format!(
                "{label}: the name already has {} records",
                before.len()
            ));
        }

        let record = NewRecord {
            name: name.clone(),
            kind: RecordKind::A,
            content: "192.0.2.1".to_owned(),
        };
        let id = dns
            .create(&zone_id, record, 60, false)
            .await
            .map_err(|e| format!("create: {}", e.reason()))?;
        created = Some(id.clone());

        let listed = dns
            .list(&zone_id, &name)
            .await
            .map_err(|e| format!("list after create: {}", e.reason()))?;
        if listed.len() != 1 {
            return Err(format!(
                "{label}: {} records after create, expected 1",
                listed.len()
            ));
        }
        let found = &listed[0];
        if found.live.id != id {
            return Err(format!("{label}: the listed record is not the created one"));
        }
        if found.live.name != name
            || found.live.kind != RecordKind::A
            || found.live.content != "192.0.2.1"
            || found.live.proxied
        {
            return Err(format!(
                "{label}: the listed record differs from what was created"
            ));
        }
        if !found.managed {
            return Err(format!(
                "{label}: the record does not carry the comment {MANAGED_COMMENT}"
            ));
        }

        dns.delete(&zone_id, &id)
            .await
            .map_err(|e| format!("delete: {}", e.reason()))?;
        created = None;

        let after = dns
            .list(&zone_id, &name)
            .await
            .map_err(|e| format!("list after delete: {}", e.reason()))?;
        if !after.is_empty() {
            return Err(format!(
                "{label}: {} records remain after delete",
                after.len()
            ));
        }
        Ok::<(), String>(())
    }
    .await;

    // Whatever happened, do not leave the record behind.
    if let (Some(id), Some(zone_id)) = (&created, &zone) {
        let cleaned = dns.delete(zone_id, id).await;
        eprintln!(
            "{label}: cleanup of the left record: {}",
            if cleaned.is_ok() { "done" } else { "FAILED" }
        );
    }
    if let Err(message) = outcome {
        panic!("{message}");
    }
}
