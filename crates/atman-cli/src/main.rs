use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "atman",
    version,
    about = "atman — flow-driven code agent runtime"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    Run {
        file: std::path::PathBuf,
        #[arg(long)]
        flow: Option<String>,
        args: Vec<String>,
    },
    Logs {
        #[command(subcommand)]
        action: LogsAction,
    },
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
    Cost {
        session_id: Option<String>,
    },
    Doctor,
    Version,
}

#[derive(Subcommand, Debug)]
enum LogsAction {
    Tail { session_id: String },
}

#[derive(Subcommand, Debug)]
enum SessionAction {
    List,
    Show { session_id: String },
    Gc,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        None => {
            println!(
                "atman v{} — REPL not yet available; try `atman --help`",
                env!("CARGO_PKG_VERSION")
            );
            Ok(())
        }
        Some(Cmd::Version) => {
            println!("atman v{}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(Cmd::Run { .. }) => {
            eprintln!("atman run: not yet implemented");
            std::process::exit(2);
        }
        Some(Cmd::Logs { .. }) => {
            eprintln!("atman logs: not yet implemented");
            std::process::exit(2);
        }
        Some(Cmd::Session { .. }) => {
            eprintln!("atman session: not yet implemented");
            std::process::exit(2);
        }
        Some(Cmd::Cost { .. }) => {
            eprintln!("atman cost: not yet implemented");
            std::process::exit(2);
        }
        Some(Cmd::Doctor) => {
            eprintln!("atman doctor: not yet implemented");
            std::process::exit(2);
        }
    }
}
