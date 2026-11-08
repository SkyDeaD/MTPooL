//! A firewall filter of another pool on the pool's port, found in `iptables-save`.

use std::sync::Arc;
use std::time::Duration;

use mtpool_agent::host::testing::ScriptedRunner;
use mtpool_agent::port_filter::{PortFilterWatch, find_port_filter};
use tokio::time::Instant;

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/iptables-save/{name}.txt",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn found(name: &str, port: u16) -> Option<String> {
    find_port_filter(&fixture(name), port)
}

#[test]
fn a_jump_to_a_chain_with_a_set_drop_is_found() {
    assert_eq!(found("foreign_chain", 443), Some("scan".into()));
}

#[test]
fn a_direct_set_drop_is_found() {
    assert_eq!(found("direct", 443), Some("scan".into()));
}

#[test]
fn a_rule_for_all_ports_is_found() {
    assert_eq!(found("all_ports", 443), Some("scan".into()));
}

#[test]
fn multiport_covering_the_port_is_found() {
    assert_eq!(found("multiport", 443), Some("scan".into()));
    assert_eq!(found("multiport", 8443), None);
}

#[test]
fn another_port_is_not() {
    assert_eq!(found("other_port", 443), None);
    assert_eq!(found("other_port", 8443), Some("scan".into()));
}

#[test]
fn our_own_antiscan_is_not_a_foreign_filter() {
    assert_eq!(found("own_antiscan", 443), None);
}

#[test]
fn an_allowlist_is_not_a_filter() {
    assert_eq!(found("allowlist", 443), None);
}

#[test]
fn a_set_renamed_by_the_other_pool_is_still_found() {
    assert_eq!(found("foreign_renamed", 443), Some("blocklist-7".into()));
}

#[test]
fn nothing_in_the_table_is_nothing() {
    assert_eq!(found("none", 443), None);
}

#[test]
fn a_port_range_and_a_reject_are_found() {
    let save = "*filter\n:INPUT ACCEPT [0:0]\n\
        -A INPUT -p tcp -m tcp --dport 400:500 -m set --match-set r src -j REJECT --reject-with icmp-port-unreachable\n\
        COMMIT\n";
    assert_eq!(find_port_filter(save, 443), Some("r".into()));
    assert_eq!(find_port_filter(save, 501), None);
}

#[test]
fn a_udp_rule_and_other_tables_are_not_the_port() {
    let save = "*nat\n:INPUT ACCEPT [0:0]\n\
        -A INPUT -m set --match-set n src -j DROP\nCOMMIT\n\
        *filter\n:INPUT ACCEPT [0:0]\n\
        -A INPUT -p udp -m udp --dport 443 -m set --match-set u src -j DROP\nCOMMIT\n";
    assert_eq!(find_port_filter(save, 443), None);
}

#[test]
fn chains_jumping_in_a_circle_end() {
    let save = "*filter\n:INPUT ACCEPT [0:0]\n:A - [0:0]\n:B - [0:0]\n\
        -A INPUT -j A\n-A A -j B\n-A B -j A\nCOMMIT\n";
    assert_eq!(find_port_filter(save, 443), None);
}

#[test]
fn a_chain_deeper_than_four_is_not_followed() {
    let chains = |n: usize| {
        let mut save = String::from("*filter\n:INPUT ACCEPT [0:0]\n");
        for i in 1..=n {
            save.push_str(&format!(":C{i} - [0:0]\n"));
        }
        save.push_str("-A INPUT -j C1\n");
        for i in 1..n {
            save.push_str(&format!("-A C{i} -j C{}\n", i + 1));
        }
        save.push_str(&format!(
            "-A C{n} -m set --match-set deep src -j DROP\nCOMMIT\n"
        ));
        save
    };
    assert_eq!(find_port_filter(&chains(4), 443), Some("deep".into()));
    assert_eq!(find_port_filter(&chains(5), 443), None);
}

#[test]
fn the_name_is_bounded_and_made_of_what_ipset_allows() {
    let long = "a".repeat(100);
    let save = format!(
        "*filter\n:INPUT ACCEPT [0:0]\n-A INPUT -m set --match-set {long} src -j DROP\nCOMMIT\n"
    );
    assert_eq!(find_port_filter(&save, 443).unwrap().len(), 64);
    let odd =
        "*filter\n:INPUT ACCEPT [0:0]\n-A INPUT -m set --match-set a<b>c;d src -j DROP\nCOMMIT\n";
    assert_eq!(find_port_filter(odd, 443), Some("abcd".into()));
    let none = "*filter\n:INPUT ACCEPT [0:0]\n-A INPUT -m set --match-set <> src -j DROP\nCOMMIT\n";
    assert_eq!(find_port_filter(none, 443), None);
}

fn secs(base: Instant, s: u64) -> Instant {
    base + Duration::from_secs(s)
}

#[tokio::test]
async fn no_iptables_save_means_no_filter_and_no_panic() {
    let runner = Arc::new(ScriptedRunner::new());
    runner.push_error(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "no iptables-save",
    ));
    runner.push_failure(1, "secret-looking stderr");
    runner.push_output(0, b"\xff\xfe not text");
    let watch = PortFilterWatch::new(runner.clone());
    let t0 = Instant::now();
    watch.tick(t0, Some(443)).await;
    assert_eq!(watch.current(), None);
    watch.tick(secs(t0, 1800), Some(443)).await;
    assert_eq!(watch.current(), None);
    watch.tick(secs(t0, 3600), Some(443)).await;
    assert_eq!(watch.current(), None);
    assert_eq!(runner.calls().len(), 3);
}

#[tokio::test]
async fn the_rules_are_read_every_30_minutes() {
    let runner = Arc::new(ScriptedRunner::new());
    runner.push_output(0, fixture("direct").as_bytes());
    runner.push_output(0, fixture("none").as_bytes());
    let watch = PortFilterWatch::new(runner.clone());
    let t0 = Instant::now();
    watch.tick(t0, Some(443)).await;
    assert_eq!(watch.current(), Some("scan".into()));
    assert_eq!(runner.calls()[0].program, "iptables-save");
    watch.tick(secs(t0, 1799), Some(443)).await;
    assert_eq!(runner.calls().len(), 1, "not yet");
    assert_eq!(watch.current(), Some("scan".into()));
    watch.tick(secs(t0, 1800), Some(443)).await;
    assert_eq!(runner.calls().len(), 2);
    assert_eq!(watch.current(), None, "the filter is gone");
}

#[tokio::test]
async fn a_changed_pool_port_is_read_at_once() {
    let runner = Arc::new(ScriptedRunner::new());
    runner.push_output(0, fixture("other_port").as_bytes());
    runner.push_output(0, fixture("other_port").as_bytes());
    let watch = PortFilterWatch::new(runner.clone());
    let t0 = Instant::now();
    watch.tick(t0, Some(443)).await;
    assert_eq!(watch.current(), None);
    watch.tick(secs(t0, 5), Some(8443)).await;
    assert_eq!(runner.calls().len(), 2);
    assert_eq!(watch.current(), Some("scan".into()));
}

#[tokio::test]
async fn without_a_port_nothing_is_read() {
    let runner = Arc::new(ScriptedRunner::new());
    let watch = PortFilterWatch::new(runner.clone());
    watch.tick(Instant::now(), None).await;
    assert!(runner.calls().is_empty());
    assert_eq!(watch.current(), None);
}
