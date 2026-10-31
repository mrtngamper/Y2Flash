//! A firmware image and what it says about the device: the write plan, the
//! target chip, the preloader, and the checks against the connected device.

use crate::da::{Area, Da, Emmc};
use crate::package::Package;
use crate::preloader::Chip;
use crate::scatter::{self, Partition};
use crate::{sparse, Event, Options};
use anyhow::{bail, Context, Result};
use std::io::Read;
use std::path::Path;

/// One partition write.
#[derive(Debug, Clone)]
pub struct Step {
    pub partition: String,
    pub file: String,
    pub area: Area,
    pub addr: u64,
    /// Bytes written (expanded size for sparse images).
    pub len: u64,
    pub sparse: bool,
    /// The file is a bare preloader: an eMMC boot header is written in front
    /// of it (see [`boot_header`]) and counted in `len`.
    pub boot_header: bool,
}

/// An opened firmware image with its scatter parsed.
pub struct Image {
    pkg: Package,
    parts: Vec<Partition>,
    /// The chip the image is built for, e.g. "MT6582".
    platform: Option<String>,
    /// Where scatter address 0 of the user area lies on the device; set by [`Image::locate`].
    user_offset: u64,
}

/// User-area offsets at which [`Image::locate`] looks for the partition table:
/// none, boot1 + boot2 (8 MiB), and the Y2's 0xB80000. They must stay more
/// than 0x80000 apart, or an EBR right after the MBR could be taken for it.
const USER_OFFSETS: [u64; 3] = [0, 0x80_0000, 0xB8_0000];

impl Image {
    /// Open an image and parse its scatter.
    ///
    /// # Arguments
    ///
    /// * `path` - a folder, a `.zip`, or the scatter file itself
    /// * `scatter` - which scatter file to use if the image has several
    ///
    /// # Returns
    ///
    /// The opened image.
    ///
    /// # Errors
    ///
    /// Fails if the image cannot be opened, has no or several scatter files, or its scatter cannot be parsed.
    pub fn open(path: &Path, scatter: Option<&str>) -> Result<Image> {
        let (pkg, text) = Package::open(path, scatter)?;
        Ok(Image { pkg, parts: scatter::parse(&text)?, platform: scatter::platform(&text), user_offset: 0 })
    }

    /// Plan the writes: every partition the scatter marks downloadable and whose
    /// file the image contains, in scatter order with the preloader (if wanted) last.
    ///
    /// # Arguments
    ///
    /// * `opts` - partition filter and whether to include the preloader
    ///
    /// # Returns
    ///
    /// The writes, in order.
    ///
    /// # Errors
    ///
    /// Fails if a partition named in `opts.only` is not downloadable, a file cannot be read, or an image is larger than its partition.
    pub fn plan(&mut self, opts: &Options) -> Result<Vec<Step>> {
        for name in &opts.only {
            if !self.parts.iter().any(|p| p.name.eq_ignore_ascii_case(name) && p.download) {
                bail!("partition '{name}' is not in the image or not meant to be written");
            }
        }
        let wanted = |name: &str| opts.only.is_empty() || opts.only.iter().any(|o| o.eq_ignore_ascii_case(name));

        let mut steps = vec![];
        for p in self.parts.iter().filter(|p| p.download && wanted(&p.name)) {
            let (Some(file), Some(area)) = (&p.file, p.area) else { continue };
            if area == Area::Boot1 && !opts.with_preloader {
                continue;
            }
            let Some(file_len) = self.pkg.size(file) else { continue }; // scatters list files images may omit
            let mut head = [0u8; sparse::HEADER_LEN];
            let n = self.pkg.read(file, |r| read_up_to(r, &mut head))?;
            let expanded = sparse::expanded_size(&head[..n]);
            let boot_header = area == Area::Boot1 && needs_boot_header(&head[..n])?;
            let len = expanded.unwrap_or(file_len) + if boot_header { BOOT_HEADER_LEN } else { 0 };
            if len > p.size {
                bail!("{file} ({len} bytes) does not fit partition {} ({} bytes)", p.name, p.size);
            }
            steps.push(Step {
                partition: p.name.clone(),
                file: file.clone(),
                area,
                addr: p.addr,
                len,
                sparse: expanded.is_some(),
                boot_header,
            });
        }
        // The preloader goes last, so an interrupted flash leaves the old one bootable.
        steps.sort_by_key(|s| s.area == Area::Boot1);
        Ok(steps)
    }

    /// Read the image's preloader file, if it has one.
    ///
    /// # Returns
    ///
    /// The preloader's bytes, or None if the image has none.
    ///
    /// # Errors
    ///
    /// Fails if the preloader file cannot be read.
    pub fn preloader(&mut self) -> Result<Option<Vec<u8>>> {
        let Some(file) = self.file_of(|p| p.area == Some(Area::Boot1)) else { return Ok(None) };
        let data = self.pkg.read(&file, |r| {
            let mut v = vec![];
            r.read_to_end(&mut v)?;
            Ok(v)
        })?;
        Ok(Some(data))
    }

    /// Refuse an image built for another chip (e.g. a Y1 MT6572 image on a Y2).
    ///
    /// # Arguments
    ///
    /// * `chip` - the connected chip
    ///
    /// # Errors
    ///
    /// Returns an error if the image is for another chip.
    pub fn check_chip(&self, chip: &Chip) -> Result<()> {
        let actual = format!("MT{:04X}", chip.hw_code);
        match &self.platform {
            Some(want) if *want != actual => bail!("this image is for {want}, but the device is {actual}"),
            _ => Ok(()),
        }
    }

    /// Check that every write ends inside the device's eMMC area. Call it
    /// after [`Image::locate`], which moves the user-area writes.
    ///
    /// # Arguments
    ///
    /// * `plan` - the planned writes
    /// * `emmc` - the device's area sizes
    ///
    /// # Errors
    ///
    /// Returns an error naming the first write that does not fit.
    pub fn check_fits(&self, plan: &[Step], emmc: &Emmc) -> Result<()> {
        for s in plan {
            let area_size = if s.area == Area::User { emmc.user } else { emmc.boot1 };
            if area_size > 0 && self.device_addr(s) + s.len > area_size {
                bail!("{} ends beyond the device's {:?} area. Is this image for another device?", s.partition, s.area);
            }
        }
        Ok(())
    }

    /// Find the image's partition table on the device and remember where the
    /// scatter's user-area addresses start. Scatter addresses count from the
    /// MBR, and old MediaTek devices keep it some way into the eMMC user area
    /// (the Y2 at 0xB80000), so each offset in [`USER_OFFSETS`] is read and
    /// [`find_table`] picks the one holding the image's MBR (or, failing
    /// that, the one holding any MBR). The extended tables (EBR1, EBR2, ...)
    /// are then read at their scatter address plus that offset, and every
    /// table the image has a file for is compared with it: its four partition
    /// entries and the 55 AA signature, not boot code or disk ID. Differing
    /// sectors are saved to the cache folder for inspection.
    ///
    /// # Arguments
    ///
    /// * `da` - the running download agent (reads one sector per offset and table)
    /// * `on` - receives log messages
    ///
    /// # Returns
    ///
    /// The tables that differ from the image's and how, e.g. "EBR1 (entry 1,
    /// entry 4)". Empty if the image's layout is the device's. Writing the
    /// image then repartitions the device.
    ///
    /// # Errors
    ///
    /// Returns an error if no or several partition tables are found, or a read fails.
    pub fn locate(&mut self, da: &mut Da, on: &mut dyn FnMut(Event)) -> Result<Vec<String>> {
        let mbr_addr = self.mbr_addr();
        let mut sectors = vec![];
        for offset in USER_OFFSETS {
            sectors.push((offset, da.read(Area::User, mbr_addr + offset, 512)?));
        }
        let ours = self.table_file("MBR")?;
        if ours.is_none() {
            on(Event::Log("The image has no MBR file, so the partition tables are not compared.".into()));
        }
        let (offset, diff) = find_table(&sectors, ours.as_deref())?;
        on(Event::Log(format!("Found the partition table at user-area offset {offset:#x}.")));
        self.user_offset = offset;

        // (table name, how it differs, device sector, image sector)
        let mut differing = vec![];
        let mut compared = vec![];
        if let (Some(diff), Some(ours)) = (diff, ours.clone()) {
            let theirs = sectors.iter().find(|(o, _)| *o == offset).unwrap().1.clone();
            differing.push(("MBR".to_string(), diff, theirs, ours));
        } else if ours.is_some() {
            compared.push("MBR".to_string());
        }
        let ebrs: Vec<(String, u64)> = self
            .parts
            .iter()
            .filter(|p| p.area == Some(Area::User) && is_ebr(&p.name))
            .map(|p| (p.name.clone(), p.addr))
            .collect();
        for (name, addr) in ebrs {
            let Some(ours) = self.table_file(&name)? else { continue };
            let theirs = da.read(Area::User, addr + offset, 512)?;
            match table_differences(&theirs, &ours) {
                None => compared.push(name),
                Some(diff) => differing.push((name, diff, theirs, ours)),
            }
        }

        if differing.is_empty() {
            if !compared.is_empty() {
                on(Event::Log(format!("The partition layout matches the image ({}).", compared.join(", "))));
            }
            return Ok(vec![]);
        }
        let dir = crate::catalog::cache_dir();
        std::fs::create_dir_all(&dir).ok();
        for (name, _, theirs, ours) in &differing {
            let name = name.to_lowercase();
            std::fs::write(dir.join(format!("layout-device-{name}.bin")), theirs).ok();
            std::fs::write(dir.join(format!("layout-image-{name}.bin")), ours).ok();
        }
        let changed: Vec<String> = differing.iter().map(|(n, d, ..)| format!("{n} ({d})")).collect();
        on(Event::Log(format!(
            "The image uses a different partition layout from the device ({}). The sectors were saved as \
             layout-device-*.bin and layout-image-*.bin in {}.",
            changed.join(", "),
            dir.display()
        )));
        Ok(changed)
    }

    /// Check that the partitions the device's tables list are the ones this
    /// image's scatter file describes, so its writes land in the right
    /// partitions. Call it after [`Image::locate`] when the image's tables are
    /// not written (no repartition). It matters most for images that carry no
    /// table files, whose layout [`Image::locate`] cannot compare.
    ///
    /// # Arguments
    ///
    /// * `da` - the running download agent (reads the MBR and each EBR)
    /// * `on` - receives log messages
    ///
    /// # Errors
    ///
    /// Fails if the device's tables cannot be read or do not fit the scatter
    /// file (see [`check_device_layout`]).
    pub fn check_device_partitions(&mut self, da: &mut Da, on: &mut dyn FnMut(Event)) -> Result<()> {
        let offset = self.user_offset;
        let device = walk_tables(self.mbr_addr(), &mut |pos| da.read(Area::User, pos + offset, 512))?;
        check_device_layout(&self.parts, &device)?;
        on(Event::Log(format!("The device's {} listed partitions match this image's scatter file.", device.len())));
        Ok(())
    }

    /// Compute where a write lands on the device: user-area writes are moved
    /// by the offset [`Image::locate`] found.
    ///
    /// # Arguments
    ///
    /// * `s` - the write
    ///
    /// # Returns
    ///
    /// The start address inside the step's eMMC area.
    fn device_addr(&self, s: &Step) -> u64 {
        if s.area == Area::User { s.addr + self.user_offset } else { s.addr }
    }

    /// Stream one step's file to the device, expanding sparse images, then
    /// read back three samples (start, middle and end, up to [`SAMPLE_LEN`]
    /// each) and compare them with what was sent.
    ///
    /// # Arguments
    ///
    /// * `s` - the write to perform
    /// * `da` - the running download agent
    /// * `on` - receives log and progress events
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read, the device rejects the
    /// data, or a sample read back differs from what was written.
    pub fn write(&mut self, s: &Step, da: &mut Da, on: &mut dyn FnMut(Event)) -> Result<()> {
        let addr = self.device_addr(s);
        on(Event::Log(format!("Writing {} ({}, {} bytes) at {addr:#x}.", s.partition, s.file, s.len)));
        let samples = self
            .pkg
            .read(&s.file, |r| {
                let mut head = [0u8; sparse::HEADER_LEN];
                let n = read_up_to(r, &mut head)?;
                let mut progress = |done| on(Event::Progress { partition: s.partition.clone(), done, total: s.len });
                let mut sparse_reader;
                let mut plain_reader;
                let mut boot_reader;
                let src: &mut dyn Read = if s.sparse {
                    sparse_reader = sparse::SparseReader::new(r, &head);
                    &mut sparse_reader
                } else if s.boot_header {
                    let header = boot_header(s.len - BOOT_HEADER_LEN);
                    boot_reader = std::io::Cursor::new(header).chain((&head[..n]).chain(r));
                    &mut boot_reader
                } else {
                    plain_reader = (&head[..n]).chain(r);
                    &mut plain_reader
                };
                let mut sampler = Sampler::new(src, s.len);
                da.write(s.area, addr, s.len, &mut sampler, &mut progress)?;
                Ok(sampler.samples)
            })
            .with_context(|| format!("writing {} failed", s.partition))?;

        for (offset, sent) in &samples {
            let theirs = da
                .read(s.area, addr + offset, (sent.len() as u64).div_ceil(512) * 512)
                .with_context(|| format!("reading back {} failed", s.partition))?;
            if theirs[..sent.len()] != sent[..] {
                bail!("{} did not land on the device. The read-back differs at offset {offset:#x}", s.partition);
            }
        }
        on(Event::Log(format!("Verified {}. All {} samples read back match.", s.partition, samples.len())));
        Ok(())
    }

    /// Find where the scatter puts the MBR (0 if it lists none).
    ///
    /// # Returns
    ///
    /// The MBR's scatter address.
    fn mbr_addr(&self) -> u64 {
        self.parts.iter().find(|p| p.name.eq_ignore_ascii_case("MBR")).map_or(0, |p| p.addr)
    }

    /// Read a partition-table file (MBR, EBR1, ...) of the image, if it has one.
    ///
    /// # Arguments
    ///
    /// * `name` - the scatter partition name
    ///
    /// # Returns
    ///
    /// The first sector of the file, zero-padded, or None if the image lacks it.
    ///
    /// # Errors
    ///
    /// Fails if the file cannot be read.
    fn table_file(&mut self, name: &str) -> Result<Option<Vec<u8>>> {
        let Some(file) = self.file_of(|p| p.name.eq_ignore_ascii_case(name)) else { return Ok(None) };
        let mut sector = vec![0u8; 512];
        self.pkg.read(&file, |r| read_up_to(r, &mut sector))?;
        Ok(Some(sector))
    }

    /// Find the file of the first matching partition, if the image contains it.
    ///
    /// # Arguments
    ///
    /// * `pred` - selects the partition
    ///
    /// # Returns
    ///
    /// The file name, or None.
    fn file_of(&mut self, pred: impl Fn(&Partition) -> bool) -> Option<String> {
        let file = self.parts.iter().find(|p| pred(p))?.file.clone()?;
        self.pkg.size(&file).map(|_| file)
    }
}

/// Size of the eMMC boot header in front of the preloader in boot1.
const BOOT_HEADER_LEN: u64 = 0x800;

/// Build the eMMC boot header the BootROM looks for at the start of boot1:
/// "EMMC_BOOT" at 0, and at 0x200 a "BRLYT" layout pointing at the preloader
/// at 0x800. Same values as the vendor flasher (via Updater CE), which match
/// the header read from a stock Y2.
///
/// # Arguments
///
/// * `preloader_len` - size of the bare preloader that follows the header
///
/// # Returns
///
/// The 0x800-byte header.
fn boot_header(preloader_len: u64) -> Vec<u8> {
    const BRLYT: usize = 0x200;
    let mut h = vec![0u8; BOOT_HEADER_LEN as usize];
    let mut put = |at: usize, v: u32| h[at..at + 4].copy_from_slice(&v.to_le_bytes());
    put(12, 1); // header version
    put(16, BRLYT as u32);
    // End of the preloader, rounded up to a sector (vendor DAs insist on it).
    let boundary = ((BOOT_HEADER_LEN + preloader_len).div_ceil(512) * 512) as u32;
    put(BRLYT + 8, 1);
    put(BRLYT + 12, BOOT_HEADER_LEN as u32);
    put(BRLYT + 16, boundary);
    put(BRLYT + 20, 0x4242_4242); // "BBBB": a boot loader is present
    put(BRLYT + 24, 0x0001_0005); // type: eMMC
    put(BRLYT + 28, BOOT_HEADER_LEN as u32);
    put(BRLYT + 32, boundary);
    put(BRLYT + 36, 1);
    h[..9].copy_from_slice(b"EMMC_BOOT");
    h[BRLYT..BRLYT + 5].copy_from_slice(b"BRLYT");
    h
}

/// Tell whether a preloader file needs a boot header written in front of it.
///
/// # Arguments
///
/// * `head` - the first bytes of the file
///
/// # Returns
///
/// True for a bare preloader ("MMM\x01"), false if it already starts with a boot header.
///
/// # Errors
///
/// Fails for anything else, so an unknown file is never written to boot1.
fn needs_boot_header(head: &[u8]) -> Result<bool> {
    if head.starts_with(b"MMM\x01") {
        Ok(true)
    } else if head.starts_with(b"EMMC_BOOT") {
        Ok(false)
    } else {
        bail!("the preloader file is neither a bare preloader nor has an eMMC boot header. It is not written")
    }
}

/// Longest stretch [`Image::write`] reads back per sample.
const SAMPLE_LEN: u64 = 64 * 1024;

/// Choose the stretches of a write to read back: start, middle and end, each
/// up to [`SAMPLE_LEN`] long and starting on a 512-byte sector.
///
/// # Arguments
///
/// * `len` - bytes written
///
/// # Returns
///
/// (offset, length) pairs, sorted and without duplicates.
fn sample_ranges(len: u64) -> Vec<(u64, u64)> {
    if len == 0 {
        return vec![];
    }
    let n = SAMPLE_LEN.min(len);
    let mut offsets: Vec<u64> = [0, (len / 2).saturating_sub(n / 2), len - n].iter().map(|o| o / 512 * 512).collect();
    offsets.dedup();
    offsets.into_iter().map(|o| (o, n)).collect()
}

/// A reader that keeps copies of chosen stretches of what passes through it.
struct Sampler<'a> {
    inner: &'a mut dyn Read,
    /// Bytes passed through so far.
    pos: u64,
    /// (offset, bytes copied so far), one per stretch
    samples: Vec<(u64, Vec<u8>)>,
    /// Length of each stretch
    wanted: Vec<u64>,
}

impl<'a> Sampler<'a> {
    /// Wrap a stream of `len` bytes, sampling the stretches [`sample_ranges`] picks.
    ///
    /// # Arguments
    ///
    /// * `inner` - the stream
    /// * `len` - its length
    ///
    /// # Returns
    ///
    /// The sampling reader.
    fn new(inner: &'a mut dyn Read, len: u64) -> Self {
        let ranges = sample_ranges(len);
        Sampler {
            inner,
            pos: 0,
            samples: ranges.iter().map(|&(o, _)| (o, vec![])).collect(),
            wanted: ranges.iter().map(|&(_, n)| n).collect(),
        }
    }
}

impl Read for Sampler<'_> {
    /// Read from the stream, copying the part that falls into a sample.
    ///
    /// # Arguments
    ///
    /// * `buf` - buffer to fill
    ///
    /// # Returns
    ///
    /// The number of bytes read.
    ///
    /// # Errors
    ///
    /// Fails if the wrapped stream fails.
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        let (start, end) = (self.pos, self.pos + n as u64);
        for ((offset, data), &want) in self.samples.iter_mut().zip(&self.wanted) {
            let from = start.max(*offset);
            let to = end.min(*offset + want);
            if from < to {
                data.extend_from_slice(&buf[(from - start) as usize..(to - start) as usize]);
            }
        }
        self.pos = end;
        Ok(n)
    }
}

/// Tell whether a partition name is an extended partition table (EBR1, EBR2, ...).
///
/// # Arguments
///
/// * `name` - scatter partition name
///
/// # Returns
///
/// True for "EBR" followed by digits, in any case.
fn is_ebr(name: &str) -> bool {
    name.len() > 3 && name[..3].eq_ignore_ascii_case("EBR") && name[3..].bytes().all(|b| b.is_ascii_digit())
}

/// Pick the offset of the device's partition table: the one sector whose
/// partition entries and signature equal the image's MBR, else the one sector
/// that is any valid MBR (55 AA and an entry with type, start and size).
///
/// # Arguments
///
/// * `sectors` - (user-area offset, sector read there)
/// * `ours` - the image's MBR sector, if it has one
///
/// # Returns
///
/// The offset, and how the table there differs from `ours` (None if it
/// matches or there is nothing to compare with).
///
/// # Errors
///
/// Fails if no offset or several offsets qualify.
fn find_table(sectors: &[(u64, Vec<u8>)], ours: Option<&[u8]>) -> Result<(u64, Option<String>)> {
    if let Some(ours) = ours {
        let same: Vec<u64> =
            sectors.iter().filter(|(_, s)| table_differences(s, ours).is_none()).map(|(o, _)| *o).collect();
        match same[..] {
            [offset] => return Ok((offset, None)),
            [] => {}
            _ => bail!("the image's partition table is on the device several times ({same:#x?}). Nothing is written"),
        }
    }
    let valid = |s: &[u8]| {
        signed(s) && (0..4).any(|i| {
            let e = entry(s, i);
            e[4] != 0 && e[8..12] != [0; 4] && e[12..16] != [0; 4]
        })
    };
    let found: Vec<&(u64, Vec<u8>)> = sectors.iter().filter(|(_, s)| s.len() >= 512 && valid(s)).collect();
    let list = || sectors.iter().map(|(o, _)| format!("{o:#x}")).collect::<Vec<_>>().join(", ");
    match found[..] {
        [(offset, s)] => Ok((*offset, ours.and_then(|ours| table_differences(s, ours)))),
        [] => bail!("no partition table was found on the device at user-area offsets {}. Nothing is written", list()),
        _ => bail!("partition tables were found at several user-area offsets ({}). Nothing is written", list()),
    }
}

/// Follow a chain of partition tables (an MBR and the EBRs it links to) and
/// collect the partitions they list. Every entry counts from the position of
/// its own table, as on MediaTek devices; an entry of type 0x05 links to the
/// next table.
///
/// # Arguments
///
/// * `mbr_addr` - where the MBR is, in scatter addresses
/// * `read` - reads the 512-byte sector at a scatter address
///
/// # Returns
///
/// (start, length) of each listed partition in scatter addresses, sorted.
///
/// # Errors
///
/// Fails if a read fails, a table has no 55 AA signature, or the chain is
/// longer than 16 tables (a loop).
fn walk_tables(mbr_addr: u64, read: &mut dyn FnMut(u64) -> Result<Vec<u8>>) -> Result<Vec<(u64, u64)>> {
    let mut parts = vec![];
    let mut tables = vec![mbr_addr];
    let mut count = 0;
    while let Some(pos) = tables.pop() {
        count += 1;
        if count > 16 {
            bail!("the device's partition tables link to more than 16 tables");
        }
        let sector = read(pos)?;
        if sector.len() < 512 || !signed(&sector) {
            bail!("there is no partition table at {pos:#x} where the device's tables point");
        }
        for i in 0..4 {
            let e = entry(&sector, i);
            let (start, len) = (crate::le32(e, 8) as u64, crate::le32(e, 12) as u64);
            match e[4] {
                0 => {}
                0x05 => tables.push(pos + start * 512),
                _ => parts.push((pos + start * 512, len * 512)),
            }
        }
    }
    parts.sort();
    Ok(parts)
}

/// Check that a device's listed partitions fit a scatter file: each one
/// starts where a user-area scatter partition starts and is at least as
/// large, and no scatter partition starts inside one of them.
///
/// # Arguments
///
/// * `parts` - the scatter file's partitions
/// * `device` - (start, length) of the device's listed partitions, from [`walk_tables`]
///
/// # Errors
///
/// Fails, naming the partition, if the device uses a different layout.
fn check_device_layout(parts: &[Partition], device: &[(u64, u64)]) -> Result<()> {
    // Addresses from 0xFFFF0000 on are counted from the end of the eMMC, not real positions.
    let user: Vec<&Partition> = parts.iter().filter(|p| p.area == Some(Area::User) && p.addr < 0xFFFF_0000).collect();
    for &(start, len) in device {
        let Some(p) = user.iter().find(|p| p.addr == start) else {
            bail!(
                "the device has a partition at {start:#x} that this image's scatter file does not have. \
                 The device uses a different partition layout. Nothing was written"
            );
        };
        if p.size > len {
            bail!(
                "{} is larger in this image's scatter file ({:#x} bytes) than on the device ({len:#x} bytes). \
                 The device uses a different partition layout. Nothing was written",
                p.name,
                p.size
            );
        }
    }
    for p in &user {
        if let Some(&(start, _)) = device.iter().find(|&&(start, len)| start < p.addr && p.addr < start + len) {
            bail!(
                "{} would start inside the device's partition at {start:#x}. The device uses a different \
                 partition layout. Nothing was written",
                p.name
            );
        }
    }
    Ok(())
}

/// Compare the partition entries and signature of two MBR sectors.
///
/// # Arguments
///
/// * `theirs` - the device's sector
/// * `ours` - the image's sector
///
/// # Returns
///
/// None if they match, else the differing parts, e.g. "entry 2, signature".
fn table_differences(theirs: &[u8], ours: &[u8]) -> Option<String> {
    if theirs.len() < 512 || ours.len() < 512 {
        return Some("short sector".into());
    }
    let differing: Vec<String> = (0..4)
        .filter(|&i| entry(theirs, i) != entry(ours, i))
        .map(|i| format!("entry {}", i + 1))
        .chain((theirs[0x1FE..0x200] != ours[0x1FE..0x200]).then(|| "signature".to_string()))
        .collect();
    (!differing.is_empty()).then(|| differing.join(", "))
}

/// One of the four 16-byte partition entries of an MBR or EBR sector.
///
/// # Arguments
///
/// * `sector` - the 512-byte table sector
/// * `i` - entry number, 0 to 3
///
/// # Returns
///
/// The entry's bytes.
fn entry(sector: &[u8], i: usize) -> &[u8] {
    &sector[0x1BE + i * 16..0x1CE + i * 16]
}

/// Tell whether a table sector ends with the 55 AA signature.
///
/// # Arguments
///
/// * `sector` - the 512-byte table sector
///
/// # Returns
///
/// True if the signature is there.
fn signed(sector: &[u8]) -> bool {
    sector[0x1FE..0x200] == [0x55, 0xAA]
}

/// Fill a buffer as far as the source allows; returns the bytes read.
///
/// # Arguments
///
/// * `r` - the source
/// * `buf` - buffer to fill
///
/// # Returns
///
/// The number of bytes read; less than the buffer only at the end of `r`.
fn read_up_to(r: &mut dyn Read, buf: &mut [u8]) -> Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}
