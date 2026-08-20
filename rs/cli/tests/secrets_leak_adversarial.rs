#![cfg(feature = "http")]

mod gate;

use base64::Engine;
use gate::{skip_release, Fixture, Spec, BIN};
use serde_json::{json, Value};
use std::time::Duration;
use tenon_base::client::Client;

const CONFIG: &str = "sandbox: none\n";
const HARNESS: &str = "llm:\n  provider: openai\n  base_url: http://127.0.0.1:1\n  \
     model: fake-model\n  api_key_env: TENON_TEST_NO_KEY\nmax_steps: 2\napproval: deny\n";
const VALUE: &str = "sk-SECRET-abc-123";

fn fixture(name: &str) -> Option<Fixture> {
    let release = skip_release(name)?;
    Some(Fixture::open(
        BIN,
        release,
        Spec {
            name,
            config: Some(CONFIG),
            harness: Some(HARNESS),
            reap_pids: true,
            lock: true,
            ..Spec::default()
        },
    ))
}

async fn wait_ready(fixture: &Fixture) {
    let ok = fixture
        .await_status(Duration::from_secs(120), |status| {
            status["nodes"]
                .as_array()
                .map(|nodes| nodes.len() >= 2)
                .unwrap_or(false)
        })
        .await;
    assert!(ok, "base never came up\n{}", fixture.log());
}

async fn subscribe(fixture: &Fixture, topics: Value) -> Client {
    let mut client = Client::connect(&fixture.sock()).await.expect("uds");
    client
        .call(
            "bus.subscribe",
            json!({"topics": topics, "since_offset": 0}),
        )
        .await
        .expect("subscribe");
    client
}

async fn next_ev(client: &mut Client, limit: Duration) -> Value {
    tokio::time::timeout(limit, client.next_ev())
        .await
        .expect("ev timed out")
        .expect("read")
        .expect("an ev frame")
}

async fn next_matching(client: &mut Client, topic: &str, limit: Duration) -> Value {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "never saw an envelope on {topic}");
        let event = next_ev(client, remaining).await;
        if event["topic"] == topic {
            return event;
        }
    }
}

/// RFC 8d.4 says the guard "scans its payload" -- literally the `payload`
/// field. `tags` is a separate, equally-open field on the same envelope
/// (RFC section 2), and nothing in `rs/bus/src/hub.rs::guard` touches it.
/// This is an honest exfiltration path: a producer (accidentally or
/// otherwise) that puts a secret value in a tag rather than the payload body
/// sails straight through both `mask` and `block` with the raw value intact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_secret_value_placed_in_tags_instead_of_payload_bypasses_mask_and_block() {
    let Some(fixture) = fixture("secrets-adv-tags") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;

    let mut base = Client::connect(&fixture.sock()).await.expect("uds");
    base.call(
        "secret.set",
        json!({"name": "tag-mask", "value": VALUE, "leak": "mask", "grants": ["root"]}),
    )
    .await
    .expect("set mask secret");
    base.call(
        "secret.set",
        json!({"name": "tag-block", "value": "TAGGED-BLOCK-VALUE", "leak": "block"}),
    )
    .await
    .expect("set block secret");

    let mut watcher = subscribe(&fixture, json!(["session/**"])).await;
    base.call(
        "bus.publish",
        json!({"envelope": {"topic": "session/tag-leak", "env": "root", "durable": true,
            "payload": {"ok": true}, "tags": {"leaked": VALUE}}}),
    )
    .await
    .expect("publish with secret in tags");
    let masked = next_matching(&mut watcher, "session/tag-leak", Duration::from_secs(10)).await;
    let tag_value = masked["tags"]["leaked"].as_str().unwrap_or_default();
    assert_ne!(
        tag_value, VALUE,
        "the guard only scans `payload`, not `tags`, so a secret value placed in a \
         tag is fanned out to every subscriber verbatim: {masked}"
    );

    let refused = base
        .call(
            "bus.publish",
            json!({"envelope": {"topic": "session/tag-block", "env": "root", "durable": true,
                "payload": {"ok": true}, "tags": {"leaked": "TAGGED-BLOCK-VALUE"}}}),
        )
        .await;
    assert!(
        refused.is_err(),
        "a block-policy secret value placed in `tags` should refuse the publish \
         exactly as it would from `payload`, but it was accepted: {refused:?}"
    );
}

/// Deeper structural coverage than the happy-path gate: the value nested
/// several objects/arrays deep, inside a longer surrounding string at the
/// same time, and split across sibling array elements.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nested_and_embedded_shapes_are_still_caught_in_the_payload() {
    let Some(fixture) = fixture("secrets-adv-nested") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;

    let mut base = Client::connect(&fixture.sock()).await.expect("uds");
    base.call(
        "secret.set",
        json!({"name": "nest-mask", "value": VALUE, "leak": "mask", "grants": ["root"]}),
    )
    .await
    .expect("set mask secret");

    let mut watcher = subscribe(&fixture, json!(["session/**"])).await;
    let payload = json!({
        "steps": [
            {"kind": "note", "text": "nothing here"},
            {"kind": "cmd", "text": format!("ran `curl -H 'Authorization: Bearer {VALUE}' https://x`")},
        ],
        "nested": {"deeper": {"deepest": [1, 2, VALUE]}},
    });
    base.call(
        "bus.publish",
        json!({"envelope": {"topic": "session/nested-leak", "env": "root", "durable": true,
            "payload": payload}}),
    )
    .await
    .expect("publish nested");
    let masked = next_matching(&mut watcher, "session/nested-leak", Duration::from_secs(10)).await;
    let flat = masked.to_string();
    assert!(
        !flat.contains(VALUE),
        "the value survived somewhere in a nested payload: {masked}"
    );
    assert!(
        flat.contains("***nest-mask***"),
        "the mask marker should appear at least once: {masked}"
    );
}

/// The RFC's own worker-side note is that tool output *tails* get the same
/// scrub before entering a payload. But the guard only ever sees one
/// envelope's payload at a time, as one contiguous string; it has no memory
/// across envelopes. If a producer streams a secret value split across two
/// separate durable envelopes (plausible for token-by-token or chunked tool
/// output), neither chunk contains the full value as a substring, so
/// neither `mask` nor `block` fires on either half -- the secret crosses the
/// wire and lands in the durable log in two adjoining, individually
/// undetectable pieces.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_secret_split_across_two_payload_chunks_defeats_the_guard() {
    let Some(fixture) = fixture("secrets-adv-split") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;

    let mut base = Client::connect(&fixture.sock()).await.expect("uds");
    base.call(
        "secret.set",
        json!({"name": "split-mask", "value": VALUE, "leak": "mask", "grants": ["root"]}),
    )
    .await
    .expect("set mask secret");

    let split = VALUE.len() / 2;
    let (head, tail) = VALUE.split_at(split);

    let mut watcher = subscribe(&fixture, json!(["session/**"])).await;
    base.call(
        "bus.publish",
        json!({"envelope": {"topic": "session/split-a", "env": "root", "durable": true,
            "payload": {"chunk": format!("...output tail: {head}")}}}),
    )
    .await
    .expect("publish chunk a");
    base.call(
        "bus.publish",
        json!({"envelope": {"topic": "session/split-b", "env": "root", "durable": true,
            "payload": {"chunk": format!("{tail} ...continues")}}}),
    )
    .await
    .expect("publish chunk b");

    let a = next_matching(&mut watcher, "session/split-a", Duration::from_secs(10)).await;
    let b = next_matching(&mut watcher, "session/split-b", Duration::from_secs(10)).await;
    let leaked = a.to_string().contains(head) && b.to_string().contains(tail);
    assert!(
        !leaked,
        "the secret's two halves both rode through unmasked because neither payload \
         contained the full contiguous value: chunk a {a}, chunk b {b} (known limitation \
         of substring-only scanning across envelope/frame boundaries -- see the RFC's \
         'tail' language in section 2 and 8d.4)"
    );
}

/// Honest documentation, not a bypass to fix: the guard does exact substring
/// matching against the literal value, so a base64- or URL-encoded copy of
/// the same secret is never recognised. This asserts today's actual,
/// expected behaviour (the value leaks in encoded form) so the limitation is
/// pinned down by a test rather than only prose.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn encoded_forms_of_a_secret_are_not_caught_documented_limitation() {
    let Some(fixture) = fixture("secrets-adv-encoded") else {
        return;
    };
    fixture.start();
    wait_ready(&fixture).await;

    let mut base = Client::connect(&fixture.sock()).await.expect("uds");
    base.call(
        "secret.set",
        json!({"name": "enc-mask", "value": VALUE, "leak": "mask", "grants": ["root"]}),
    )
    .await
    .expect("set mask secret");

    let b64 = base64::engine::general_purpose::STANDARD.encode(VALUE);
    let url_encoded = VALUE.replace('-', "%2D");

    let mut watcher = subscribe(&fixture, json!(["session/**"])).await;
    base.call(
        "bus.publish",
        json!({"envelope": {"topic": "session/enc-leak", "env": "root", "durable": true,
            "payload": {"b64": b64, "url": url_encoded}}}),
    )
    .await
    .expect("publish encoded forms");
    let seen = next_matching(&mut watcher, "session/enc-leak", Duration::from_secs(10)).await;
    let flat = seen.to_string();
    assert!(
        !flat.contains(VALUE),
        "sanity: the literal value should not itself be present: {seen}"
    );
    assert!(
        flat.contains(&base64::engine::general_purpose::STANDARD.encode(VALUE)),
        "documented limitation: a base64-encoded copy of the secret is not caught \
         by substring matching and passes through untouched: {seen}"
    );
}
