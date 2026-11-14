//! The host traffic for each case.
//!
//! Both cases make the host send control transfers back to back on EP0 while the firmware's load
//! keeps the USB task late. They differ in what else is in the device's shared RX FIFO when the
//! race puts it out of step, and so in which word the driver then misreads as a status word. In
//! the traces recorded on the board with embassy-usb-synopsys-otg 0.3.3:
//!
//! - `stream`: the misread word was the bulk data, `ff 01 01 01` = 0x010101ff (endpoint 15), or
//!   the SET_LINE_CODING data, whose first word is the baud rate: 115207 = 0x0001c207
//!   (endpoint 7). Either endpoint is past the endpoint count, and the assert fired: a panic.
//! - `reopen`: the misread word was a GET_LINE_CODING request, `a1 21 00 00` = 0x000021a1, which
//!   decodes as endpoint 1 (exists) with a reserved packet status (ignored). The assert passed,
//!   the driver carried on out of step, and EP0 stopped answering: a hang.
//!
//! On embassy main both cases end with the host declaring the device not functioning instead;
//! the RX FIFO there has not been traced.
//!
//! The control transfers named below are what the Windows 11 CDC driver (usbser.sys) sends for
//! each serial call; other hosts were not tried.

use anyhow::{Context, Result, bail};
use serialport::ClearBuffer;

use crate::port::{self, BAUD};
use crate::watchdog::RoundCounter;

#[derive(Clone, Copy)]
pub enum Case {
    Stream,
    Reopen,
}

impl Case {
    pub fn from_name(name: &str) -> Option<Case> {
        match name {
            "stream" => Some(Case::Stream),
            "reopen" => Some(Case::Reopen),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Case::Stream => "stream",
            Case::Reopen => "reopen",
        }
    }

    pub fn default_rounds(self) -> u32 {
        match self {
            Case::Stream => 2000,
            Case::Reopen => 500,
        }
    }

    /// Runs rounds while `counter.next()` yields one.
    pub fn drive(self, port_name: &str, counter: &RoundCounter) -> Result<()> {
        match self {
            Case::Stream => drive_stream(port_name, counter),
            Case::Reopen => drive_reopen(port_name, counter),
        }
    }
}

/// Each round: SET_LINE_CODING (the baud rate alternates, so a change is sent every time),
/// SET_CONTROL_LINE_STATE (DTR alternates), then 512 bytes of bulk OUT data.
fn drive_stream(port_name: &str, counter: &RoundCounter) -> Result<()> {
    let block = [0xFF, 0x01, 0x01, 0x01].repeat(128);
    let mut port = port::open(port_name)?;
    while let Some(round) = counter.next() {
        let odd = round % 2 == 1;
        let baud = if odd { BAUD } else { BAUD + 7 };
        port.set_baud_rate(baud).context("set baud rate")?;
        port.write_data_terminal_ready(odd).context("set DTR")?;
        port.write_all(&block).context("write")?;
        port.clear(ClearBuffer::Input).context("discard echo")?;
    }
    Ok(())
}

/// Each round: open the port (a burst of control transfers: GET_LINE_CODING, SET_LINE_CODING,
/// SET_CONTROL_LINE_STATE), write 4 bytes, read the echo, close.
fn drive_reopen(port_name: &str, counter: &RoundCounter) -> Result<()> {
    while counter.next().is_some() {
        let mut port = port::open(port_name)?;
        port.write_all(b"ping").context("write")?;
        let mut echo = [0; 4];
        port.read_exact(&mut echo).context("read echo")?;
        if &echo != b"ping" {
            bail!("echo was {echo:02x?}, expected \"ping\"");
        }
    }
    Ok(())
}
