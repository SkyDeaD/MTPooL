//! The client against a registrar double: every kind of answer decodes into the `Answer` that
//! says what it means.

use std::time::Duration;

use mtpool_agent::client::{Answer, DEFAULT_RETRY_AFTER, MAX_RETRY_AFTER, RegistrarClient};
use mtpool_protocol::agent::{EnrollRequest, TerminateReason};
use mtpool_protocol::public::Engine;
use reqwest::Url;
use serde_json::json;
use wiremock::matchers::{body_json, header, header_regex, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;
use common::*;

async fn setup(
    verb: &str,
    route: &str,
    template: ResponseTemplate,
) -> (MockServer, RegistrarClient) {
    let server = MockServer::start().await;
    Mock::given(method(verb))
        .and(path(route))
        .respond_with(template)
        .mount(&server)
        .await;
    let client = RegistrarClient::new(Url::parse(&server.uri()).unwrap()).unwrap();
    (server, client)
}

fn enroll_request() -> EnrollRequest {
    EnrollRequest {
        name: "alpha".into(),
        port: 443,
        engine: Engine::Telemt,
    }
}

#[tokio::test]
async fn ok_answers_decode() {
    let (_s, client) = setup(
        "POST",
        ENROLL,
        ResponseTemplate::new(200).set_body_json(enroll_response("t")),
    )
    .await;
    assert_eq!(
        client.enroll("e", &enroll_request()).await,
        Answer::Ok(enroll_response("t"))
    );

    let (_s, client) = setup(
        "GET",
        CONFIG,
        ResponseTemplate::new(200).set_body_json(agent_config(15, 60)),
    )
    .await;
    assert_eq!(client.config("n").await, Answer::Ok(agent_config(15, 60)));

    for route in [HEARTBEAT, "/api/v1/agent/retire", REPORT] {
        let (_s, client) = setup(
            "POST",
            route,
            ResponseTemplate::new(200).set_body_json(json!({})),
        )
        .await;
        let answer = match route {
            HEARTBEAT => client.heartbeat("n").await,
            REPORT => client.report("n", &sample_report()).await,
            _ => client.retire("n").await,
        };
        assert!(matches!(answer, Answer::Ok(_)), "{route}: {answer:?}");
    }
}

#[tokio::test]
async fn requests_carry_the_bearer_token_user_agent_and_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(ENROLL))
        .and(header("authorization", "Bearer enroll-secret"))
        .and(header_regex("user-agent", r"^mtpool-agent/\d"))
        .and(body_json(
            json!({ "name": "alpha", "port": 443, "engine": "telemt" }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(enroll_response("t")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(HEARTBEAT))
        .and(header("authorization", "Bearer node-secret"))
        .and(body_json(json!({})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(REPORT))
        .and(header("authorization", "Bearer node-secret"))
        .and(body_json(serde_json::to_value(sample_report()).unwrap()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;
    let client = RegistrarClient::new(Url::parse(&server.uri()).unwrap()).unwrap();
    assert!(matches!(
        client.enroll("enroll-secret", &enroll_request()).await,
        Answer::Ok(_)
    ));
    assert!(matches!(
        client.heartbeat("node-secret").await,
        Answer::Ok(_)
    ));
    assert!(matches!(
        client.report("node-secret", &sample_report()).await,
        Answer::Ok(_)
    ));
}

#[tokio::test]
async fn a_base_url_with_a_path_is_kept() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/pool/api/v1/agent/heartbeat"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;
    let client =
        RegistrarClient::new(Url::parse(&format!("{}/pool/", server.uri())).unwrap()).unwrap();
    assert!(matches!(client.heartbeat("n").await, Answer::Ok(_)));
}

#[tokio::test]
async fn status_codes_become_meanings() {
    let (_s, client) = setup(
        "POST",
        HEARTBEAT,
        ResponseTemplate::new(401).set_body_json(problem_body(401, "unauthorized")),
    )
    .await;
    assert_eq!(client.heartbeat("n").await, Answer::Unauthorized);

    let (_s, client) = setup(
        "POST",
        HEARTBEAT,
        ResponseTemplate::new(410).set_body_json(problem_body(410, "unknown_node")),
    )
    .await;
    assert_eq!(client.heartbeat("n").await, Answer::Unknown);

    // 401 and 410 mean the same with no body at all.
    let (_s, client) = setup("POST", HEARTBEAT, ResponseTemplate::new(410)).await;
    assert_eq!(client.heartbeat("n").await, Answer::Unknown);

    let (_s, client) = setup(
        "POST",
        HEARTBEAT,
        ResponseTemplate::new(403).set_body_json(terminate_body()),
    )
    .await;
    match client.heartbeat("n").await {
        Answer::Terminate(t) => {
            assert!(t.terminate);
            assert_eq!(t.reason, TerminateReason::Banned);
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_403_that_is_not_the_terminate_document_is_a_problem() {
    let (_s, client) = setup(
        "POST",
        ENROLL,
        ResponseTemplate::new(403).set_body_json(problem_body(403, "owner_limit")),
    )
    .await;
    match client.enroll("e", &enroll_request()).await {
        Answer::Problem(p) => assert_eq!((p.status, p.code.as_str()), (403, "owner_limit")),
        other => panic!("{other:?}"),
    }
    // `terminate: false` is not an order to stop either.
    let body = json!({ "terminate": false, "reason": "banned", "message": "m" });
    let (_s, client) = setup(
        "POST",
        HEARTBEAT,
        ResponseTemplate::new(403).set_body_json(body),
    )
    .await;
    assert!(matches!(client.heartbeat("n").await, Answer::Problem(_)));
}

#[tokio::test]
async fn retry_after_is_read_defaulted_and_capped() {
    let cases: [(Option<&str>, Duration); 6] = [
        (Some("30"), Duration::from_secs(30)),
        (Some(" 7 "), Duration::from_secs(7)),
        (None, DEFAULT_RETRY_AFTER),
        (Some("soon"), DEFAULT_RETRY_AFTER),
        (Some("Wed, 21 Oct 2026 07:28:00 GMT"), DEFAULT_RETRY_AFTER),
        (Some("99999999999"), MAX_RETRY_AFTER),
    ];
    for (header_value, expected) in cases {
        let mut template =
            ResponseTemplate::new(429).set_body_json(problem_body(429, "rate_limited"));
        if let Some(value) = header_value {
            template = template.insert_header("Retry-After", value);
        }
        let (_s, client) = setup("POST", HEARTBEAT, template).await;
        assert_eq!(
            client.heartbeat("n").await,
            Answer::Paused {
                retry_after: expected
            },
            "{header_value:?}"
        );
    }
    assert_eq!(DEFAULT_RETRY_AFTER, Duration::from_secs(60));
}

#[tokio::test]
async fn other_statuses_are_problems_with_their_status() {
    let (_s, client) = setup(
        "POST",
        REPORT,
        ResponseTemplate::new(409).set_body_json(problem_body(409, "clock_skew")),
    )
    .await;
    match client.report("n", &sample_report()).await {
        Answer::Problem(p) => assert_eq!((p.status, p.code.as_str()), (409, "clock_skew")),
        other => panic!("{other:?}"),
    }
    // A 5xx with a body that is not a problem document still has its status.
    let (_s, client) = setup(
        "POST",
        REPORT,
        ResponseTemplate::new(502).set_body_string("<html>bad gateway</html>"),
    )
    .await;
    match client.report("n", &sample_report()).await {
        Answer::Problem(p) => assert_eq!(p.status, 502),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(HEARTBEAT))
        .respond_with(
            ResponseTemplate::new(302).insert_header("Location", "http://127.0.0.1:1/elsewhere"),
        )
        .mount(&server)
        .await;
    let client = RegistrarClient::new(Url::parse(&server.uri()).unwrap()).unwrap();
    match client.heartbeat("n").await {
        Answer::Problem(p) => assert_eq!(p.status, 302),
        other => panic!("{other:?}"),
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn unreadable_answers_and_dead_connections_are_transport_errors() {
    let (_s, client) = setup(
        "GET",
        CONFIG,
        ResponseTemplate::new(200).set_body_string("not json"),
    )
    .await;
    assert!(matches!(client.config("n").await, Answer::Transport(_)));
    let (_s, client) = setup(
        "GET",
        CONFIG,
        ResponseTemplate::new(200).set_body_json(json!({"users": 1})),
    )
    .await;
    assert!(matches!(client.config("n").await, Answer::Transport(_)));

    // Nothing listens here.
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };
    let client = RegistrarClient::new(Url::parse(&format!("http://{closed}")).unwrap()).unwrap();
    assert!(matches!(client.heartbeat("n").await, Answer::Transport(_)));
}

#[tokio::test]
async fn an_oversized_answer_is_refused() {
    let big = "x".repeat(2 * 1024 * 1024);
    let (_s, client) = setup(
        "GET",
        CONFIG,
        ResponseTemplate::new(200).set_body_string(big),
    )
    .await;
    assert!(matches!(client.config("n").await, Answer::Transport(_)));
}

#[tokio::test]
async fn a_transport_error_does_not_contain_the_token() {
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };
    let client = RegistrarClient::new(Url::parse(&format!("http://{closed}")).unwrap()).unwrap();
    let answer = client.heartbeat("super-secret-token").await;
    let text = format!("{answer:?}");
    assert!(!text.contains("super-secret-token"), "{text}");
}
