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
