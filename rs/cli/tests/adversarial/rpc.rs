use crate::support::*;
use std::time::Duration;

#[test]
fn garbage_bytes_do_not_crash_base() {
    let Some(fixture) = fixture("rpc-garbage") else {
        return;
    };
    fixture.start(&[]);
    let base = fixture.base_pid();

    let mut stream = raw_connect(&fixture.sock());
    send_raw(&mut stream, b"not json at all").expect("write garbage");
    drop(stream);
    std::thread::sleep(Duration::from_millis(300));

    assert!(alive(base), "base died on a garbage frame");
    assert!(
        fixture.status_result().is_ok(),
        "base stopped answering after a garbage frame"
    );
}

#[test]
fn oversized_frame_header_is_rejected_not_fatal() {
    let Some(fixture) = fixture("rpc-oversize") else {
        return;
    };
    fixture.start(&[]);
    let base = fixture.base_pid();

    let mut stream = raw_connect(&fixture.sock());
    let huge = (2_000_000u32).to_be_bytes();
    std::io::Write::write_all(&mut stream, &huge).expect("write oversized header");
    drop(stream);
    std::thread::sleep(Duration::from_millis(300));

    assert!(alive(base), "base died on an oversized frame header");
    assert!(
        fixture.status_result().is_ok(),
        "base stopped answering after an oversized frame"
    );
}

#[test]
fn unknown_method_answers_an_error_and_the_connection_stays_useful() {
    let Some(fixture) = fixture("rpc-unknown") else {
        return;
    };
    fixture.start(&[]);

    let mut stream = raw_connect(&fixture.sock());
    send_frame(
        &mut stream,
        &serde_json::json!({"t": "not_a_real_method", "id": 1}),
    )
    .expect("send unknown method");
    let reply = read_frame(&mut stream, Duration::from_secs(5)).expect("read reply");
    assert_eq!(reply["id"], 1);
    assert_eq!(reply["error"], "unknown_method:not_a_real_method");

    send_frame(&mut stream, &serde_json::json!({"t": "status", "id": 2})).expect("send status");
    let reply = read_frame(&mut stream, Duration::from_secs(5)).expect("read status reply");
    assert_eq!(reply["id"], 2);
    assert!(
        reply.get("error").is_none(),
        "status failed after an unknown method on the same connection: {reply}"
    );
}

#[test]
fn a_half_open_connection_does_not_block_other_clients() {
    let Some(fixture) = fixture("rpc-halfopen") else {
        return;
    };
    fixture.start(&[]);

    let _idle = raw_connect(&fixture.sock());
    for i in 0..5 {
        assert!(
            fixture.status_result().is_ok(),
            "status call {i} blocked by an idle half-open peer"
        );
    }
}

#[test]
fn node_register_from_an_untrusted_peer_should_not_hijack_a_running_env() {
    let Some(fixture) = fixture("rpc-hijack") else {
        return;
    };
    fixture.start(&[]);
    let real_pid = fixture.node("root")["pid"].as_i64().unwrap();

    let mut stream = raw_connect(&fixture.sock());
    send_frame(
        &mut stream,
        &serde_json::json!({"t": "node.register", "role": "agent", "env": "root", "pid": 999999}),
    )
    .expect("send forged node.register");
    std::thread::sleep(Duration::from_millis(300));

    let root = fixture.node("root");
    assert_eq!(
        root["pid"].as_i64().unwrap(),
        real_pid,
        "an unauthenticated UDS peer overwrote base's record of root's pid to a value it \
         chose (999999); reset would then signal an arbitrary pid"
    );
}
