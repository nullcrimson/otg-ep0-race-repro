//! One command per failure case:
//!
//! ```text
//! cargo xtask stream [--fixed] [--no-load] [--rounds N] [--drive-only]
//! cargo xtask reopen [--fixed] [--no-load] [--rounds N] [--drive-only]
//! ```
//!
//! Each command builds and flashes the firmware, resets the board, drives the device's serial
//! port with the case's traffic until a round stalls, and then asks the probe whether the core
//! panicked or is still running.
//!
//! - `board.rs`: build, flash, reset, read the core state (through the `probe-rs` CLI)
//! - `port.rs`: find and open the device's serial port
//! - `cases.rs`: the traffic of each case
//! - `watchdog.rs`: run the rounds on a worker thread and notice a stalled one

mod board;
mod cases;
mod port;
mod watchdog;

use std::process::ExitCode;

use anyhow::{Context, Result, bail};

use board::{CoreState, Embassy};
use cases::Case;
use watchdog::Outcome;

const USAGE: &str = "\
usage: cargo xtask <case> [--fixed] [--no-load] [--rounds N] [--drive-only]

cases:
  stream  keep the port open and stream data, changing the baud rate and DTR between blocks
  reopen  open the port, echo 4 bytes, close it, in a loop

Against embassy main both fail within a few rounds; with --fixed or --no-load every round
passes.

options:
  --fixed       build against embassy main plus the proposed fix (`firmware-fixed/`) instead of
                embassy main (`firmware/`): expected to complete every round
  --no-load     build the firmware without its interrupt-level load (the control run:
                expected to complete every round)
  --rounds N    stop after N rounds if nothing has failed (N >= 1)
  --drive-only  only drive the serial port: do not build, flash or reset, and do not read the
                core state. For use while `cargo run --release` in `firmware/` holds the probe
                and prints the firmware's log

exit code: 0 every round passed, 1 a round failed, 2 the tool itself failed";

const EXIT_ROUND_FAILED: u8 = 1;
const EXIT_TOOL_ERROR: u8 = 2;

struct Args {
    case: Case,
    embassy: Embassy,
    with_load: bool,
    rounds: u32,
    drive_only: bool,
}

enum Command {
    Help,
    Run(Args),
}

fn parse_args() -> Result<Command> {
    let mut args = std::env::args().skip(1);
    let case = match args.next().as_deref() {
        None => bail!("no case given"),
        Some("-h" | "--help") => return Ok(Command::Help),
        Some(name) => Case::from_name(name).with_context(|| format!("unknown case `{name}`"))?,
    };
    let mut parsed = Args {
        case,
        embassy: Embassy::Main,
        with_load: true,
        rounds: case.default_rounds(),
        drive_only: false,
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "--fixed" => parsed.embassy = Embassy::Fixed,
            "--no-load" => parsed.with_load = false,
            "--drive-only" => parsed.drive_only = true,
            "--rounds" => {
                let n = args.next().context("--rounds needs a number")?;
                parsed.rounds = n
                    .parse()
                    .ok()
                    .filter(|&n| n >= 1)
                    .with_context(|| format!("--rounds needs a number >= 1, got `{n}`"))?;
            }
            other => bail!("unknown argument `{other}`"),
        }
    }
    let changes_build = !parsed.with_load || matches!(parsed.embassy, Embassy::Fixed);
    if parsed.drive_only && changes_build {
        bail!("--fixed and --no-load change the firmware build, which --drive-only skips");
    }
    Ok(Command::Run(parsed))
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Command::Run(args)) => args,
        Ok(Command::Help) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return ExitCode::from(EXIT_TOOL_ERROR);
        }
    };
    match run(&args) {
        Ok(Outcome::Completed) => ExitCode::SUCCESS,
        Ok(_) => ExitCode::from(EXIT_ROUND_FAILED),
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(EXIT_TOOL_ERROR)
        }
    }
}

fn run(args: &Args) -> Result<Outcome> {
    if args.drive_only {
        println!("case: {}, driving the port only", args.case.name());
    } else {
        let load = if args.with_load {
            "with load"
        } else {
            "without load (control run)"
        };
        println!(
            "case: {}, {}, firmware {load}",
            args.case.name(),
            args.embassy.describe()
        );
        board::build_and_flash(args.embassy, args.with_load)?;
        board::reset()?;
    }
    let port = port::find_device()?;
    println!("device on {port}");

    let case = args.case;
    let outcome = watchdog::run_rounds(args.rounds, move |counter| case.drive(&port, counter));

    let stall_secs = watchdog::STALL.as_secs();
    match &outcome {
        Outcome::Completed => {
            println!("result: {} rounds, no failure", args.rounds);
            return Ok(outcome);
        }
        Outcome::Stalled { round: 0 } => {
            println!("no progress for {stall_secs} s before the first round (opening the port)");
        }
        Outcome::Stalled { round } => {
            println!("round {round}: no progress for {stall_secs} s");
        }
        Outcome::Failed { round, error } => {
            println!("round {round}: {error:#}");
        }
    }

    if args.drive_only {
        println!("result: see the probe-rs terminal");
        return Ok(outcome);
    }
    let usb_stalled = matches!(outcome, Outcome::Stalled { .. });
    match board::core_state()? {
        CoreState::HardFault { icsr } => {
            println!("result: PANIC (core in HardFault, ICSR {icsr:08x})");
        }
        CoreState::Running { icsr } if usb_stalled => {
            println!("result: HANG (USB not answering, core running, ICSR {icsr:08x})");
        }
        CoreState::Running { icsr } => {
            println!("result: FAILED (a serial call failed, above; core running, ICSR {icsr:08x})");
        }
    }
    Ok(outcome)
}
