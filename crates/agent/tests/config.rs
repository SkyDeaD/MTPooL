use std::path::Path;

use mtpool_agent::config::{ConfigError, DEFAULT_INTERVALS, DEFAULT_STATE_DIR, LocalConfig};

fn parse(text: &str) -> Result<LocalConfig, ConfigError> {
    LocalConfig::parse(text, Path::new("/etc/mtpool/agent.toml"))
}

fn invalid(text: &str) -> String {
    let error = parse(text).expect_err("must be refused");
    assert!(matches!(error, ConfigError::Invalid { .. }), "{error:?}");
    error.to_string()
}

#[test]
fn minimal_config_takes_the_defaults() {
    let config = parse("registrar = \"https://pool.example.test\"\nname = \"alpha\"\n").unwrap();
    assert_eq!(config.registrar.as_str(), "https://pool.example.test/");
    assert_eq!(config.name, "alpha");
    assert_eq!(config.state_dir, Path::new(DEFAULT_STATE_DIR));
    assert_eq!(config.port, None);
    assert!(config.antiscan);
    assert_eq!(config.intervals(), DEFAULT_INTERVALS);
    assert_eq!(
        (
            DEFAULT_INTERVALS.heartbeat_sec,
            DEFAULT_INTERVALS.globalping_sec,
            DEFAULT_INTERVALS.metrics_sec,
            DEFAULT_INTERVALS.sync_sec
        ),
        (15, 300, 60, 60)
    );
}

#[test]
fn every_key_is_read() {
    let config = parse(
        "registrar = \"https://pool.example.test/base\"\nname = \"a.b_c-1\"\nstate_dir = \"/tmp/x\"\nport = 8443\nantiscan = false\n\n[intervals]\nheartbeat_sec = 5\nsync_sec = 30\n",
    )
    .unwrap();
    assert_eq!(config.state_dir, Path::new("/tmp/x"));
    assert_eq!(config.port, Some(8443));
    assert!(!config.antiscan);
    let intervals = config.intervals();
    assert_eq!((intervals.heartbeat_sec, intervals.sync_sec), (5, 30));
    // What is not overridden keeps its default.
    assert_eq!((intervals.globalping_sec, intervals.metrics_sec), (300, 60));
}

#[test]
fn unknown_keys_are_errors_naming_the_file() {
    let message = invalid("registrar = \"https://p.example.test\"\nname = \"a\"\nbogus = 1\n");
    assert!(message.contains("/etc/mtpool/agent.toml"), "{message}");
    assert!(message.contains("bogus"), "{message}");
    let message =
        invalid("registrar = \"https://p.example.test\"\nname = \"a\"\n[intervals]\nbogus = 1\n");
    assert!(message.contains("bogus"), "{message}");
}

#[test]
fn registrar_and_name_are_required() {
    assert!(invalid("name = \"a\"\n").contains("registrar"));
    assert!(invalid("registrar = \"https://p.example.test\"\n").contains("name"));
}

#[test]
fn name_follows_the_registrar_rule() {
    let with = |name: &str| format!("registrar = \"https://p.example.test\"\nname = \"{name}\"\n");
    for good in ["a", "alpha", "a.b_c-1", "0", &"x".repeat(32)] {
        assert!(parse(&with(good)).is_ok(), "{good}");
    }
    for bad in ["", "-a", "a-", ".a", "a b", "a/b", "é", &"x".repeat(33)] {
        assert!(invalid(&with(bad)).contains("name"), "{bad}");
    }
}

#[test]
fn registrar_must_be_https_unless_it_is_loopback() {
    let with = |url: &str| format!("registrar = \"{url}\"\nname = \"a\"\n");
    assert!(parse(&with("https://pool.example.test")).is_ok());
    assert!(parse(&with("http://127.0.0.1:8080")).is_ok());
    assert!(parse(&with("http://localhost:8080")).is_ok());
    assert!(parse(&with("http://[::1]:8080")).is_ok());
    assert!(invalid(&with("http://pool.example.test")).contains("https"));
    assert!(invalid(&with("ftp://pool.example.test")).contains("https"));
    assert!(invalid(&with("pool.example.test")).contains("registrar"));
}

#[test]
fn port_and_intervals_must_be_positive() {
    let base = "registrar = \"https://p.example.test\"\nname = \"a\"\n";
    assert!(invalid(&format!("{base}port = 0\n")).contains("port"));
    assert!(invalid(&format!("{base}port = 70000\n")).contains("port"));
    for key in ["heartbeat_sec", "globalping_sec", "metrics_sec", "sync_sec"] {
        let message = invalid(&format!("{base}[intervals]\n{key} = 0\n"));
        assert!(message.contains(key), "{message}");
    }
}

#[test]
fn load_reads_a_file_and_names_a_missing_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent.toml");
    std::fs::write(
        &path,
        "registrar = \"https://p.example.test\"\nname = \"a\"\n",
    )
    .unwrap();
    assert_eq!(LocalConfig::load(&path).unwrap().name, "a");

    let missing = dir.path().join("missing.toml");
    let error = LocalConfig::load(&missing).expect_err("missing file");
    assert!(matches!(error, ConfigError::Read { .. }));
    assert!(
        error.to_string().contains(&missing.display().to_string()),
        "{error}"
    );

    std::fs::write(&path, "not toml [").unwrap();
    let error = LocalConfig::load(&path).expect_err("bad syntax");
    assert!(
        error.to_string().contains(&path.display().to_string()),
        "{error}"
    );
}

// ----- the environment of the binary ------------------------------------------------------

use mtpool_agent::config::Overrides;
use mtpool_agent::gp::DEFAULT_BASE as GLOBALPING_BASE;
use mtpool_agent::ip_detect::IP_SOURCES;

fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |name| {
        pairs
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| (*v).to_owned())
    }
}

fn env_error(pairs: &[(&str, &str)]) -> String {
    Overrides::from_lookup(env_of(pairs))
        .expect_err("must be refused")
        .to_string()
}

#[test]
fn the_environment_defaults_to_the_real_services() {
    let o = Overrides::from_lookup(env_of(&[])).unwrap();
    assert_eq!(o.globalping_url, GLOBALPING_BASE);
    assert_eq!(o.ip_sources, IP_SOURCES.map(String::from).to_vec());
    assert_eq!(o.ipsum_url, mtpool_agent::antiscan::IPSUM_URL);
    assert!(!o.allow_private_ips);
    // Empty values count as unset.
    let o = Overrides::from_lookup(env_of(&[
        ("MTPOOL_GLOBALPING_URL", ""),
        ("MTPOOL_IP_SOURCES", ""),
        ("MTPOOL_IPSUM_URL", ""),
    ]))
    .unwrap();
    assert_eq!(o.globalping_url, GLOBALPING_BASE);
    assert_eq!(o.ip_sources.len(), 4);
}

#[test]
fn the_environment_replaces_the_endpoints() {
    let o = Overrides::from_lookup(env_of(&[
        (
            "MTPOOL_GLOBALPING_URL",
            "http://fakes.example.test:9000/globalping/",
        ),
        (
            "MTPOOL_IP_SOURCES",
            "http://fakes.example.test:9000/ip, http://fakes.example.test:9000/ip?2",
        ),
        ("MTPOOL_IPSUM_URL", "http://fakes.example.test:9000/ipsum"),
    ]))
    .unwrap();
    assert_eq!(
        o.globalping_url,
        "http://fakes.example.test:9000/globalping/"
    );
    assert_eq!(
        o.ip_sources,
        [
            "http://fakes.example.test:9000/ip",
            "http://fakes.example.test:9000/ip?2"
        ]
    );
    assert_eq!(o.ipsum_url, "http://fakes.example.test:9000/ipsum");
}

#[test]
fn ip_sources_are_two_to_eight_urls() {
    let list = |n: usize| {
        (1..=n)
            .map(|i| format!("http://ip{i}.example.test/"))
            .collect::<Vec<_>>()
            .join(",")
    };
    for n in [0usize, 1, 9] {
        if n == 0 {
            continue;
        }
        let message = env_error(&[("MTPOOL_IP_SOURCES", &list(n))]);
        assert!(message.contains("MTPOOL_IP_SOURCES"), "{n}: {message}");
    }
    for n in [2usize, 3, 8] {
        let o = Overrides::from_lookup(env_of(&[("MTPOOL_IP_SOURCES", &list(n))])).unwrap();
        assert_eq!(o.ip_sources.len(), n);
    }
    for bad in [
        "http://a.example.test/,,http://b.example.test/",
        "http://a.example.test/,not a url",
        "http://a.example.test/,ftp://b.example.test/",
    ] {
        let message = env_error(&[("MTPOOL_IP_SOURCES", bad)]);
        assert!(message.contains("MTPOOL_IP_SOURCES"), "{bad}: {message}");
    }
}

#[test]
fn a_bad_endpoint_names_its_variable() {
    for var in ["MTPOOL_GLOBALPING_URL", "MTPOOL_IPSUM_URL"] {
        for bad in ["nope", "ftp://example.test/", "http://"] {
            let message = env_error(&[(var, bad)]);
            assert!(message.contains(var), "{var}={bad}: {message}");
        }
    }
}

#[test]
fn private_ips_are_off_unless_the_value_is_exactly_true() {
    for value in ["", "1", "TRUE", "True", "yes", "false", " true"] {
        let o = Overrides::from_lookup(env_of(&[("MTPOOL_ALLOW_PRIVATE_IPS", value)])).unwrap();
        assert!(!o.allow_private_ips, "{value:?}");
    }
    let o = Overrides::from_lookup(env_of(&[("MTPOOL_ALLOW_PRIVATE_IPS", "true")])).unwrap();
    assert!(o.allow_private_ips);
}

#[test]
fn private_ip_registrar_over_plain_http_needs_the_flag() {
    let text = "registrar = \"http://registrar:8080\"\nname = \"alpha\"\n";
    let origin = Path::new("/etc/mtpool/agent.toml");
    let message = invalid(text);
    assert!(message.contains("https"), "{message}");
    let config = LocalConfig::parse_allowing(text, origin, true).unwrap();
    assert_eq!(config.registrar.as_str(), "http://registrar:8080/");
    // The flag lifts the rule about plain http, nothing else.
    for bad in ["ftp://registrar:8080", "registrar:8080", "http://"] {
        let text = format!("registrar = \"{bad}\"\nname = \"alpha\"\n");
        assert!(
            LocalConfig::parse_allowing(&text, origin, true).is_err(),
            "{bad}"
        );
    }
    assert!(
        LocalConfig::parse_allowing(
            "registrar = \"https://pool.example.test\"\nname = \"alpha\"\n",
            origin,
            true
        )
        .is_ok()
    );
}

#[test]
fn check_host_port_defaults_to_22() {
    let config = parse("registrar = \"https://pool.example.test\"\nname = \"alpha\"\n").unwrap();
    assert_eq!(config.check_host_port, 22);
    let config = parse(
        "registrar = \"https://pool.example.test\"\nname = \"alpha\"\ncheck_host_port = 2222\n",
    )
    .unwrap();
    assert_eq!(config.check_host_port, 2222);
    invalid(
        "registrar = \"https://pool.example.test\"\nname = \"alpha\"\ncheck_host_port = 70000\n",
    );
}

#[test]
fn zero_turns_it_off() {
    let config =
        parse("registrar = \"https://pool.example.test\"\nname = \"alpha\"\ncheck_host_port = 0\n")
            .unwrap();
    assert_eq!(config.check_host_port, 0);
}
