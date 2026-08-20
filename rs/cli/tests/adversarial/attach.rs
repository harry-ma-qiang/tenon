use crate::support::*;
use std::io::{BufRead, BufReader};
use std::time::Duration;

/// Waits for the subscribe line and then keeps draining that attach's stdout in
/// a thread until the process ends. Dropping the reader instead would close the
/// pipe, and the next event base streams would kill `attach` with EPIPE — which
/// looks exactly like a subscriber detaching on purpose.
fn wait_for_attach_line(child: &mut std::process::Child) {
    let stdout = child.stdout.take().expect("attach stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            panic!("attach exited before subscribing: {}", stderr_of(child));
        }
        if line.contains("attached from") {
            std::thread::spawn(move || {
                let mut rest = String::new();
                while reader.read_line(&mut rest).unwrap_or(0) > 0 {
                    rest.clear();
                }
            });
            return;
        }
        if std::time::Instant::now() > deadline {
            panic!("attach never subscribed: {}", stderr_of(child));
        }
    }
}

fn stderr_of(child: &mut std::process::Child) -> String {
    let Some(mut pipe) = child.stderr.take() else {
        return "no stderr".to_string();
    };
    let mut text = String::new();
    let _ = std::io::Read::read_to_string(&mut pipe, &mut text);
    text
}

#[test]
fn attach_with_exit_on_detach_stops_everything_on_disconnect() {
    let Some(fixture) = fixture("attach-solo") else {
        return;
    };
    fixture.start_with(&["--exit-on-detach"]);
    let base = fixture.base_pid();
    let guardian = fixture.cli_node("guardian")["pid"].as_i64().unwrap();
    let root = fixture.cli_node("root")["pid"].as_i64().unwrap();

    let mut child = fixture.spawn(&["attach"]);
    wait_for_attach_line(&mut child);

    let _ = child.kill();
    let _ = child.wait();

    let took = wait_gone(&[base, guardian, root], Duration::from_secs(15));
    assert!(
        !alive(base),
        "base survived its only subscriber detaching after {took:?}"
    );
    assert!(
        !alive(guardian),
        "guardian survived exit-on-detach after {took:?}"
    );
    assert!(!alive(root), "root survived exit-on-detach after {took:?}");
}

#[test]
fn two_attaches_one_disconnect_keeps_running() {
    let Some(fixture) = fixture("attach-two") else {
        return;
    };
    fixture.start_with(&["--exit-on-detach"]);
    let base = fixture.base_pid();
    let guardian = fixture.cli_node("guardian")["pid"].as_i64().unwrap();
    let root = fixture.cli_node("root")["pid"].as_i64().unwrap();

    let mut a1 = fixture.spawn(&["attach"]);
    let mut a2 = fixture.spawn(&["attach"]);
    wait_for_attach_line(&mut a1);
    wait_for_attach_line(&mut a2);

    let _ = a1.kill();
    let _ = a1.wait();
    std::thread::sleep(Duration::from_millis(800));

    assert!(
        alive(base),
        "base stopped after only one of two attaches detached"
    );
    assert!(
        alive(guardian) && alive(root),
        "nodes stopped after one of two attaches detached"
    );
    let status = fixture.cli_status_result();
    assert!(
        status.is_ok(),
        "base stopped responding with a subscriber left: {status:?}"
    );

    let _ = a2.kill();
    let _ = a2.wait();
    let took = wait_gone(&[base, guardian, root], Duration::from_secs(15));
    assert!(
        !alive(base),
        "base survived its last subscriber detaching after {took:?}"
    );
}
