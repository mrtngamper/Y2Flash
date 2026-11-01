//! A device held in download mode, up to the point where the download agent runs.

use crate::da::Da;
use crate::link::Link;
use crate::preloader::{self, Chip};
use crate::{loader, port, Event};
use anyhow::{bail, Result};
use std::path::Path;
use std::time::Duration;

/// A device caught in preloader or BootROM mode. After the handshake it waits
/// for commands instead of booting on, for as long as the session is kept.
pub struct Session {
    link: Link,
    pub chip: Chip,
}

/// Wait for a device in preloader or BootROM mode and hold it there.
/// Only identification commands are sent; dropping the session releases the
/// port, and the device then waits until it is unplugged or power-cycled.
///
/// # Arguments
///
/// * `timeout` - how long to wait for the device
/// * `on` - receives log messages
///
/// # Returns
///
/// The held device.
///
/// # Errors
///
/// Fails if no device appears within `timeout` or the handshake fails.
pub fn catch(timeout: Duration, on: &mut dyn FnMut(Event)) -> Result<Session> {
    open(&[port::PID_PRELOADER, port::PID_BOOTROM], timeout, on)
}

impl Session {
    /// Run `preloader` from SRAM if the device is in BootROM mode: it sets up
    /// DRAM and comes back as a preloader-mode device, whatever is on the eMMC.
    /// Sessions already in preloader mode are returned unchanged.
    ///
    /// # Arguments
    ///
    /// * `preloader` - the image's preloader file (needed in BootROM mode)
    /// * `on` - receives log messages
    ///
    /// # Returns
    ///
    /// A session in preloader mode.
    ///
    /// # Errors
    ///
    /// Fails if no preloader is given in BootROM mode, the upload or jump is refused, or the preloader does not come back within 20 s.
    pub fn leave_bootrom(mut self, preloader: Option<&[u8]>, on: &mut dyn FnMut(Event)) -> Result<Session> {
        if !self.chip.bootrom {
            return Ok(self);
        }
        let Some(file) = preloader else {
            bail!("the device is in BootROM mode, which needs a firmware image with a preloader");
        };
        on(Event::Log("The player is in BootROM mode. Starting the image's preloader in RAM.".into()));
        preloader::boot_preloader(&mut self.link, file)?;
        drop(self); // the BootROM's USB device goes away; the preloader enumerates anew
        let session = open(&[port::PID_PRELOADER], Duration::from_secs(20), on)?;
        if session.chip.bootrom {
            bail!("the preloader did not start");
        }
        Ok(session)
    }

    /// Upload and start the download agent for this chip.
    ///
    /// # Arguments
    ///
    /// * `loaders` - folder with download agent files
    /// * `on` - receives log messages
    ///
    /// # Returns
    ///
    /// The running download agent.
    ///
    /// # Errors
    ///
    /// Fails if the device requires authentication, no loader matches the chip, or the agent does not start.
    pub fn start_agent(self, loaders: &Path, on: &mut dyn FnMut(Event)) -> Result<Da> {
        if self.chip.sla || self.chip.daa {
            bail!("the device requires authentication, which is not supported");
        }
        let loader = loader::find(loaders, &self.chip)?;
        on(Event::Log(format!("Using the loader {}.", loader.path.display())));
        let da = Da::start(self.link, self.chip, &loader)?;
        let e = &da.emmc;
        on(Event::Log(format!("The eMMC has a {} MiB user area and {} KiB boot areas.", e.user >> 20, e.boot1 >> 10)));
        Ok(da)
    }
}

/// Wait for a device, then handshake and identify the chip.
///
/// # Arguments
///
/// * `pids` - accepted USB product IDs
/// * `timeout` - how long to wait
/// * `on` - receives log messages
///
/// # Returns
///
/// The identified device.
fn open(pids: &[u16], timeout: Duration, on: &mut dyn FnMut(Event)) -> Result<Session> {
    let (io, port) = port::wait_and_open(pids, timeout)?;
    let mut link = Link::new(io);
    preloader::handshake(&mut link)?;
    let chip = preloader::identify(&mut link)?;
    on(Event::Log(format!(
        "Connected on {port}: chip {:#06x} (HW ver {:#06x}), {} mode",
        chip.hw_code,
        chip.hw_ver,
        if chip.bootrom { "BootROM" } else { "preloader" }
    )));
    Ok(Session { link, chip })
}
