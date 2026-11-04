//! The smoke scenario of the Docker stand. Needs Docker, so it is ignored by default:
//! `cargo test -p mtpool-e2e --test smoke -- --ignored --nocapture`.

use std::time::Duration;

use mtpool_e2e::Stand;

const DOMAIN: &str = "pool.example.com";

#[tokio::test]
#[ignore = "needs Docker; run with --ignored"]
async fn node_enrolls_becomes_master_and_dns_points_to_it() {
    let stand = Stand::up().await.expect("the stand comes up");

    let node = stand.node(1);
    let node_id = node.enroll("node-one").await.expect("the node enrolls");

    let nodes = stand.nodes().await.expect("public node list");
    assert!(
        nodes
            .iter()
            .any(|n| n["id"].as_str() == Some(node_id.as_str())),
        "the node is not in the public list: {nodes:#?}"
    );

    stand
        .wait_state(&node_id, "healthy", Duration::from_secs(120))
        .await
        .expect("the node becomes healthy");
    stand
        .wait_master(DOMAIN, &node_id, Duration::from_secs(60))
        .await
        .expect("the node becomes the master of the domain");

    let ip = node.ip().await.expect("the node's address");
    let record = stand
        .wait_dns_record(DOMAIN, &ip, Duration::from_secs(60))
        .await
        .expect("the DNS record points to the node");
    assert_eq!(
        record["comment"], "MTPooL",
        "the record is managed: {record:#?}"
    );

    let domains = stand.public_domains().await.expect("public domains");
    let listed = domains.to_string();
    assert!(
        listed.contains(&node_id),
        "the domain list does not show the node as master: {domains:#?}"
    );
}
