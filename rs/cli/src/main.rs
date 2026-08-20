use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::json;
use std::path::PathBuf;
use tenon_base::StartOpts;

mod payload {
    include!(concat!(env!("OUT_DIR"), "/payload.rs"));
}

#[derive(Parser)]
#[command(name = "tenon", version, about = "Tenon barebone and roles")]
struct Cli {
    #[arg(long, global = true, value_name = "DIR")]
    home: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Boot the barebone: the guardian node, the root environment and the front door
    Start {
        #[arg(long)]
        foreground: bool,
        #[arg(long)]
        exit_on_detach: bool,
        #[arg(long, value_name = "DIR")]
        release_dir: Option<PathBuf>,
    },
    /// Print the status and stream the event log until Ctrl-C
    Attach {
        #[arg(long, value_name = "NAME")]
        env: Option<String>,
        /// Render the built-in ASCII UI instead of streaming raw events
        #[arg(long)]
        ui: bool,
    },
    /// List the approval queue
    Approvals {
        /// pending (default), approved, denied, expired or all
        #[arg(long, default_value = "pending", value_name = "STATUS")]
        status: String,
    },
    /// Answer one pending approval
    Approve {
        id: i64,
        /// Refuse instead of approving
        #[arg(long)]
        deny: bool,
        #[arg(long, value_name = "TEXT")]
        note: Option<String>,
    },
    /// Serve the built-in ASCII UI (and the WebSocket carrier) as a localhost page
    #[cfg(feature = "http")]
    Serve {
        /// The address to bind, loopback only
        #[arg(long = "http", default_value = "127.0.0.1:8791", value_name = "ADDR")]
        http: String,
        #[arg(long, value_name = "NAME")]
        env: Option<String>,
        /// Terminate TLS with rustls; without --cert/--key a dev self-signed cert is minted
        #[arg(long)]
        https: bool,
        /// PEM certificate chain for --https
        #[arg(long, value_name = "PEM")]
        cert: Option<std::path::PathBuf>,
        /// PEM private key for --https
        #[arg(long, value_name = "PEM")]
        key: Option<std::path::PathBuf>,
        /// Bearer token every request must carry; also read from TENON_AUTH_TOKEN
        #[arg(long, value_name = "TOKEN")]
        auth_token: Option<String>,
        /// Skip the bearer check for this serve surface (ingress is P4.5)
        #[arg(long)]
        public: bool,
        /// Leave the WebSocket carrier unscoped (base/barebone cross-env access);
        /// by default every WS connection is bound to this serve's env
        #[arg(long)]
        admin: bool,
    },
    /// List the `/app/<name>` ingress routes base is serving (RFC 8c, P4.5)
    #[cfg(feature = "http")]
    Ingress {
        /// Only the routes of this env; every env by default
        #[arg(long, value_name = "NAME")]
        env: Option<String>,
    },
    /// Stop every environment, then the guardian, then the base
    Stop {
        /// Also sweep this home's stale sandbox containers whose base is dead
        #[arg(long)]
        all: bool,
    },
    /// Restart one environment from its last known good profile
    Reset {
        #[arg(long, value_name = "NAME")]
        env: Option<String>,
    },
    /// One JSON document with every node, its environment and its fiber tree
    Status {
        /// Print what the last LKG promotion pinned, and verify it, instead
        #[arg(long)]
        lkg: bool,
    },
    /// Write (and on Linux enable) the OS service unit for this binary and home
    InstallService {
        /// The only supported scope: a user unit, never a system one
        #[arg(long)]
        user: bool,
        /// Print the unit instead of writing it
        #[arg(long)]
        print: bool,
    },
    /// Restore the last known good config, profiles and state copy
    Rollback {
        /// Restore even though the LKG manifest does not match what is on disk
        #[arg(long)]
        force: bool,
    },
    /// Drive the change protocol: propose an upgrade, or read what happened
    Upgrade {
        #[command(subcommand)]
        command: UpgradeCommand,
    },
    /// Run a contract suite against an artifact before it is trusted
    Check {
        #[command(subcommand)]
        command: CheckCommand,
    },
    /// Sandbox backend maintenance for humans
    Sandbox {
        #[command(subcommand)]
        command: SandboxCommand,
    },
    /// Give the environment's agent one task and stream its answer
    Run {
        /// What the agent should do
        task: String,
        #[arg(long, value_name = "NAME")]
        env: Option<String>,
        /// How long to wait for the turn to end
        #[arg(long, default_value_t = 600, value_name = "SECONDS")]
        timeout: u64,
    },
    /// Agent loop, llm adapter and session log, one per environment
    Harness {
        /// The environment this harness serves; defaults to $TENON_ENV
        #[arg(long, value_name = "NAME")]
        env: Option<String>,
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Expose this env's tools bus as an MCP server over stdio (JSON-RPC 2.0)
    Mcp {
        /// The environment whose tools to expose; defaults to the root env
        #[arg(long, value_name = "NAME")]
        env: Option<String>,
    },
    /// Resident tool process inside the sandbox
    Worker {
        /// The workspace it serves; defaults to $TENON_WORKSPACE, then /workspace
        #[arg(long, value_name = "DIR")]
        workspace: Option<PathBuf>,
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
}

#[derive(Subcommand)]
enum UpgradeCommand {
    /// Propose a change: plugin, worker, kernel or config
    Propose {
        /// plugin | worker | kernel | config
        target: String,
        /// The artifact as JSON, the same object the tool takes
        #[arg(long, value_name = "JSON")]
        artifact: String,
        #[arg(long, value_name = "NAME")]
        env: Option<String>,
        #[arg(long, default_value = "", value_name = "TEXT")]
        notes: String,
    },
    /// What one proposal did, phase by phase
    Status { id: i64 },
    /// Every proposal and every benchmark row
    List {
        #[arg(long, value_name = "NAME")]
        env: Option<String>,
    },
}

#[derive(Subcommand)]
enum CheckCommand {
    /// Run the kernel contract suite shipped in the beam release against a
    /// `tenon.beam`; without --beam it checks the one the release ships
    Kernel {
        #[arg(long, value_name = "PATH")]
        beam: Option<PathBuf>,
        #[arg(long, value_name = "DIR")]
        release_dir: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum SandboxCommand {
    /// Remove stale containers for this home; works whether or not base is up.
    /// Without --all, only containers whose recorded base pid is dead are
    /// touched; with it, every container for this home goes regardless.
    Reap {
        #[arg(long)]
        all: bool,
    },
    /// Unpack an OCI image into <home>/images/<name>/rootfs, the root
    /// filesystem a microVM boots from. podman, docker or skopeo + umoci.
    Image {
        #[command(subcommand)]
        command: ImageCommand,
    },
    /// One microVM, in its own process: the krun backend re-execs this.
    /// libkrun takes over the process and exits it with the guest's status.
    Vmm {
        #[arg(long, value_name = "FILE")]
        config: PathBuf,
    },
}

#[derive(Subcommand)]
enum ImageCommand {
    /// Pull and unpack an image reference, e.g. `alpine:3.20`
    Pull {
        /// The image reference an engine can resolve
        reference: String,
        /// The name it is stored and referred to under; defaults to `default`
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
    },
}

fn main() {
    let code = match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("tenon: {error:#}");
            1
        }
    };
    std::process::exit(code);
}

fn run() -> Result<i32> {
    let cli = Cli::parse();
    match cli.command {
        Command::Harness { env, args } => {
            let mut argv = Vec::new();
            if let Some(env) = env {
                argv.push("--env".to_string());
                argv.push(env);
            }
            argv.extend(args);
            Ok(tenon_harness::run(&argv))
        }
        Command::Worker { workspace, args } => {
            let mut argv = Vec::new();
            if let Some(dir) = workspace {
                argv.push("--workspace".to_string());
                argv.push(dir.display().to_string());
            }
            argv.extend(args);
            Ok(tenon_worker::run(&argv))
        }
        Command::Sandbox {
            command: SandboxCommand::Vmm { config },
        } => tenon_sandbox::krun::vmm::main(&tenon_sandbox::krun::vmm::read(&config)?),
        command => runtime()?.block_on(dispatch(cli.home, command)),
    }
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?)
}

async fn dispatch(home: Option<PathBuf>, command: Command) -> Result<i32> {
    match command {
        Command::Start {
            foreground,
            exit_on_detach,
            release_dir,
        } => {
            tenon_base::start(StartOpts {
                home,
                release_dir,
                foreground,
                exit_on_detach,
                payload: payload::PAYLOAD,
                version: payload::VERSION,
            })
            .await
        }
        Command::Attach { env, ui } => match ui {
            true => tenon_base::tui::attach(home, env).await,
            false => tenon_base::attach(home, env).await,
        },
        Command::Approvals { status } => tenon_base::approvals(home, Some(status)).await,
        Command::Approve { id, deny, note } => tenon_base::approve(home, id, deny, note).await,
        #[cfg(feature = "http")]
        Command::Serve {
            http,
            env,
            https,
            cert,
            key,
            auth_token,
            public,
            admin,
        } => {
            let config = tenon_base::http::ServeConfig {
                https,
                cert,
                key,
                auth: tenon_base::auth::Auth::resolve(auth_token, public),
                admin,
            };
            tenon_base::http::serve(home, env, http, config).await
        }
        #[cfg(feature = "http")]
        Command::Ingress { env } => {
            let params = env.map(|env| json!({ "env": env })).unwrap_or(json!({}));
            tenon_base::rpc(home, "ingress.list", params).await
        }
        Command::Stop { all } => {
            let code = tenon_base::rpc(home.clone(), "stop", json!({})).await?;
            if all {
                tenon_base::sandbox_reap(home, false).await?;
            }
            Ok(code)
        }
        Command::Reset { env } => {
            let params = env.map(|env| json!({ "env": env })).unwrap_or(json!({}));
            tenon_base::rpc(home, "reset", params).await
        }
        Command::Status { lkg } => match lkg {
            true => tenon_base::lkg_status(home),
            false => tenon_base::rpc(home, "status", json!({})).await,
        },
        Command::Rollback { force } => tenon_base::rollback(home, force).await,
        Command::Upgrade { command } => match command {
            UpgradeCommand::Propose {
                target,
                artifact,
                env,
                notes,
            } => {
                let artifact: serde_json::Value = serde_json::from_str(&artifact)?;
                let mut params = json!({"target": target, "artifact": artifact, "notes": notes});
                if let Some(env) = env {
                    params["env"] = json!(env);
                }
                tenon_base::rpc(home, "upgrade.propose", params).await
            }
            UpgradeCommand::Status { id } => {
                tenon_base::rpc(home, "upgrade.status", json!({ "upgrade_id": id })).await
            }
            UpgradeCommand::List { env } => {
                let params = env.map(|env| json!({ "env": env })).unwrap_or(json!({}));
                tenon_base::rpc(home, "upgrade.list", params).await
            }
        },
        Command::Check {
            command: CheckCommand::Kernel { beam, release_dir },
        } => {
            tenon_base::check::command(home, beam, release_dir, payload::PAYLOAD, payload::VERSION)
        }
        Command::InstallService { user, print } => match user || print {
            true => tenon_base::service::install(home, print),
            false => {
                eprintln!("tenon: install-service needs --user (system units are not written)");
                Ok(1)
            }
        },
        Command::Run { task, env, timeout } => {
            tenon_base::run::task(home, env, task, std::time::Duration::from_secs(timeout)).await
        }
        Command::Mcp { env } => tenon_base::mcp::stdio(home, env).await,
        Command::Sandbox { command } => match command {
            SandboxCommand::Reap { all } => tenon_base::sandbox_reap(home, all).await,
            SandboxCommand::Image {
                command: ImageCommand::Pull { reference, name },
            } => tenon_base::image_pull(home, &reference, name.as_deref()).await,
            SandboxCommand::Vmm { .. } => unreachable!("handled before"),
        },
        Command::Harness { .. } | Command::Worker { .. } => unreachable!("handled before"),
    }
}
