mod cli;
mod db;
mod nix;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "lethe", version, about = "Record and inspect NixOS deployments")]
struct Cli {
    #[arg(
        long,
        global = true,
        env = "LETHE_DB",
        value_name = "PATH",
        help = "Path to the SQLite database [default: $XDG_DATA_HOME/lethe/lethe.db]"
    )]
    db: Option<PathBuf>,

    #[arg(long, global = true, help = "Disable colored output (NO_COLOR is also respected)")]
    no_color: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Record a new deployment for a machine
    Record {
        /// SSH target: [user@]host[:port] or ssh[-ng]://[user@]host[:port].
        /// Uses ssh-ng unless ssh:// is given [default: this machine]
        target: Option<String>,

        /// Machine identifier to record under [default: the target's short hostname]
        #[arg(long)]
        name: Option<String>,

        /// System to record: any path that resolves into the store, such as
        /// /run/booted-system or /nix/var/nix/profiles/system-192-link
        #[arg(long, value_name = "PATH", default_value = "/run/current-system")]
        system_link: String,
    },

    /// List all known machines
    Machines,

    /// List deployments for a machine
    Deployments {
        /// Machine identifier
        machine: String,
    },

    /// Show one deployment of a machine
    Show {
        /// Machine identifier
        machine: String,

        /// Deployment number or toplevel /nix/store path [default: the latest]
        deployment: Option<String>,
    },

    /// Diff two deployments of a machine
    Diff {
        /// Machine identifier. On its own, diffs its last two deployments
        machine: String,

        /// Deployment number or toplevel /nix/store path to start from
        old: Option<String>,

        /// Deployment number or toplevel /nix/store path [default: the latest]
        new: Option<String>,
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
        // dix logs a summary of every diff at info, which repeats the report.
        .unwrap_or_else(|_| EnvFilter::new("info,dix=warn"));
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

    // `TTY_AND_COLOR` covers `NO_COLOR`, `CLICOLOR`/`CLICOLOR_FORCE`, and TTY
    // detection; it is what the `detect-env`/`detect-tty` features are for.
    if cli.no_color {
        yansi::disable();
    } else {
        yansi::whenever(yansi::Condition::TTY_AND_COLOR);
    }

    let conn = db::open(cli.db)?;
    let out = &mut std::io::stdout().lock();
    match cli.command {
        Command::Record { target, name, system_link } => {
            let nix_target = match target {
                Some(raw) => nix::Target::Ssh(nix::parse_ssh_target(&raw)?),
                None => nix::Target::Local,
            };
            cli::record(&conn, out, name.as_deref(), &nix_target, &system_link)
        }
        Command::Machines => cli::machines(&conn, out),
        Command::Deployments { machine } => cli::deployments(&conn, out, &machine),
        Command::Show { machine, deployment } => {
            cli::show(&conn, out, &machine, deployment.as_deref())
        }
        Command::Diff { machine, old, new } => {
            cli::diff(&conn, out, &machine, old.as_deref(), new.as_deref())
        }
        Command::Completions { .. } => unreachable!(),
    }
}
