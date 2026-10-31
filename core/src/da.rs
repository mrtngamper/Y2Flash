//! Session with a legacy MediaTek download agent: boot it, then read and write eMMC.
//! Ported from mtkclient (`Library/DA/legacy/dalegacy_lib.py`).

use crate::link::Link;
use crate::loader::Loader;
use crate::preloader::{self, Chip};
use anyhow::{bail, Context, Result};
use std::io::Read;
use std::time::Duration;

const ACK: u8 = 0x5A;
const NACK: u8 = 0xA5;
const CONT: u8 = 0x69;
const SYNC: u8 = 0xC0;

const SWITCH_PART: u8 = 0x60;
const WRITE_DATA: u8 = 0x62;
const USB_CHECK_STATUS: u8 = 0x72;
const READ: u8 = 0xD6;
const FINISH: u8 = 0xD9;

const CHUNK: usize = 0x10_0000;

/// eMMC hardware areas, as numbered by the download agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Area {
    Boot1 = 1,
    Boot2 = 2,
    User = 8,
}

/// eMMC area sizes in bytes, as reported by the download agent.
#[derive(Debug, Clone, Default)]
pub struct Emmc {
    pub boot1: u64,
    pub boot2: u64,
    pub rpmb: u64,
    pub user: u64,
}

/// Per-chip values the stage-2 handshake needs (mtkclient `set_stage2_config`, `bmtsettings`).
struct ChipParams {
    bmt_flag: u8,
    bmt_part_size: u32,
    extra: Option<u32>,
}

/// Look up the stage-2 values for a supported chip.
///
/// # Arguments
///
/// * `hw_code` - the chip ID
///
/// # Returns
///
/// The stage-2 values, or an error for unsupported chips.
fn chip_params(hw_code: u16) -> Result<ChipParams> {
    Ok(match hw_code {
        0x6582 => ChipParams { bmt_flag: 2, bmt_part_size: 0x150_0000, extra: Some(1) }, // "newcombo"
        0x6572 => ChipParams { bmt_flag: 0, bmt_part_size: 0xA8, extra: None },
        other => bail!("chip {other:#06x} is not supported yet (supported: MT6582, MT6572)"),
    })
}

/// A running download agent, able to read and write the eMMC.
pub struct Da {
    link: Link,
    pub chip: Chip,
    pub emmc: Emmc,
}

impl Da {
    /// Upload and start both download agent stages. Expects preloader mode,
    /// where the preloader has already brought up DRAM.
    ///
    /// # Arguments
    ///
    /// * `link` - link to the device after `identify`
    /// * `chip` - the identified chip
    /// * `loader` - the agent for this chip
    /// * `log` - receives progress messages
    ///
    /// # Returns
    ///
    /// The running agent, with the eMMC sizes it reported.
    ///
    /// # Errors
    ///
    /// Fails if the chip is unsupported, an upload is refused, the device still needs DRAM setup (BootROM mode), or no eMMC is found.
    pub fn start(mut link: Link, chip: Chip, loader: &Loader, log: &mut dyn FnMut(String)) -> Result<Da> {
        let params = chip_params(chip.hw_code)?;

        log("Starting stage 1 of the download agent.".into());
        let (r1, da1) = loader.stage(1)?;
        preloader::send_da(&mut link, r1.addr, &da1, r1.sig_len)?;
        preloader::jump_da(&mut link, r1.addr)?;
        link.expect(SYNC, "download agent sync")?;

        // Stage 1 reports which storage it found.
        let nand_info = link.u32()?;
        let nand_ids = link.u16()? as usize;
        link.read(nand_ids * 2)?;
        let _emmc_info = link.u32()?;
        let emmc_id = link.read(16)?;
        if emmc_id.iter().all(|&b| b == 0) {
            bail!("download agent found no eMMC (NAND info {nand_info:#x})");
        }
        link.write(&[ACK])?;
        link.read(3)?;

        // Stage-2 configuration, field by field as mtkclient sends it for eMMC.
        let mut cfg = vec![chip.brom_ver, chip.bl_ver];
        cfg.extend(0x08u16.to_be_bytes()); // NOR chip
        cfg.push(0x00); // NOR chip select
        cfg.extend(0x7007_FFFFu32.to_be_bytes()); // NAND ACCCON
        cfg.push(params.bmt_flag);
        cfg.extend(params.bmt_part_size.to_be_bytes());
        cfg.extend([0x01, 0x01, 0x02, 0x00]); // force charge, reset keys, 26 MHz clock, MSDC boot channel
        if let Some(extra) = params.extra {
            cfg.extend(extra.to_be_bytes());
        }
        link.write(&cfg)?;
        std::thread::sleep(Duration::from_millis(350));
        match link.u32()? {
            0 => {}
            0xBC3 => bail!("the device needs DRAM setup. Connect it in preloader mode, not in BootROM mode"),
            e => bail!("stage-2 configuration refused ({e:#x})"),
        }

        log("Starting stage 2 of the download agent.".into());
        let (r2, da2) = loader.stage(2)?;
        link.write(&r2.addr.to_be_bytes())?;
        link.write(&(da2.len() as u32).to_be_bytes())?;
        link.write(&0x1000u32.to_be_bytes())?;
        link.expect(ACK, "stage 2 header")?;
        for packet in da2.chunks(0x1000) {
            link.write(packet)?;
            link.expect(ACK, "stage 2 upload")?;
        }
        std::thread::sleep(Duration::from_millis(500));
        link.write(&[ACK])?;
        link.expect(ACK, "stage 2 start")?;

        let emmc = read_flash_info(&mut link)?;
        let mut da = Da { link, chip, emmc };
        da.usb_status()?;
        Ok(da)
    }

    /// Ask the agent for its USB status (mtkclient does this before reads).
    ///
    /// # Errors
    ///
    /// Returns an error if the agent does not acknowledge.
    fn usb_status(&mut self) -> Result<()> {
        self.link.write(&[USB_CHECK_STATUS])?;
        self.link.expect(ACK, "USB status")?;
        self.link.u8()?; // 0 = full speed, 1 = high speed
        Ok(())
    }

    /// Read bytes from the eMMC.
    ///
    /// # Arguments
    ///
    /// * `area` - eMMC area to read from
    /// * `addr` - start address inside that area
    /// * `len` - number of bytes
    ///
    /// # Returns
    ///
    /// The bytes read.
    ///
    /// # Errors
    ///
    /// Fails if the agent rejects the read or stops answering.
    pub fn read(&mut self, area: Area, addr: u64, len: u64) -> Result<Vec<u8>> {
        self.usb_status()?;
        self.link.write(&[SWITCH_PART])?;
        self.link.expect(ACK, "switch partition")?;
        self.link.write(&[area as u8])?;
        self.link.expect(ACK, "switch partition")?;

        let mut cmd = vec![READ, 0x0C, 0x02]; // host Linux, storage eMMC
        cmd.extend(addr.to_be_bytes());
        cmd.extend(len.to_be_bytes());
        cmd.extend((CHUNK as u32).to_be_bytes());
        self.link.write(&cmd)?;
        self.link.expect(ACK, "read command")?;

        let mut out = Vec::with_capacity(len as usize);
        while (out.len() as u64) < len {
            let n = (len - out.len() as u64).min(CHUNK as u64) as usize;
            out.extend(self.link.read(n)?);
            self.link.u16()?; // packet checksum, not checked (like mtkclient)
            self.link.write(&[ACK])?;
        }
        Ok(out)
    }

    /// Write to the eMMC, padding the data to whole 512-byte sectors.
    ///
    /// # Arguments
    ///
    /// * `area` - eMMC area to write to
    /// * `addr` - start address inside that area
    /// * `len` - number of bytes taken from `src`
    /// * `src` - the data
    /// * `progress` - receives the bytes written so far
    ///
    /// # Errors
    ///
    /// Returns an error if the agent rejects the command or any chunk.
    pub fn write(
        &mut self,
        area: Area,
        addr: u64,
        len: u64,
        src: &mut dyn Read,
        progress: &mut dyn FnMut(u64),
    ) -> Result<()> {
        let padded = len.div_ceil(512) * 512;
        // No partition switch here: the MT6582 agent rejects writes after one.
        let mut cmd = vec![WRITE_DATA, 0x01, area as u8]; // storage eMMC
        cmd.extend(addr.to_be_bytes());
        cmd.extend(padded.to_be_bytes());
        cmd.extend((CHUNK as u32).to_be_bytes());
        self.link.write(&cmd)?;
        self.check_ack("write command")?;

        let mut buf = vec![0u8; CHUNK];
        let mut done = 0u64;
        while done < padded {
            let n = (padded - done).min(CHUNK as u64) as usize;
            let chunk = &mut buf[..n];
            fill(src, chunk, len.saturating_sub(done))?;
            let sum = chunk.iter().fold(0u16, |s, &b| s.wrapping_add(b as u16));
            self.link.write(&[ACK])?;
            self.link.write(chunk)?;
            self.link.write(&sum.to_be_bytes())?;
            match self.link.u8()? {
                CONT => {}
                NACK => bail!("device rejected data at offset {done:#x} (status {:#x})", self.status_le()?),
                b => bail!("unexpected reply {b:#04x} at offset {done:#x}"),
            }
            done += n as u64;
            progress(done.min(len));
        }
        Ok(())
    }

    /// Read the agent's ACK, turning a NACK into an error with its status.
    ///
    /// # Arguments
    ///
    /// * `what` - the command, for the error message
    ///
    /// # Errors
    ///
    /// Returns an error on a NACK or an unexpected reply.
    fn check_ack(&mut self, what: &str) -> Result<()> {
        match self.link.u8()? {
            ACK => Ok(()),
            NACK => bail!("{what} refused (status {:#x})", self.status_le()?),
            b => bail!("{what}: unexpected reply {b:#04x}"),
        }
    }

    /// Read the little-endian status that follows a NACK.
    ///
    /// # Returns
    ///
    /// The status code.
    fn status_le(&mut self) -> Result<u32> {
        Ok(crate::le32(&self.link.read(4)?, 0))
    }

    /// End the session; the device then boots normally.
    ///
    /// # Errors
    ///
    /// Returns an error if the agent does not acknowledge.
    pub fn finish(&mut self) -> Result<()> {
        self.link.write(&[FINISH])?;
        self.link.expect(ACK, "finish")?;
        self.link.write(&0u32.to_be_bytes())?; // boot mode: normal
        self.link.expect(ACK, "finish")
    }

    /// End the session if `result` is an error, so the device restarts
    /// normally, then pass the result on.
    ///
    /// # Arguments
    ///
    /// * `result` - the outcome of a step that wrote nothing
    ///
    /// # Returns
    ///
    /// `result` unchanged.
    pub fn finish_on_err<T>(&mut self, result: Result<T>) -> Result<T> {
        if result.is_err() {
            self.finish().ok();
        }
        result
    }
}

/// Fill a chunk from the source and zero-pad what the source cannot provide.
///
/// # Arguments
///
/// * `src` - the data
/// * `chunk` - buffer to fill completely
/// * `available` - bytes left in `src`
///
/// # Errors
///
/// Returns an error if `src` ends before `available` bytes.
fn fill(src: &mut dyn Read, chunk: &mut [u8], available: u64) -> Result<()> {
    let take = (chunk.len() as u64).min(available) as usize;
    src.read_exact(&mut chunk[..take]).context("image ended early")?;
    chunk[take..].fill(0);
    Ok(())
}

/// Read stage 2's storage report and keep the eMMC sizes.
///
/// # Arguments
///
/// * `link` - link right after stage 2 started
///
/// # Returns
///
/// The eMMC area sizes.
fn read_flash_info(link: &mut Link) -> Result<Emmc> {
    link.read(0x1C)?; // NOR info
    let nand = link.read(0x11)?;
    let ids64 = u16::from_be_bytes([nand[15], nand[16]]) as usize;
    if ids64 != 0 {
        link.read(ids64 * 2)?;
    } else {
        // 32-bit layout: the ID count sits earlier and its IDs begin inside `nand`.
        let ids32 = u16::from_be_bytes([nand[11], nand[12]]) as usize;
        link.read((ids32 * 2).saturating_sub(4))?;
    }
    link.read(9)?; // NAND page info
    let e = link.read(0x5C)?;
    let q = |o: usize| u64::from_be_bytes(e[o..o + 8].try_into().unwrap());
    let emmc = Emmc { boot1: q(4), boot2: q(12), rpmb: q(20), user: q(60) };
    link.read(0x1C)?; // SD card info
    link.read(0x26)?; // flash config
    let pass = link.read(0xA)?;
    if pass[0] != ACK {
        if pass[4] != ACK {
            bail!("download agent did not report ready");
        }
        link.read(1)?;
    }
    Ok(emmc)
}
