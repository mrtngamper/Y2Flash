//! Finding the player on USB and opening its serial port.

use crate::link::Port;
use anyhow::{bail, Context, Result};
use std::time::{Duration, Instant};

const MEDIATEK_VID: u16 = 0x0E8D;
pub const PID_BOOTROM: u16 = 0x0003;
pub const PID_PRELOADER: u16 = 0x2000;

/// Wait until a MediaTek serial port with one of `pids` appears and open it.
/// Polls every 20 ms: the preloader only listens for about three seconds.
/// Returns the port and its path.
///
/// # Arguments
///
/// * `pids` - accepted USB product IDs (`PID_PRELOADER`, `PID_BOOTROM`)
/// * `timeout` - how long to wait for the device
///
/// # Returns
///
/// The opened port and its path.
///
/// # Errors
///
/// Fails if no matching device appears within `timeout`, or its port cannot be opened (e.g. missing permissions).
pub fn wait_and_open(pids: &[u16], timeout: Duration) -> Result<(Box<dyn Port>, String)> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(path) = find(pids)? {
            return Ok((open(&path, deadline)?, path));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    bail!("no MediaTek device appeared within {}s", timeout.as_secs())
}

/// Find the first MediaTek serial port with one of `pids`.
///
/// # Arguments
///
/// * `pids` - accepted USB product IDs
///
/// # Returns
///
/// The port's path, or None.
fn find(pids: &[u16]) -> Result<Option<String>> {
    let ports = serialport::available_ports().context("cannot list serial ports")?;
    Ok(ports.into_iter().find_map(|p| match p.port_type {
        serialport::SerialPortType::UsbPort(u) if u.vid == MEDIATEK_VID && pids.contains(&u.pid) => {
            Some(p.port_name)
        }
        _ => None,
    }))
}

/// Open a serial port, retrying because the device node can appear a moment
/// before its permissions are applied.
///
/// # Arguments
///
/// * `path` - the port, e.g. `/dev/ttyACM0`
/// * `deadline` - when to give up
///
/// # Returns
///
/// The opened port.
fn open(path: &str, deadline: Instant) -> Result<Box<dyn Port>> {
    loop {
        match serialport::new(path, 115_200).timeout(Duration::from_millis(100)).open() {
            Ok(port) => return Ok(Box::new(port)),
            Err(e) if Instant::now() > deadline => {
                return Err(e).with_context(|| {
                    format!("cannot open {path} (on Linux: add your user to the 'dialout' group, then log out and back in)")
                })
            }
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}
