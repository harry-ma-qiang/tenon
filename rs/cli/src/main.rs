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
    },
    /// Stop every environment, then the guardian, then the base
    Stop,
    /// Restart one environment from its last known good profile
    Reset {
        #[arg(long, value_name = "NAME")]
        env: Option<String>,
    },
    /// One JSON document with every node, its environment and its fiber tree
    Status,
    /// Agent loop, llm adapter and session log, one per environment
    Harness {
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Resident tool process inside the sandbox
    Worker {
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
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
        Command::Harness { args } => Ok(tenon_harness::run(&args)),
        Command::Worker { args } => Ok(tenon_worker::run(&args)),
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
        Command::Attach { env } => tenon_base::attach(home, env).await,
        Command::Stop => tenon_base::rpc(home, "stop", json!({})).await,
        Command::Reset { env } => {
            let params = env.map(|env| json!({ "env": env })).unwrap_or(json!({}));
            tenon_base::rpc(home, "reset", params).await
        }
        Command::Status => tenon_base::rpc(home, "status", json!({})).await,
        Command::Harness { .. } | Command::Worker { .. } => unreachable!("handled before"),
    }
}
