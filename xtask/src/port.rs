//! Finding and opening the device's CDC ACM serial port.

use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serialport::{SerialPort, SerialPortType};

/// The VID:PID the firmware enumerates with.
const VID: u16 = 0xC0DE;
const PID: u16 = 0xCAFE;

pub const BAUD: u32 = 115_200;
const READ_TIMEOUT: Duration = Duration::from_millis(200);
const ENUMERATION_TIMEOUT: Duration = Duration::from_secs(30);
const SETTLE: Duration = Duration::from_secs(1);

/// Waits for the device to enumerate after a reset and returns its port name.
pub fn find_device() -> Result<String> {
    let deadline = Instant::now() + ENUMERATION_TIMEOUT;
    while Instant::now() < deadline {
        let ports = serialport::available_ports().context("could not list serial ports")?;
        let device = ports.into_iter().find(|port| match &port.port_type {
            SerialPortType::UsbPort(usb) => usb.vid == VID && usb.pid == PID,
            _ => false,
        });
        if let Some(device) = device {
            thread::sleep(SETTLE);
            return Ok(device.port_name);
        }
        thread::sleep(Duration::from_millis(500));
    }
    bail!("no device with VID:PID {VID:04X}:{PID:04X}; is the board's user USB port connected?")
}

/// Opens the port and raises DTR: the firmware echoes only while DTR is set.
pub fn open(name: &str) -> Result<Box<dyn SerialPort>> {
    let mut port = serialport::new(name, BAUD)
        .timeout(READ_TIMEOUT)
        .open()
        .with_context(|| format!("could not open {name}"))?;
    port.write_data_terminal_ready(true)
        .context("could not set DTR")?;
    Ok(port)
}
