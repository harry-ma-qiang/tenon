use crate::support::*;

#[test]
fn corrupt_profile_is_restored_from_lkg_on_reset() {
    let Some(fixture) = fixture("lkg-profile") else {
        return;
    };
    fixture.start(&[]);
    let profile = fixture.home.join("profiles/root/tenon.yml");
    let lkg_profile = fixture.home.join("lkg/profiles/root/tenon.yml");
    let good = std::fs::read(&lkg_profile).expect("lkg profile copy should exist after boot");

    std::fs::write(&profile, b"not: [valid, yaml, {{{").expect("corrupt the live profile");

    let (ok, text) = fixture.run(&["reset"]);
    assert!(ok, "reset failed: {text}");
    let response: serde_json::Value = serde_json::from_str(&text).expect("reset json");
    assert_eq!(
        response["lkg"], true,
        "reset should report it restored from lkg: {text}"
    );

    let restored = std::fs::read(&profile).expect("profile should exist after reset");
    assert_eq!(
        restored, good,
        "the corrupted profile was not replaced by the lkg copy"
    );

    let came_back = fixture.await_condition(std::time::Duration::from_secs(15), |status| {
        status["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|node| node["env"] == "root" && node["registered"] == true)
    });
    assert!(came_back, "root did not come back after a profile restore");
}

#[test]
fn corrupt_state_sqlite_is_restored_from_lkg_on_reset() {
    let Some(fixture) = fixture("lkg-state") else {
        return;
    };
    fixture.start(&[]);
    let state = fixture.home.join("state.sqlite");
    let lkg_state = fixture.home.join("lkg/state.sqlite");
    assert!(
        lkg_state.is_file(),
        "boot should have promoted a state.sqlite lkg copy"
    );
    let good = std::fs::read(&lkg_state).expect("read lkg state.sqlite");
    assert!(!good.is_empty(), "the lkg state.sqlite copy is empty");

    std::fs::File::create(&state)
        .and_then(|file| file.set_len(0))
        .expect("truncate the live state.sqlite");

    let (ok, text) = fixture.run(&["reset"]);
    assert!(ok, "reset failed: {text}");

    let after = std::fs::read(&state).expect("state.sqlite should still exist after reset");
    assert!(
        after.starts_with(b"SQLite format 3\0") && after.len() >= good.len() / 2,
        "reset did not restore a corrupted state.sqlite from lkg: {} bytes, header {:?}",
        after.len(),
        &after[..after.len().min(16)]
    );

    assert!(
        fixture.status_result().is_ok(),
        "base stopped answering after a state.sqlite corruption"
    );
}
