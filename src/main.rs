mod agent_prompt;
mod bin_codec;
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
mod search;

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
