//! What the Globalping loop writes to the journal. A file of its own, with one test: a log line
//! that another thread reaches first while a scoped subscriber is set stays off for good, so
//! this test cannot share a binary with the other tests of the loop.

mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::FakePorts;
use mtpool_agent::gp::{GpJob, GpLoop};
use tokio::time::Instant;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[derive(Clone, Default)]
struct Journal(Arc<Mutex<Vec<u8>>>);

impl Write for Journal {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn a_measurement_that_was_not_made_says_why_in_the_journal() {
    let journal = Journal::default();
    let sink = journal.clone();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || sink.clone())
            .finish(),
    );

    for (status, reason) in [(429, "rate_limited"), (500, "unavailable")] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/measurements"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&server)
            .await;
        let gp = GpLoop::new(&server.uri(), FakePorts::new(true)).unwrap();
        gp.set_job(Some(GpJob {
            ip: "192.0.2.1".parse().unwrap(),
            port: 8443,
            sni: "tls.example.test".into(),
            interval: Duration::from_secs(600),
        }));

        gp.tick(Instant::now()).await;

        let text = String::from_utf8_lossy(&journal.0.lock().unwrap()).into_owned();
        let line = text
            .lines()
            .find(|l| l.contains("no Globalping measurement") && l.contains(reason));
        let line = line.unwrap_or_else(|| panic!("HTTP {status}: {text}"));
        assert!(line.contains("next_try_sec=600"), "{line}");
    }
}
