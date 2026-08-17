//! `spg-oracle-runner` — v7.38 元机制 C 差分 oracle.
//!
//! Three-master differential test harness. Runs the same SQL on
//! SPG (embedded path) and on a reference master (PG18 / MySQL 8 /
//! MariaDB 11), normalises both result sets through `adjust_*`,
//! sorts, and asserts byte-equal.
//!
//! Scaffolding stage (v7.38 C): the CLI is wired and the
//! orchestrator dispatches to the right adapter, but SPG / sqlx
//! execution hooks are stubs marked `todo!()`. Filling them is the
//! first task of v7.38 P1 (see
//! `.claude/notes/v7.38-differential-oracle-design.md` §11).

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod dialect;
mod docker;
mod naming;
mod normalise;
mod runner;
mod self_diff;

#[derive(Parser, Debug)]
#[command(
    name = "spg-oracle-runner",
    about = "v7.38 differential oracle — SPG vs PG18 / MySQL 8 / MariaDB 11"
)]
struct Cli {
    /// Override corpus root (default `xtests/oracle/sql`).
    #[arg(long, default_value = "xtests/oracle/sql")]
    corpus: PathBuf,

    /// Override expected/ root (default `xtests/oracle/expected`).
    #[arg(long, default_value = "xtests/oracle/expected")]
    expected: PathBuf,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// `docker up` — bring up the three reference masters.
    Docker {
        #[command(subcommand)]
        action: DockerAction,
    },
    /// List corpus fixtures grouped by port. / orig. / depd. prefix.
    List,
    /// Run the corpus against a single oracle.
    Run {
        /// Which oracle to differential against.
        #[arg(long, value_enum)]
        oracle: dialect::Oracle,
        /// Capture the live oracle's output as the expected baseline
        /// instead of comparing (writes `expected/<stem>.<suffix>.out`).
        #[arg(long)]
        bless: bool,
    },
    /// Run the corpus against all three oracles.
    All {
        /// Capture baselines on every leg instead of comparing.
        #[arg(long)]
        bless: bool,
    },
    /// fast-tier replacement: SPG-self differential, no docker.
    ///
    /// Same fixtures, run on embedded / server_simple / server_extended
    /// permutations; any byte-level divergence = fail. Stands in for
    /// the docker path when the 60s fast-tier budget can't absorb
    /// container startup.
    SelfDiff,
    /// Dump the canonicalised SPG output for a single fixture.
    /// v7.38 P1 helper for filling EXPECTED FAILURE locks and
    /// bisecting normalisation pipeline divergences.
    Dump {
        /// Path to the fixture .sql file.
        fixture: std::path::PathBuf,
    },
    /// Print a recap of the last run.
    Report,
}

#[derive(Subcommand, Debug)]
enum DockerAction {
    Up,
    Down,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.cmd {
        Cmd::Docker { action } => match action {
            DockerAction::Up => docker::up().context("docker up"),
            DockerAction::Down => docker::down().context("docker down"),
        },
        Cmd::List => {
            let grouped = naming::list(&cli.corpus)?;
            for (kind, files) in grouped {
                println!("# {kind:?} ({} fixtures)", files.len());
                for f in files {
                    println!("  {}", f.display());
                }
            }
            Ok(())
        }
        Cmd::Run { oracle, bless } => runner::run_all(&cli.corpus, &cli.expected, oracle, bless),
        Cmd::All { bless } => {
            for oracle in [
                dialect::Oracle::Pg18,
                dialect::Oracle::Mysql,
                dialect::Oracle::Mariadb,
            ] {
                runner::run_all(&cli.corpus, &cli.expected, oracle, bless)
                    .with_context(|| format!("oracle {oracle:?}"))?;
            }
            Ok(())
        }
        Cmd::SelfDiff => self_diff::run(&cli.corpus),
        Cmd::Dump { fixture } => {
            let raw = runner::dump_spg(&fixture)?;
            print!("{raw}");
            Ok(())
        }
        Cmd::Report => Err(anyhow!(
            "report: not implemented in v7.38 C scaffolding — adopt during P1 fill"
        )),
    }
}
