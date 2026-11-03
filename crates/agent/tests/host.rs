use std::io;

use mtpool_agent::host::testing::{Call, ScriptedRunner};
use mtpool_agent::host::{CommandRunner, TokioRunner};

#[tokio::test]
async fn scripted_runner_answers_in_order_and_records_calls() {
    let runner = ScriptedRunner::new();
    runner.push_output(0, b"first");
    runner.push_output(3, b"");
    runner.push_error(io::Error::new(io::ErrorKind::NotFound, "no such program"));

    let one = runner
        .run("ipset", &["restore"], Some(b"create x"))
        .await
        .unwrap();
    assert!(one.status.success());
    assert_eq!(one.stdout, b"first");
    let two = runner
        .run("systemctl", &["restart", "telemt"], None)
        .await
        .unwrap();
    assert_eq!(two.status.code(), Some(3));
    let three = runner.run("nope", &[], None).await;
    assert_eq!(three.unwrap_err().kind(), io::ErrorKind::NotFound);
    // The script is used up: the runner says so instead of inventing an answer.
    assert!(runner.run("more", &[], None).await.is_err());

    let calls = runner.calls();
    assert_eq!(calls.len(), 4);
    assert_eq!(
        calls[0],
        Call {
            program: "ipset".into(),
            args: vec!["restore".into()],
            stdin: Some(b"create x".to_vec())
        }
    );
    assert_eq!(calls[1].args, ["restart", "telemt"]);
    assert_eq!(calls[1].stdin, None);
}

#[tokio::test]
async fn a_program_that_cannot_start_is_an_error() {
    // Nothing is executed: the path does not exist.
    let result = TokioRunner
        .run("/nonexistent/mtpool-test-program", &["x"], Some(b"in"))
        .await;
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::NotFound);
}
