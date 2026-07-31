mod address;
mod backend;
mod cli;
mod commands;
mod output;
mod resolve;
mod screen;
mod server;
mod stdio;
#[cfg(test)]
mod testbed;

use clap::Parser;

fn main() -> std::process::ExitCode {
    // Before the first byte goes out, and before clap can print a usage error.
    stdio::end_pipelines_quietly();
    let cli = cli::Cli::parse();
    let json = cli.json;
    let quiet = cli.quiet;
    let ctx = address::Context::from_env();
    let mut backend = backend::RealBackend::new(cli.machine.clone());
    match commands::execute(cli, &ctx, &mut backend) {
        // `run` stands in for the command it launched, so its exit code is
        // ours. The report rides along anyway: --json must not go silent just
        // because the verb also carries an exit code.
        Ok(commands::Outcome::Exit(code, report)) => {
            // No flush needed before the exit below: `stdio::out` already did.
            emit(report, json, quiet);
            std::process::exit(code)
        }
        Ok(commands::Outcome::Report(report)) => {
            emit(report, json, quiet);
            std::process::ExitCode::SUCCESS
        }
        // --quiet suppresses output on success, not the reason for a failure:
        // swallowing this leaves a bare exit code and nothing to debug.
        Err(e) => {
            eprintln!("tty7: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn emit(report: commands::Report, json: bool, quiet: bool) {
    if quiet {
        return;
    }
    if json {
        if !report.json.is_null() {
            stdio::line(&report.json.to_string());
        }
    } else if !report.human.is_empty() {
        stdio::out(ensure_newline(report.human).as_bytes());
    }
}

fn ensure_newline(mut s: String) -> String {
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s
}
