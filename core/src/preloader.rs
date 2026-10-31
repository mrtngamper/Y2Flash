//! Commands understood by the BootROM / preloader before any download agent runs.
//! Ported from mtkclient (`Library/mtk_preloader.py`).

use crate::link::Link;
use anyhow::{bail, Result};
use std::time::{Duration, Instant};

const GET_HW_CODE: u8 = 0xFD;
const GET_HW_SW_VER: u8 = 0xFC;
const GET_BL_VER: u8 = 0xFE;
const GET_VERSION: u8 = 0xFF;
const GET_TARGET_CONFIG: u8 = 0xD8;
const WRITE32: u8 = 0xD4;
const SEND_DA: u8 = 0xD7;
const JUMP_DA: u8 = 0xD5;

/// What the BootROM / preloader reports about the chip.
#[derive(Debug, Clone, Default)]
pub struct Chip {
    /// Chip ID, e.g. 0x6582.
    pub hw_code: u16,
    pub hw_sub_code: u16,
    pub hw_ver: u16,
    pub sw_ver: u16,
    /// Preloader version (0xFE in BootROM mode).
    pub bl_ver: u8,
    pub brom_ver: u8,
    /// True in BootROM mode, false in preloader mode.
    pub bootrom: bool,
    /// Secure boot, serial link authentication and download agent authentication.
    pub secure_boot: bool,
    pub sla: bool,
    pub daa: bool,
}

/// Synchronise with the device. A device still in download mode from an
/// earlier session (already synchronised) is detected and used as it is.
/// Otherwise the normal handshake runs: the preloader repeats "READY" until it
/// sees 0xA0, so keep sending 0xA0 and skip everything until a reply ends in 0x5F.
///
/// # Arguments
///
/// * `link` - link to a freshly opened device
///
/// # Errors
///
/// Returns an error if the device does not answer.
pub fn handshake(link: &mut Link) -> Result<()> {
    if already_synced(link)? {
        return Ok(());
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        link.write(&[0xA0])?;
        std::thread::sleep(Duration::from_millis(5));
        if link.read_some()?.last() == Some(&0x5F) {
            break;
        }
        if Instant::now() > deadline {
            bail!(
                "the device does not answer the handshake. Unplug it, hold power for 10 s \
                 (or press the reset pinhole) and plug it in again"
            );
        }
    }
    for b in [0x0A, 0x50, 0x05] {
        link.write(&[b])?;
        link.expect(!b, "handshake")?;
    }
    Ok(())
}

/// Check whether the device already passed a handshake in an earlier session
/// and now waits for commands: it then echoes GET_HW_CODE. A device waiting
/// for the handshake ignores the byte (the preloader keeps sending "READY").
///
/// # Arguments
///
/// * `link` - link to a freshly opened device
///
/// # Returns
///
/// True if the device answered as an already synchronised one.
///
/// # Errors
///
/// Returns an error if the port fails.
fn already_synced(link: &mut Link) -> Result<bool> {
    link.write(&[GET_HW_CODE])?;
    let normal = std::mem::replace(&mut link.timeout, Duration::from_millis(150));
    let reply = link.read(1);
    link.timeout = normal;
    if !matches!(reply.as_deref(), Ok([GET_HW_CODE])) {
        return Ok(false);
    }
    link.read(4)?; // the HW code that follows; `identify` asks again
    Ok(true)
}

/// Identify the chip and stop its watchdog, in the order mtkclient does it.
///
/// # Arguments
///
/// * `link` - link after a successful handshake
///
/// # Returns
///
/// The chip information.
///
/// # Errors
///
/// Fails if the device stops answering or reports an error status.
pub fn identify(link: &mut Link) -> Result<Chip> {
    let mut chip = Chip::default();

    link.echo(&[GET_HW_CODE])?;
    chip.hw_code = (link.u32()? >> 16) as u16;

    // Without this the watchdog reboots the chip a few seconds into the session.
    if chip.hw_code == 0x6582 || chip.hw_code == 0x6572 {
        write32(link, 0x1000_7000, 0x2200_0064)?;
    }

    link.echo(&[GET_TARGET_CONFIG])?;
    let config = link.u32()?;
    status(link.u16()?, "GET_TARGET_CONFIG")?;
    chip.secure_boot = config & 1 != 0;
    chip.sla = config & 2 != 0;
    chip.daa = config & 4 != 0;

    link.write(&[GET_BL_VER])?;
    chip.bl_ver = link.u8()?;
    chip.bootrom = chip.bl_ver == GET_BL_VER;

    link.write(&[GET_VERSION])?;
    chip.brom_ver = link.u8()?;

    link.echo(&[GET_HW_SW_VER])?;
    chip.hw_sub_code = link.u16()?;
    chip.hw_ver = link.u16()?;
    chip.sw_ver = link.u16()?;
    status(link.u16()?, "GET_HW_SW_VER")?;
    Ok(chip)
}

/// Write one 32-bit value to a memory or register address.
///
/// # Arguments
///
/// * `link` - link after the handshake
/// * `addr` - target address
/// * `value` - value to write
///
/// # Errors
///
/// Returns an error if the device refuses the write.
fn write32(link: &mut Link, addr: u32, value: u32) -> Result<()> {
    link.echo(&[WRITE32])?;
    link.echo_u32(addr)?;
    link.echo_u32(1)?;
    if link.u16()? > 3 {
        bail!("WRITE32 to {addr:#x} refused");
    }
    link.echo_u32(value)?;
    status(link.u16()?, "WRITE32")
}

/// Upload code (a download agent stage or a preloader) into device memory.
///
/// # Arguments
///
/// * `link` - link after the handshake
/// * `addr` - load address
/// * `data` - the code, including its signature at the end
/// * `sig_len` - length of that signature
///
/// # Errors
///
/// Returns an error if the device refuses the upload.
pub fn send_da(link: &mut Link, addr: u32, data: &[u8], sig_len: u32) -> Result<()> {
    link.echo(&[SEND_DA])?;
    link.echo_u32(addr)?;
    link.echo_u32(data.len() as u32)?;
    link.echo_u32(sig_len)?;
    let st = link.u16()?;
    if st == 0x1D0D {
        bail!("this device requires authentication (SLA), which is not supported");
    }
    status(st, "SEND_DA")?;
    link.write(data)?;
    std::thread::sleep(Duration::from_millis(120));
    let _device_checksum = link.u16()?; // mtkclient only logs a mismatch, so do we
    status(link.u16()?, "SEND_DA upload")
}

/// Start code uploaded with `send_da`.
///
/// # Arguments
///
/// * `link` - link after the upload
/// * `addr` - entry point to jump to
///
/// # Errors
///
/// Returns an error if the device refuses the jump.
pub fn jump_da(link: &mut Link, addr: u32) -> Result<()> {
    link.echo(&[JUMP_DA])?;
    link.echo_u32(addr)?;
    match link.u16()? {
        0 => Ok(()),
        st => bail!("JUMP_DA failed with status {st:#06x}"),
    }
}

/// Where a preloader image wants to run, read from its `MMM\x01` FILE_INFO header.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RamImage {
    pub load: u32,
    pub entry: u32,
    pub sig_len: u32,
    /// Byte range of the loadable image inside the file.
    pub range: std::ops::Range<usize>,
}

/// Parse a preloader file. Some packages put an `EMMC_BOOT` header in front;
/// the loadable image then starts at 0x800 instead of 0.
///
/// # Arguments
///
/// * `file` - the preloader file's bytes
///
/// # Returns
///
/// Where and how to load the image.
///
/// # Errors
///
/// Fails if the file has no FILE_INFO header or is shorter than its header says.
fn ram_image(file: &[u8]) -> Result<RamImage> {
    for start in [0usize, 0x800] {
        let h = match file.get(start..start + 0x38) {
            Some(h) if h.starts_with(b"MMM\x01") && &h[8..17] == b"FILE_INFO" => h,
            _ => continue,
        };
        let (load, len, sig_len, jump) = (crate::le32(h, 28), crate::le32(h, 32) as usize, crate::le32(h, 44), crate::le32(h, 48));
        if start + len > file.len() || jump as usize >= len {
            bail!("preloader header describes {len:#x} bytes but the file is shorter");
        }
        return Ok(RamImage { load, entry: load + jump, sig_len, range: start..start + len });
    }
    bail!("not a MediaTek preloader image (no FILE_INFO header)")
}

/// Load a preloader into SRAM from BootROM mode and start it. It sets up DRAM
/// and re-appears as a preloader-mode USB device, independent of the eMMC.
///
/// # Arguments
///
/// * `link` - link to the device in BootROM mode
/// * `file` - the preloader file's bytes
///
/// # Errors
///
/// Returns an error if the upload or the jump fails.
pub fn boot_preloader(link: &mut Link, file: &[u8]) -> Result<()> {
    let img = ram_image(file)?;
    send_da(link, img.load, &file[img.range.clone()], img.sig_len)?;
    jump_da(link, img.entry)
}

/// Turn a status word into an error if it signals failure (above 0xFF).
///
/// # Arguments
///
/// * `code` - the status word
/// * `what` - the command, for the error message
///
/// # Errors
///
/// Returns an error for failure codes.
fn status(code: u16, what: &str) -> Result<()> {
    if code > 0xFF {
        bail!("{what} failed with status {code:#06x}");
    }
    Ok(())
}
