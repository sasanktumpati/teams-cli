#![forbid(unsafe_code)]

use clap::Parser;
use std::process::ExitCode;
use teams_cli::{cli, formatters};

fn main() -> ExitCode {
    let parsed = cli::Cli::parse();
    let notice_mode = parsed.command.update_notice_mode();
    match cli::run(parsed) {
        Ok(code) => {
            match notice_mode {
                cli::UpdateNoticeMode::Full => teams_cli::update::maybe_print_update_notice(true),
                cli::UpdateNoticeMode::CheckOnly => teams_cli::update::maybe_print_update_notice(false),
                cli::UpdateNoticeMode::None => {}
            }
            ExitCode::from(code as u8)
        }
        Err(cli::CliError::Api(e)) => {
            eprintln!("teams: {}", e.message);
            if let Some(ref d) = e.details {
                formatters::print_error_details(d);
            }
            ExitCode::from(2)
        }
        Err(cli::CliError::Runtime(msg)) => {
            eprintln!("teams: {msg}");
            ExitCode::from(1)
        }
    }
}
