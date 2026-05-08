mod cli;
mod db;
mod diff;
mod nix;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "lethe", version, about = "Record and inspect NixOS deployments")]
struct Cli {
    /// Path to the `SQLite` database (defaults to $`XDG_DATA_HOME/lethe/lethe.db`)
    #[arg(long, global = true, env = "LETHE_DB")]
    db: Option<PathBuf>,

    /// Disable colored output (also respects `NO_COLOR` env var and CI environments)
    #[arg(long, global = true)]
    no_color: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Record a new deployment for a machine
    Record {
        /// SSH target: hostname, user@host, or ssh://[user@]host[:port].
        /// Omit when using --local.
        #[arg(required_unless_present = "local")]
        target: Option<String>,

        /// Record the local system instead of going over SSH
        #[arg(long, conflicts_with = "target")]
        local: bool,

        /// Override the stored machine identifier (defaults to `hostname -s` on the target)
        #[arg(long)]
        name: Option<String>,

        /// Path to the system to record (defaults to /run/current-system).
        /// Accepts any nix path-info argument: /run/booted-system,
        /// /nix/var/nix/profiles/system-192-link, a /nix/store path, etc.
        #[arg(long, default_value = "/run/current-system")]
        system_link: String,
    },

    /// List all known machines
    Machines,

    /// List deployments for a machine
    Deployments {
        /// Machine identifier
        machine: String,
    },

    /// Show a single deployment
    Show {
        /// Deployment id
        id: i64,
    },

    /// Diff two deployments
    Diff {
        /// Old deployment id
        old: i64,

        /// New deployment id (defaults to the latest deployment of the same machine)
        new: Option<i64>,
    },

    /// Print a shell completion script to stdout
    Completions {
        /// Target shell (bash, zsh, fish, elvish, powershell)
        shell: Shell,
    },
}

fn main() -> ExitCode {
    init_tracing();
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .with_writer(std::io::stderr)
        .init();
}

fn run(cli: Cli) -> anyhow::Result<()> {
    if let Command::Completions { shell } = cli.command {
        let mut cmd = Cli::command();
        clap_complete::generate(shell, &mut cmd, "lethe", &mut std::io::stdout());
        return Ok(());
    }

    lix_diff::color::init(cli.no_color);

    let conn = db::open(cli.db)?;
    match cli.command {
        Command::Record { target, local, name, system_link } => {
            let nix_target = if local {
                nix::Target::Local
            } else {
                let raw = target.expect("clap enforces this via required_unless_present");
                nix::Target::Ssh(nix::parse_ssh_target(&raw))
            };
            cli::record(&conn, name.as_deref(), &nix_target, &system_link)
        }
        Command::Machines => cli::machines(&conn),
        Command::Deployments { machine } => cli::deployments(&conn, &machine),
        Command::Show { id } => cli::show(&conn, id),
        Command::Diff { old, new } => cli::diff(&conn, old, new),
        Command::Completions { .. } => unreachable!(),
    }
}
