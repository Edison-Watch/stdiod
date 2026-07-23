use super::*;
use serde_json::json;
use tokio::io::duplex;

#[test]
fn diagnostics_are_bounded_and_redacted() {
    let diagnostics = ChildDiagnostics::default();
    diagnostics.record_stderr("Authorization: Bearer do-not-show");
    diagnostics.record_stderr("Connecting to https://example.test/mcp?key=do-not-show");
    diagnostics.record_stderr("postgres://user:password@example.test/database");
    diagnostics.record_stderr("AWS_ACCESS_KEY_ID=do-not-show");
    diagnostics.record_stderr("eyJhbGciOiJIUzI1NiJ9.payload.signature");
    let redacted = diagnostics.terminal_error("server", None);
    assert!(!redacted.message.contains("do-not-show"));
    assert!(redacted.message.contains("[redacted"));

    let diagnostics = ChildDiagnostics::new(["violet-horse-battery".into()]);
    diagnostics.record_stderr("upstream rejected violet-horse-battery");
    let redacted = diagnostics.terminal_error("server", None);
    assert!(!redacted.message.contains("violet-horse-battery"));
    assert!(redacted.message.contains("upstream rejected [redacted]"));

    let diagnostics = ChildDiagnostics::default();
    for index in 0..25 {
        diagnostics.record_stderr(&format!("diagnostic line {index}"));
    }

    let error = diagnostics.terminal_error("server", None);
    assert!(!error.message.contains("diagnostic line 0"));
    assert!(error.message.contains("diagnostic line 24"));
}

#[test]
fn terminal_error_is_emitted_only_once_per_process() {
    let diagnostics = ChildDiagnostics::default();
    assert!(diagnostics.take_terminal_error("server", None).is_some());
    assert!(diagnostics.take_terminal_error("server", None).is_none());
}

#[tokio::test]
async fn broken_stdin_replays_actionable_terminal_error() {
    let diagnostics = ChildDiagnostics::default();
    diagnostics.record_stderr("Connection failed: ECONNREFUSED localhost:23373");
    let outgoing = OutgoingHandle::new();
    let (wire_tx, mut wire_rx) = mpsc::channel(1);
    outgoing.set(wire_tx);
    let (frame_tx, frame_rx) = mpsc::channel(1);
    let (stdin, reader) = duplex(64);
    drop(reader);
    frame_tx.send(json!({"jsonrpc": "2.0"})).await.unwrap();
    drop(frame_tx);

    stdin_pump("server".into(), stdin, frame_rx, outgoing, diagnostics, None).await;

    let frame = wire_rx.recv().await.unwrap();
    let TunnelFrame::TunnelError(error) = frame else {
        panic!("expected terminal tunnel error");
    };
    assert_eq!(error.code, "server_offline");
    assert!(error.message.contains("ECONNREFUSED localhost:23373"));
}

#[cfg(unix)]
#[tokio::test]
async fn exited_process_reports_final_stderr_once() {
    let desired = DesiredServer {
        server_id: "server".into(),
        name: "server".into(),
        command: "/bin/sh".into(),
        args: vec![
            "-c".into(),
            "echo 'connect failed: ECONNREFUSED' >&2; exit 7".into(),
        ],
        env: Default::default(),
        working_dir: None,
        enabled: true,
    };
    let outgoing = OutgoingHandle::new();
    let (wire_tx, mut wire_rx) = mpsc::channel(4);
    outgoing.set(wire_tx);
    let child = ChildServer::spawn(&desired, &desired, outgoing, Vec::new()).unwrap();

    let frame = tokio::time::timeout(Duration::from_secs(1), wire_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let TunnelFrame::TunnelError(error) = frame else {
        panic!("expected terminal tunnel error");
    };
    assert!(error.message.contains("connect failed: ECONNREFUSED"));
    assert!(
        error.message.contains("exited with code 7"),
        "terminal diagnostic should carry the child's exit code: {}",
        error.message
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), wire_rx.recv())
            .await
            .is_err()
    );

    child.shutdown().await;
}
