//! The board side: build and flash the firmware, reset it, and read the core's state. Everything
//! goes through the `probe-rs` command-line tool.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// The same chip name as the runner in `firmware/.cargo/config.toml`.
const CHIP: &str = "STM32F429ZITx";

/// Interrupt Control and State Register. Bits 8:0 (VECTACTIVE) hold the exception the core is
/// executing right now; 3 is HardFault, where `panic-probe` ends up after printing a panic.
const ICSR_ADDRESS: &str = "0xE000ED04";
const VECTACTIVE_MASK: u32 = 0x1FF;
const HARD_FAULT: u32 = 3;

/// Which embassy the firmware is built against. Both build the same `firmware/src/main.rs`.
#[derive(Clone, Copy)]
pub enum Embassy {
    /// `firmware/`: embassy main.
    Main,
    /// `firmware-fixed/`: embassy main plus the proposed fix.
    Fixed,
}

impl Embassy {
    pub fn describe(self) -> &'static str {
        match self {
            Embassy::Main => "embassy main",
            Embassy::Fixed => "embassy main + the fix",
        }
    }

    fn firmware_dir(self) -> PathBuf {
        let dir = match self {
            Embassy::Main => "firmware",
            Embassy::Fixed => "firmware-fixed",
        };
        Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(dir)
    }
}

pub enum CoreState {
    HardFault { icsr: u32 },
    Running { icsr: u32 },
}

/// Each firmware directory is its own Cargo project, built from its own directory so its
/// `.cargo/config.toml` (target, linker scripts) applies. The target directory is passed
/// explicitly so that a `CARGO_TARGET_DIR` in the environment cannot send the ELF elsewhere.
pub fn build_and_flash(embassy: Embassy, with_load: bool) -> Result<()> {
    let dir = embassy.firmware_dir();
    let target_dir = dir.join("target");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());

    println!("building firmware");
    let mut build = Command::new(cargo);
    build
        .current_dir(&dir)
        .args(["build", "--release", "--target-dir"])
        .arg(&target_dir);
    if !with_load {
        build.args(["--features", "no-load"]);
    }
    run_visible(&mut build)?;

    println!("flashing");
    let elf = target_dir.join("thumbv7em-none-eabihf/release/otg-ep0-race-repro");
    run_visible(
        Command::new("probe-rs")
            .arg("download")
            .arg(&elf)
            .args(["--chip", CHIP]),
    )
}

pub fn reset() -> Result<()> {
    run_visible(Command::new("probe-rs").args(["reset", "--chip", CHIP]))
}

/// Reading memory through the debug port does not halt the core.
pub fn core_state() -> Result<CoreState> {
    let output = Command::new("probe-rs")
        .args(["read", "b32", ICSR_ADDRESS, "1", "--chip", CHIP])
        .output()
        .context("could not start probe-rs")?;
    if !output.status.success() {
        bail!(
            "probe-rs read failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let word = stdout
        .split_whitespace()
        .last()
        .context("probe-rs read printed nothing")?;
    let icsr = u32::from_str_radix(word, 16).with_context(|| format!("bad ICSR `{word}`"))?;
    Ok(if icsr & VECTACTIVE_MASK == HARD_FAULT {
        CoreState::HardFault { icsr }
    } else {
        CoreState::Running { icsr }
    })
}

/// Runs a command with its output going to this terminal (build and flash progress).
fn run_visible(command: &mut Command) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("could not start {command:?}"))?;
    if !status.success() {
        bail!("{command:?} failed ({status})");
    }
    Ok(())
}
