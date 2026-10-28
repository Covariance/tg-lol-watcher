//! Entrypoint: `lolwatch [command]`.

use std::process::ExitCode;

use lolwatch::cli::{self, CliEnv, Command};
use lolwatch::config::{load_config, process_env};
use lolwatch::error::Error;
use lolwatch::riot::RiotError;
use lolwatch::{app, logger};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match cli::parse_args(&args) {
        Ok(command) => command,
        Err(e) => {
            eprintln!("lolwatch: {e}\n\n{}", cli::USAGE);
            return ExitCode::from(2);
        }
    };
    match command {
        Command::Help => {
            print!("{}", cli::USAGE);
            return ExitCode::SUCCESS;
        }
        Command::Version => {
            println!("lolwatch {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        _ => {}
    }

    let bot = command == Command::Run;
    let cfg = match load_config(None, &process_env(), bot) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("lolwatch: invalid configuration:\n{e}");
            return ExitCode::from(2);
        }
    };
    logger::init(&cfg.log_level, !bot);

    if bot {
        return match app::run(cfg) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                log::error!("refusing to start: {e}");
                ExitCode::from(1)
            }
        };
    }

    let code = match command {
        Command::Smoke { riot_id, platform } => Ok(cli::smoke(&cfg, &riot_id, platform.as_deref())),
        command => {
            let env = CliEnv::real(cfg);
            match command {
                Command::Resolve { riot_id, platform } => cli::resolve(&env, &riot_id, platform.as_deref()),
                Command::Backfill { days } => cli::backfill(&env, days),
                Command::Repuuid => cli::repuuid(&env),
                Command::DryRunSummary { days } => cli::dry_run_summary(&env, days).map(|out| {
                    print!("{out}");
                    0
                }),
                _ => unreachable!("handled above"),
            }
        }
    };
    match code {
        Ok(code) => ExitCode::from(code as u8),
        Err(Error::Riot(RiotError::Auth(e))) => {
            eprintln!("Riot rejected the API key (403): {e}");
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("{}: {e}", e.kind());
            ExitCode::from(1)
        }
    }
}
