//! Finding the public address of the node. The answers are the recorded shapes of the four
//! services (the address in them is a documentation address).

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use mtpool_agent::engine::http::HttpResponse;
use mtpool_agent::engine::http::testing::ScriptedHttp;
use mtpool_agent::ip_detect::{
    IP_CHECK_EVERY, IP_SOURCES, IpEvent, IpWatch, PUBLIC_IP_FILE, detect_public_ip,
    detect_public_ip_allowing,
};
use mtpool_agent::state_files::{Fs, MemFs};
use tokio::time::Instant;

const IPIFY: &str = include_str!("fixtures/ip/ipify.txt");
const IFCONFIG_ME: &str = include_str!("fixtures/ip/ifconfig_me.txt");
const ICANHAZIP: &str = include_str!("fixtures/ip/icanhazip.txt");
const IPINFO: &str = include_str!("fixtures/ip/ipinfo.txt");

const STATE_DIR: &str = "/state";

fn text(body: &str) -> Result<HttpResponse, String> {
    Ok(HttpResponse {
        status: 200,
        body: body.to_owned(),
    })
}

/// The four services answer with these, in the order of [`IP_SOURCES`].
fn script(answers: [Result<HttpResponse, String>; 4]) -> Arc<ScriptedHttp> {
    let http = Arc::new(ScriptedHttp::new());
    for (url, answer) in IP_SOURCES.iter().zip(answers) {
        http.always(url, answer);
    }
    http
}

fn recorded() -> Arc<ScriptedHttp> {
    script([
        text(IPIFY),
        text(IFCONFIG_ME),
        text(ICANHAZIP),
        text(IPINFO),
    ])
}

fn watch(http: &Arc<ScriptedHttp>, fs: &Arc<MemFs>) -> IpWatch<ScriptedHttp> {
    IpWatch::new(http.clone(), fs.clone(), PathBuf::from(STATE_DIR))
}

fn ip(text: &str) -> std::net::Ipv4Addr {
    text.parse().unwrap()
}

#[tokio::test]
async fn ip_detect_majority_of_4() {
    // The recorded answers, newline of icanhazip and all.
    assert_eq!(
        detect_public_ip(&*recorded(), &IP_SOURCES).await,
        Some(ip("192.0.2.1"))
    );

    // Two against two others that differ: two win.
    let http = script([
        text("192.0.2.1"),
        text("192.0.2.1"),
        text("198.51.100.4"),
        text("203.0.113.9\n"),
    ]);
    assert_eq!(
        detect_public_ip(&*http, &IP_SOURCES).await,
        Some(ip("192.0.2.1"))
    );

    // Three against one failure; one service broken, one lying.
    let http = script([
        text("192.0.2.1"),
        Err("connection refused".into()),
        text("192.0.2.1\n"),
        Ok(HttpResponse {
            status: 500,
            body: "192.0.2.77".into(),
        }),
    ]);
    assert_eq!(
        detect_public_ip(&*http, &IP_SOURCES).await,
        Some(ip("192.0.2.1"))
    );

    // A split without a majority is no answer.
    let http = script([
        text("192.0.2.1"),
        text("192.0.2.1"),
        text("198.51.100.4"),
        text("198.51.100.4"),
    ]);
    assert_eq!(detect_public_ip(&*http, &IP_SOURCES).await, None);

    // All different, or only one answers: no answer.
    let http = script([
        text("192.0.2.1"),
        text("198.51.100.4"),
        text("203.0.113.9"),
        Err("timeout".into()),
    ]);
    assert_eq!(detect_public_ip(&*http, &IP_SOURCES).await, None);
    let http = script([
        text("192.0.2.1"),
        Err("x".into()),
        Err("x".into()),
        Err("x".into()),
    ]);
    assert_eq!(detect_public_ip(&*http, &IP_SOURCES).await, None);
}

#[tokio::test]
async fn answers_that_are_not_public_ipv4_do_not_count() {
    for bad in [
        "10.0.0.5",
        "127.0.0.1",
        "192.168.1.9",
        "169.254.1.1",
        "100.64.0.1",
        "0.0.0.0",
        "2001:db8::1",
        "<html>",
        "",
    ] {
        let http = script([text(bad), text(bad), text(bad), text(bad)]);
        assert_eq!(detect_public_ip(&*http, &IP_SOURCES).await, None, "{bad:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_source_that_never_answers_does_not_hold_the_others() {
    struct Silent;
    impl mtpool_agent::engine::http::HttpGet for Silent {
        async fn get(&self, url: &str, _auth: Option<&str>) -> Result<HttpResponse, String> {
            if url == IP_SOURCES[3] {
                std::future::pending::<()>().await;
            }
            Ok(HttpResponse {
                status: 200,
                body: "192.0.2.1".into(),
            })
        }
    }
    let started = tokio::time::Instant::now();
    assert_eq!(
        detect_public_ip(&Silent, &IP_SOURCES).await,
        Some(ip("192.0.2.1"))
    );
    assert!(started.elapsed() <= Duration::from_secs(5));
}

#[tokio::test]
async fn ip_checked_every_45s() {
    assert_eq!(IP_CHECK_EVERY, Duration::from_secs(45));
    let http = recorded();
    let fs = Arc::new(MemFs::new());
    let mut watch = watch(&http, &fs);
    let t0 = Instant::now();

    assert_eq!(watch.current(), None);
    assert_eq!(watch.check(t0).await, IpEvent::Found(ip("192.0.2.1")));
    assert_eq!(http.requests().len(), 4);
    assert_eq!(watch.current(), Some(ip("192.0.2.1")));

    assert_eq!(
        watch.check(t0 + Duration::from_secs(44)).await,
        IpEvent::Quiet
    );
    assert_eq!(http.requests().len(), 4, "not due yet: no requests");

    assert_eq!(
        watch.check(t0 + Duration::from_secs(45)).await,
        IpEvent::Quiet
    );
    assert_eq!(http.requests().len(), 8);
    assert_eq!(
        watch.check(t0 + Duration::from_secs(89)).await,
        IpEvent::Quiet
    );
    assert_eq!(http.requests().len(), 8);
    assert_eq!(
        watch.check(t0 + Duration::from_secs(90)).await,
        IpEvent::Quiet
    );
    assert_eq!(http.requests().len(), 12);
    for request in http.requests() {
        assert_eq!(request.authorization, None);
    }
}

#[tokio::test]
async fn ip_change_is_reported_once() {
    let http = recorded();
    let fs = Arc::new(MemFs::new());
    let mut watch = watch(&http, &fs);
    let t0 = Instant::now();
    assert_eq!(watch.check(t0).await, IpEvent::Found(ip("192.0.2.1")));

    // The services now say another address.
    for url in IP_SOURCES {
        http.always(url, text("198.51.100.4"));
    }
    assert_eq!(
        watch.check(t0 + IP_CHECK_EVERY).await,
        IpEvent::Changed {
            from: ip("192.0.2.1"),
            to: ip("198.51.100.4")
        }
    );
    assert_eq!(watch.current(), Some(ip("198.51.100.4")));
    // It is told once.
    assert_eq!(watch.check(t0 + 2 * IP_CHECK_EVERY).await, IpEvent::Quiet);
}

#[tokio::test]
async fn no_answer_keeps_the_last_known_address() {
    let http = recorded();
    let fs = Arc::new(MemFs::new());
    let mut watch = watch(&http, &fs);
    let t0 = Instant::now();
    watch.check(t0).await;

    for url in IP_SOURCES {
        http.always(url, Err("down".into()));
    }
    assert_eq!(watch.check(t0 + IP_CHECK_EVERY).await, IpEvent::Quiet);
    assert_eq!(watch.current(), Some(ip("192.0.2.1")));
}

#[tokio::test]
async fn the_known_address_survives_a_restart() {
    let http = recorded();
    let fs = Arc::new(MemFs::new());
    let t0 = Instant::now();
    {
        let mut first = watch(&http, &fs);
        first.check(t0).await;
    }
    let stored = fs
        .read(&Path::new(STATE_DIR).join(PUBLIC_IP_FILE))
        .unwrap()
        .map(|b| String::from_utf8(b).unwrap());
    assert_eq!(stored.as_deref().map(str::trim), Some("192.0.2.1"));

    // After a restart the address is another one: that is a change.
    for url in IP_SOURCES {
        http.always(url, text("198.51.100.4"));
    }
    let mut second = watch(&http, &fs);
    assert_eq!(second.current(), Some(ip("192.0.2.1")));
    assert_eq!(
        second.check(t0).await,
        IpEvent::Changed {
            from: ip("192.0.2.1"),
            to: ip("198.51.100.4")
        }
    );
}

// ----- any number of sources, private addresses on a test network --------------------------

/// `n` sources, `answer(i)` the text source `i` gives.
fn sources_with(
    n: usize,
    answer: impl Fn(usize) -> &'static str,
) -> (Vec<String>, Arc<ScriptedHttp>) {
    let urls: Vec<String> = (0..n)
        .map(|i| format!("http://ip{i}.example.test/"))
        .collect();
    let http = Arc::new(ScriptedHttp::new());
    for (i, url) in urls.iter().enumerate() {
        http.always(url, text(answer(i)));
    }
    (urls, http)
}

fn refs(urls: &[String]) -> Vec<&str> {
    urls.iter().map(String::as_str).collect()
}

#[tokio::test]
async fn ip_detect_needs_two_agreeing_sources_out_of_any_number() {
    // Two sources: both must agree.
    let (urls, http) = sources_with(2, |_| "192.0.2.1");
    assert_eq!(
        detect_public_ip(&*http, &refs(&urls)).await,
        Some(ip("192.0.2.1"))
    );
    let (urls, http) = sources_with(2, |i| ["192.0.2.1", "192.0.2.2"][i]);
    assert_eq!(detect_public_ip(&*http, &refs(&urls)).await, None);

    // Eight sources: every one is asked, two that agree are enough.
    let (urls, http) = sources_with(8, |i| match i {
        0 => "192.0.2.9",
        3 => "192.0.2.9",
        _ => "not an address",
    });
    assert_eq!(
        detect_public_ip(&*http, &refs(&urls)).await,
        Some(ip("192.0.2.9"))
    );
    assert_eq!(http.requests().len(), 8);

    // Two different pairs in eight are a split.
    let (urls, http) = sources_with(8, |i| match i {
        0 | 1 => "192.0.2.1",
        2 | 3 => "192.0.2.2",
        _ => "",
    });
    assert_eq!(detect_public_ip(&*http, &refs(&urls)).await, None);

    // One lonely answer is no majority.
    let (urls, http) = sources_with(3, |i| ["192.0.2.1", "", ""][i]);
    assert_eq!(detect_public_ip(&*http, &refs(&urls)).await, None);
}

#[tokio::test]
async fn private_addresses_count_only_when_allowed() {
    for private in ["10.0.0.5", "172.18.0.3", "192.168.1.9", "100.64.0.1"] {
        let (urls, http) = sources_with(4, |_| match private {
            "10.0.0.5" => "10.0.0.5",
            "172.18.0.3" => "172.18.0.3",
            "192.168.1.9" => "192.168.1.9",
            _ => "100.64.0.1",
        });
        assert_eq!(
            detect_public_ip(&*http, &refs(&urls)).await,
            None,
            "{private}"
        );
        assert_eq!(
            detect_public_ip_allowing(&*http, &refs(&urls), true).await,
            Some(ip(private)),
            "{private}"
        );
        assert_eq!(
            detect_public_ip_allowing(&*http, &refs(&urls), false).await,
            None,
            "{private}"
        );
    }
    // What is no address of a host stays refused with the flag.
    for bad in [
        "0.0.0.0",
        "224.0.0.1",
        "255.255.255.255",
        "240.0.0.1",
        "127.0.0.1",
    ] {
        let (urls, http) = sources_with(2, |_| match bad {
            "0.0.0.0" => "0.0.0.0",
            "224.0.0.1" => "224.0.0.1",
            "255.255.255.255" => "255.255.255.255",
            "240.0.0.1" => "240.0.0.1",
            _ => "127.0.0.1",
        });
        let found = detect_public_ip_allowing(&*http, &refs(&urls), true).await;
        // Loopback is a private network address too; the others are never accepted.
        let expected = (bad == "127.0.0.1").then(|| ip(bad));
        assert_eq!(found, expected, "{bad}");
    }
}

#[tokio::test]
async fn a_watch_with_other_sources_and_private_addresses_finds_its_address() {
    let (urls, http) = sources_with(3, |_| "172.18.0.3");
    let fs = Arc::new(MemFs::new());
    let mut watch = watch(&http, &fs)
        .with_sources(urls.clone())
        .with_allow_private(true);
    assert_eq!(
        watch.check(Instant::now()).await,
        IpEvent::Found(ip("172.18.0.3"))
    );
    assert_eq!(http.requests().len(), 3);
    // It is remembered, and read back by the next run with the flag on.
    let mut again = IpWatch::new(http.clone(), fs.clone(), PathBuf::from(STATE_DIR))
        .with_sources(urls)
        .with_allow_private(true);
    assert_eq!(again.check(Instant::now()).await, IpEvent::Quiet);
}
