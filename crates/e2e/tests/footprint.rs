//! The footprint of the agent on a node of the Docker stand (specification, section 13): under
//! 20 MB of resident memory at idle and a binary under 15 MB. Needs Docker, so it is ignored by
//! default: `cargo test -p mtpool-e2e --test footprint -- --ignored --nocapture`.

use std::time::Duration;

use mtpool_e2e::Stand;

/// 20 MB as decimal megabytes (the stricter reading), in the kibibytes /proc reports.
const RSS_LIMIT_KB: u64 = 20_000_000 / 1024;
const BINARY_LIMIT_BYTES: u64 = 15_000_000;

#[tokio::test]
#[ignore = "needs Docker; run with --ignored"]
async fn agent_idles_under_20_mb_and_the_binary_is_under_15_mb() {
    let stand = Stand::up().await.expect("the stand comes up");
    let node = stand.node(1);
    let node_id = node.enroll("footprint").await.expect("the node enrolls");
    stand
        .wait_state(&node_id, "healthy", Duration::from_secs(120))
        .await
        .expect("the node becomes healthy");

    // The stand's intervals are accelerated for the scenarios. Idle is the agent as it runs in
    // production: the default intervals, an enrolled node, nothing else going on.
    node.exec(
        "printf 'registrar = \"http://registrar:8080\"\\nname = \"footprint\"\\nantiscan = false\\n' \
         > /etc/mtpool/agent.toml && systemctl restart mtpool-agent",
    )
    .await
    .expect("the agent restarts with the default intervals");
    tokio::time::sleep(Duration::from_secs(90)).await;

    let pid = node
        .exec("systemctl show -p MainPID --value mtpool-agent")
        .await
        .expect("main pid");
    let pid = pid.trim();
    assert!(
        pid.parse::<u32>().is_ok_and(|p| p > 0),
        "the agent is not running: pid {pid:?}"
    );
    let status = node
        .exec(&format!("cat /proc/{pid}/status"))
        .await
        .expect("agent status");
    let kb = |name: &str| -> u64 {
        status
            .lines()
            .find_map(|l| {
                l.strip_prefix(name)?
                    .trim()
                    .strip_suffix("kB")?
                    .trim()
                    .parse()
                    .ok()
            })
            .unwrap_or_else(|| panic!("no {name} in the status of the agent"))
    };
    let (rss, hwm) = (kb("VmRSS:"), kb("VmHWM:"));
    let size: u64 = node
        .exec("stat -c %s /usr/local/bin/mtpool-agent")
        .await
        .expect("binary size")
        .trim()
        .parse()
        .expect("size is a number");
    println!("agent idle: VmRSS {rss} kB, VmHWM {hwm} kB, binary {size} bytes");

    assert!(
        rss < RSS_LIMIT_KB,
        "agent RSS {rss} kB, limit {RSS_LIMIT_KB} kB"
    );
    assert!(size < BINARY_LIMIT_BYTES, "agent binary {size} bytes");
}
