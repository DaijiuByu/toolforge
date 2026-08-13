use clap::{Parser, Subcommand};
use std::io;
use std::path::PathBuf;
use toolforge::{serve_jsonl, Executor, Policy};

#[derive(Debug, Parser)]
#[command(
    name = "toolforge",
    version,
    about = "A policy-controlled JSONL harness for coding agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Read JSON tool requests from stdin and write JSON responses to stdout.
    Serve {
        /// Directory exposed to tools and used as the process working directory.
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
        /// Optional JSONL audit log path.
        #[arg(long)]
        audit: Option<PathBuf>,
        /// Maximum number of tool calls in this process.
        #[arg(long, default_value_t = 24)]
        max_calls: usize,
        /// Maximum runtime for one test command, in milliseconds.
        #[arg(long, default_value_t = 30_000)]
        max_runtime_ms: u64,
    },
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve {
            workspace,
            audit,
            max_calls,
            max_runtime_ms,
        } => {
            let mut policy = Policy::new(workspace)?;
            policy.max_calls = max_calls;
            policy.max_runtime = std::time::Duration::from_millis(max_runtime_ms);
            let mut executor = Executor::new(policy, audit.as_deref())?;
            serve_jsonl(io::stdin(), io::stdout(), &mut executor)
        }
    }
}
