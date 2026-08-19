use crate::support::*;
use std::process::Command;
use std::time::{Duration, Instant};
use tenon_base::home::Home;

const LEAK_IMAGE: &str = "python:3.12-alpine";

fn oci_cli() -> Option<&'static str> {
    let path = std::env::var_os("PATH")?;
    ["podman", "docker"]
        .into_iter()
        .find(|name| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
}

fn dead_pid() -> i64 {
    let mut child = Command::new("true").spawn().expect("spawn true");
    let pid = child.id() as i64;
    let _ = child.wait();
    wait_gone(&[pid], Duration::from_secs(5));
    pid
}

struct LeakedContainer {
    cli: &'static str,
    id: String,
    removed: bool,
}

impl LeakedContainer {
    fn create(cli: &'static str, env: &str, home_hash: &str, base_pid: i64) -> Self {
        let name = format!("tenon-{home_hash}-{env}-leak-test");
        let output = Command::new(cli)
            .args([
                "run",
                "-d",
                "--name",
                &name,
                "--label",
                &format!("tenon.env={env}"),
                "--label",
                &format!("tenon.home={home_hash}"),
                "--label",
                &format!("tenon.base={base_pid}"),
                LEAK_IMAGE,
                "sleep",
                "infinity",
            ])
            .output()
            .expect("run leaked container");
        assert!(
            output.status.success(),
            "could not create the leaked test container: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Self {
            cli,
            id: String::from_utf8_lossy(&output.stdout).trim().to_string(),
            removed: false,
        }
    }

    fn exists(&self) -> bool {
        let output = Command::new(self.cli)
            .args([
                "ps",
                "-a",
                "--filter",
                &format!("id={}", self.id),
                "--format",
                "{{.ID}}",
            ])
            .output()
            .expect("ps leaked container");
        !String::from_utf8_lossy(&output.stdout).trim().is_empty()
    }
}

impl Drop for LeakedContainer {
    fn drop(&mut self) {
        if self.removed {
            return;
        }
        let _ = Command::new(self.cli).args(["rm", "-f", &self.id]).status();
    }
}

#[test]
fn a_leaked_container_with_a_dead_base_is_reaped_on_next_start() {
    let Some(cli) = oci_cli() else {
        println!("skipping reap-leak: neither podman nor docker found in PATH");
        return;
    };
    let Some(fixture) = fixture_with_config("reap-leak", Some("sandbox: oci\n")) else {
        return;
    };
    let home_hash = Home {
        root: fixture.home.clone(),
    }
    .hash();

    let mut leaked = LeakedContainer::create(cli, "stale-env", &home_hash, dead_pid());
    assert!(leaked.exists(), "the leaked container was not created");

    fixture.start();

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut gone = false;
    while Instant::now() < deadline {
        if !leaked.exists() {
            gone = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    assert!(
        gone,
        "a leaked container whose tenon.base pid was dead survived a fresh `tenon start` \
         of the same home; the boot-time reap should have removed it"
    );
    leaked.removed = true;

    assert!(
        fixture.cli_node("root")["registered"] == true,
        "the real root env failed to come up while the reap ran"
    );
}
