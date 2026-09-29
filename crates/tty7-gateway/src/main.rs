use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};
use tty7_gateway::service;
use tty7_gateway::state::State;

#[derive(Parser)]
#[command(
    name = "tty7-gateway",
    version,
    about = "Reach this machine's tty7 panes from your phone"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the gateway. Paired phones can reach this machine while it runs.
    Serve {
        /// Stop when stdin closes: how a tty7 server runs its gateway.
        #[arg(long)]
        exit_with_stdin: bool,
    },
    /// Show a one-time code for the tty7 app to scan.
    Pair {
        /// How long the code stays valid, in seconds.
        #[arg(long, default_value_t = 600)]
        ttl: u64,
    },
    /// List paired phones.
    Devices,
    /// Unpair a phone, by name or by the start of its id.
    Revoke { device: String },
}

fn main() -> Result<()> {
    if log::set_logger(&STDERR).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
    let cli = Cli::parse();
    let state = State::open_default()?;
    match cli.command {
        Command::Serve {
            exit_with_stdin: true,
        } => service::serve_until_stdin_closes(state),
        Command::Serve {
            exit_with_stdin: false,
        } => {
            let _running = service::start(state)?;
            // The gateway runs on its own thread until the process is killed.
            loop {
                std::thread::park();
            }
        }
        Command::Pair { ttl } => pair(&state, ttl),
        Command::Devices => {
            let devices = state.devices()?;
            if devices.is_empty() {
                println!("no phones paired — `tty7-gateway pair` makes a code to scan");
            }
            for d in devices {
                println!("{}  {}", &d.id[..d.id.len().min(12)], d.name);
            }
            Ok(())
        }
        Command::Revoke { device } => {
            let gone = state.revoke(&device)?;
            if gone.is_empty() {
                anyhow::bail!("no paired phone matches '{device}'");
            }
            for d in gone {
                println!("unpaired {}", d.name);
            }
            Ok(())
        }
    }
}

fn pair(state: &State, ttl: u64) -> Result<()> {
    let code = service::pair_code(state, Duration::from_secs(ttl))?.code;
    let qr = qrcode::QrCode::new(code.as_bytes()).context("drawing the pairing code")?;
    let art = qr
        .render::<qrcode::render::unicode::Dense1x2>()
        .quiet_zone(true)
        .build();
    println!("{art}");
    println!("Scan with the tty7 app, or paste this code into it:\n\n{code}\n");
    println!(
        "Valid for {} minutes, for one phone. The gateway must be running: \
         `tty7-gateway serve`, or Mobile access switched on in tty7's Settings.",
        ttl / 60
    );
    Ok(())
}

/// The gateway logs through `log`, which the desktop's daemon sends to its log
/// file; run on its own, that goes to stderr.
struct Stderr;

static STDERR: Stderr = Stderr;

impl log::Log for Stderr {
    fn enabled(&self, meta: &log::Metadata) -> bool {
        meta.level() <= log::max_level()
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) && record.target().starts_with("tty7_gateway") {
            eprintln!("tty7-gateway: {}", record.args());
        }
    }

    fn flush(&self) {}
}
