use crate::support::*;
use std::time::Duration;

#[test]
fn double_start_refuses_and_first_keeps_running() {
    let Some(fixture) = fixture("double-start") else {
        return;
    };
    fixture.start(&[]);
    let base1 = fixture.base_pid();
    let guardian1 = fixture.node("guardian")["pid"].as_i64().unwrap();
    let root1 = fixture.node("root")["pid"].as_i64().unwrap();

    let (ok2, text2) = fixture.run_timeout(&["start"], Duration::from_secs(20));

    let bases = fixture.base_pids();
    assert_eq!(
        bases.len(),
        1,
        "expected exactly 1 base process after a second start attempt, found {bases:?} \
         (first base {base1}, second start ok={ok2}, output: {text2})"
    );
    assert!(
        !ok2,
        "a second `tenon start` against a live home should refuse cleanly, but it exited 0: {text2}"
    );
    assert!(
        alive(guardian1),
        "the first guardian died from a second start"
    );
    assert!(alive(root1), "the first root env died from a second start");
    assert!(alive(base1), "the first base died from a second start");
}

#[test]
fn stale_socket_and_ready_file_recover_on_next_start() {
    let Some(fixture) = fixture("stale-recover") else {
        return;
    };
    fixture.start(&[]);
    let base1 = fixture.base_pid();
    let guardian1 = fixture.node("guardian")["pid"].as_i64().unwrap();
    let root1 = fixture.node("root")["pid"].as_i64().unwrap();

    kill(base1, "-9");
    wait_gone(&[base1], Duration::from_secs(5));
    wait_gone(&[guardian1, root1], Duration::from_secs(5));
    assert!(
        fixture.sock().exists() || fixture.home.join("run/base.ready").exists(),
        "expected a crashed base to leave stale run/ files behind"
    );

    fixture.start(&[]);
    let ready_immediately_after_start =
        std::fs::read_to_string(fixture.home.join("run/base.ready")).unwrap_or_default();
    assert_ne!(
        ready_immediately_after_start.trim(),
        base1.to_string(),
        "`start` reported success while run/base.ready still named the crashed base's pid \
         ({base1}); a client reading the ready file right after `start` returns gets a dead pid"
    );

    let base2 = fixture.base_pid();
    assert_ne!(base1, base2, "recovered start reused the old base pid");
    let guardian2 = fixture.node("guardian")["pid"].as_i64().unwrap();
    let root2 = fixture.node("root")["pid"].as_i64().unwrap();
    assert!(fixture.node("guardian")["registered"] == true);
    assert!(fixture.node("root")["registered"] == true);
    assert!(alive(guardian2) && alive(root2));
}

#[test]
fn reset_storm_leaves_no_orphans() {
    let Some(fixture) = fixture("reset-storm") else {
        return;
    };
    fixture.start(&[]);
    let guardian0 = fixture.node("guardian")["pid"].as_i64().unwrap();
    let mut root_pids = vec![fixture.node("root")["pid"].as_i64().unwrap()];

    for round in 0..5 {
        let (ok, text) = fixture.run(&["reset"]);
        assert!(ok, "reset #{round} failed: {text}");
        let last = *root_pids.last().unwrap();
        root_pids.push(fixture.await_fresh("root", last));
    }

    assert_eq!(
        fixture.node("guardian")["pid"].as_i64().unwrap(),
        guardian0,
        "the guardian pid changed across a reset storm that never touched it"
    );
    assert!(alive(guardian0), "the guardian died during the reset storm");

    for old in &root_pids[..root_pids.len() - 1] {
        assert!(
            !alive(*old),
            "root pid {old} survived its own reset, an orphan"
        );
    }
    let newest = *root_pids.last().unwrap();
    assert!(alive(newest), "the final root pid {newest} is not running");
}

#[test]
fn stop_while_a_reset_is_in_flight() {
    let Some(fixture) = fixture("stop-vs-reset") else {
        return;
    };
    fixture.start(&[]);
    let base = fixture.base_pid();
    let guardian = fixture.node("guardian")["pid"].as_i64().unwrap();
    let root = fixture.node("root")["pid"].as_i64().unwrap();

    std::thread::scope(|scope| {
        let reset = scope.spawn(|| fixture.run_timeout(&["reset"], Duration::from_secs(20)));
        std::thread::sleep(Duration::from_millis(80));
        let stop = fixture.run_timeout(&["stop"], Duration::from_secs(20));
        let (reset_ok, reset_text) = reset.join().expect("reset thread panicked");
        println!("reset during stop: ok={reset_ok} text={reset_text}");
        let (stop_ok, stop_text) = stop;
        assert!(
            stop_ok || reset_ok,
            "neither stop nor reset completed cleanly: stop={stop_text} reset={reset_text}"
        );
    });

    let took = wait_gone(&[base, guardian, root], Duration::from_secs(15));
    assert!(
        !alive(base),
        "base survived stop-during-reset after {took:?}"
    );
    assert!(
        !fixture.sock().exists(),
        "socket left behind after stop-during-reset"
    );
    assert!(
        !fixture.home.join("run/base.ready").exists(),
        "ready file left behind after stop-during-reset"
    );
}

#[test]
fn sigterm_during_boot_leaves_no_zombies() {
    let Some(fixture) = fixture("sigterm-boot") else {
        return;
    };
    let mut child = std::process::Command::new(BIN)
        .arg("--home")
        .arg(&fixture.home)
        .arg("start")
        .arg("--foreground")
        .env("TENON_RELEASE_DIR", &fixture.release)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn foreground base");
    let base_pid = child.id() as i64;

    std::thread::sleep(Duration::from_millis(120));
    assert!(
        !fixture.home.join("run/base.ready").exists(),
        "test raced past boot completion, rerun"
    );
    // Whatever guardian/agent processes base has already forked by now, by pid,
    // regardless of how many helper/plugin processes each one also owns.
    let spawned_before_signal = fixture.node_pids();
    kill(base_pid, "-TERM");

    let took = wait_gone(&[base_pid], Duration::from_secs(5));
    assert!(
        !alive(base_pid),
        "base ignored SIGTERM during boot after {took:?}"
    );

    std::thread::sleep(Duration::from_secs(4));
    let still_alive: Vec<i64> = spawned_before_signal
        .iter()
        .copied()
        .filter(|pid| alive(*pid))
        .collect();
    let leftover_base = fixture.base_pids();
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        still_alive.is_empty() && leftover_base.is_empty(),
        "boot-time SIGTERM left zombie/orphan processes that were alive at signal time: \
         {still_alive:?} (of {spawned_before_signal:?}), plus base procs {leftover_base:?}"
    );
}

#[test]
fn twenty_parallel_status_calls_during_a_reset() {
    let Some(fixture) = fixture("concurrency") else {
        return;
    };
    fixture.start(&[]);

    std::thread::scope(|scope| {
        let fx = &fixture;
        let reset = scope.spawn(|| fx.run_timeout(&["reset"], Duration::from_secs(20)));
        let statuses: Vec<_> = (0..20)
            .map(|i| {
                scope.spawn(move || {
                    let result = fx.status_result();
                    (i, result)
                })
            })
            .collect();
        let (reset_ok, reset_text) = reset.join().expect("reset thread panicked");
        assert!(reset_ok, "reset failed under status load: {reset_text}");
        for handle in statuses {
            let (i, result) = handle.join().expect("status thread panicked");
            assert!(result.is_ok(), "status call {i} failed: {result:?}");
        }
    });
}
