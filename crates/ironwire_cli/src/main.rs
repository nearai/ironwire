//! The `ironwire` command-line interface.

mod commands;
mod render;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// IronWire — one local inference endpoint for all your AI capacity.
#[derive(Debug, Parser)]
#[command(name = "ironwire", version, about, long_about = None)]
struct Cli {
    /// Port the daemon listens on.
    #[arg(long, env = "IRONWIRE_PORT", global = true)]
    port: Option<u16>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the loopback daemon in the foreground.
    Serve,

    /// Show every connected backend and its observed capacity.
    Status {
        /// Emit JSON instead of the human-readable table.
        #[arg(long)]
        json: bool,
    },

    /// Connect a coding agent or a capacity source.
    Connect {
        /// What to connect: `claude`, `codex`, `anthropic-api`, `openai-api`, `near`.
        target: String,
        /// Enable the subscription backend for this target, after consent.
        #[arg(long)]
        subscription: bool,
        /// Print what would change without writing anything.
        #[arg(long)]
        dry_run: bool,
    },

    /// Undo a `connect`.
    Disconnect {
        /// What to disconnect.
        target: String,
        /// Revoke the subscription consent as well.
        #[arg(long)]
        subscription: bool,
    },

    /// Check every connection end to end, with a real request per backend.
    Doctor,

    /// Show recent exchanges from the local trace ledger.
    Log {
        /// How many exchanges to show, newest first.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Emit JSON instead of the human-readable table.
        #[arg(long)]
        json: bool,
    },

    /// Print the environment a client needs, for `eval "$(ironwire env)"`.
    Env,

    /// Force all traffic onto one backend, or clear the force.
    Pin {
        /// Backend id. Omit to clear.
        backend: Option<String>,
        /// Model to force.
        #[arg(long)]
        model: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("IRONWIRE_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("ironwire=info,warn")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Serve => commands::serve::run(cli.port).await,
        Command::Status { json } => commands::status::run(cli.port, json).await,
        Command::Connect {
            target,
            subscription,
            dry_run,
        } => commands::connect::run(&target, subscription, dry_run, cli.port),
        Command::Disconnect {
            target,
            subscription,
        } => commands::connect::disconnect(&target, subscription),
        Command::Doctor => commands::doctor::run(cli.port).await,
        Command::Log { limit, json } => commands::log::run(cli.port, limit, json).await,
        Command::Env => commands::connect::print_env(cli.port),
        Command::Pin { backend, model } => commands::pin::run(cli.port, backend, model).await,
    }
}
