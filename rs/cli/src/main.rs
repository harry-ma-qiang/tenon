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
    /// Serve the built-in ASCII UI as a localhost web page
    #[cfg(feature = "http")]
    Serve {
        /// The address to bind, loopback only
        #[arg(long = "http", default_value = "127.0.0.1:8791", value_name = "ADDR")]
        http: String,
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
    Status,
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
enum SandboxCommand {
    /// Remove stale containers for this home; works whether or not base is up.
    /// Without --all, only containers whose recorded base pid is dead are
    /// touched; with it, every container for this home goes regardless.
    Reap {
        #[arg(long)]
        all: bool,
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
        Command::Serve { http, env } => tenon_base::http::serve(home, env, http).await,
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
        Command::Status => tenon_base::rpc(home, "status", json!({})).await,
        Command::Run { task, env, timeout } => {
            tenon_base::run::task(home, env, task, std::time::Duration::from_secs(timeout)).await
        }
        Command::Sandbox { command } => match command {
            SandboxCommand::Reap { all } => tenon_base::sandbox_reap(home, all).await,
        },
        Command::Harness { .. } | Command::Worker { .. } => unreachable!("handled before"),
    }
}
