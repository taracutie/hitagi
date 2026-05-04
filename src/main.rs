mod cache;
mod cli;
mod commands;
mod error;
mod git;
mod lang;
mod models;
mod output;
mod parser;
mod queries;
mod repo;

use std::process::ExitCode;

fn main() -> ExitCode {
    match cli::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}
