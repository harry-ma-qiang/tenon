mod gate;

use gate::{fixture, skip};
use std::time::Duration;

const NAME: &str = "harness-model";
const KEY: &str = "DEEPSEEK_API_KEY";
const BASE_URL: &str = "https://api.deepseek.com";
const MODEL: &str = "deepseek-v4-flash";

/// The real-model smoke: one `tenon run` against DeepSeek. Skipped, not
/// failed, wherever the key, a container runtime or the release is missing —
/// the key never enters the sandbox and is never printed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_real_model_answers_one_turn() {
    if std::env::var(KEY)
        .ok()
        .filter(|key| !key.is_empty())
        .is_none()
    {
        println!("skipping {NAME}: {KEY} is not set");
        return;
    }
    let Some(release) = skip(NAME) else { return };
    let harness = format!(
        "llm:\n  provider: deepseek\n  base_url: {BASE_URL}\n  model: {MODEL}\n  \
         api_key_env: {KEY}\nmax_steps: 4\n"
    );
    let fixture = fixture(NAME, release, "sandbox: oci\n", &harness);
    let (ok, out, err) = fixture.run(&["start"]);
    assert!(ok, "start failed: {out}{err}");
    fixture.ready(Duration::from_secs(120)).await;
    let (ok, out, err) =
        fixture.run(&["run", "reply with the single word pong", "--timeout", "180"]);
    assert!(ok, "tenon run failed: {out}{err}");
    assert!(
        out.to_lowercase().contains("pong"),
        "the model did not answer pong: {out:?}"
    );
    assert!(err.contains("usage"), "no usage reported: {err:?}");
}
