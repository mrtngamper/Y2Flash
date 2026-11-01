//! Flash a MediaTek firmware image (folder or `.zip` with a scatter file) to a
//! device over USB. The protocol is ported from mtkclient; the download agent
//! comes from the `loaders` folder.
//!
//! A flash runs in eight steps (see [`flash`]):
//!
//! 1. Open the image: parse its scatter once and plan the partition writes.
//! 2. Catch the device in preloader or BootROM mode (handshake, chip ID).
//! 3. Check that the image is built for this chip.
//! 4. BootROM mode only: start the image's preloader in RAM (recovery path).
//! 5. Start the download agent (stage 1 in SRAM, stage 2 in DRAM).
//! 6. Find the partition table on the device (it fixes where the scatter's
//!    addresses lie) and check the writes fit. If the image's tables differ,
//!    writing it repartitions the device, which needs explicit consent.
//!    Otherwise the device's listed partitions must match the scatter file.
//! 7. Write each partition, the preloader last.
//! 8. Let the device restart.
//!
//! ```no_run
//! let image = std::path::Path::new("rom_y2.zip");
//! let on = &mut |e| println!("{e:?}");
//! y2flash::flash(image, &y2flash::default_loader_dir(), &Default::default(), on, &mut |_| false)?;
//! # anyhow::Ok(())
//! ```

pub mod catalog;
mod da;
mod image;
mod link;
mod loader;
#[cfg(target_os = "linux")]
mod nbd;
mod package;
mod port;
mod preloader;
mod scatter;
mod session;
mod sparse;

pub use da::{Area, Emmc};
pub use image::{ListedPartition, Step};
pub use preloader::Chip;

use anyhow::{bail, Result};
use da::Da;
use image::Image;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// What to flash.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// File name of the scatter to use, for images that carry several.
    pub scatter: Option<String>,
    /// Only flash these partitions (scatter names, case-insensitive). Empty = all.
    pub only: Vec<String>,
    /// Also write the preloader. Off by default because it is rarely needed;
    /// if a written preloader ever fails, BootROM mode (see README) recovers.
    pub with_preloader: bool,
    /// Repartition the device if the image's partition layout differs from
    /// the device's, without asking. This erases the internal storage. When
    /// false, [`flash`] asks its `confirm` callback instead.
    pub repartition: bool,
}

/// Progress reports from a running operation.
#[derive(Debug, Clone)]
pub enum Event {
    Log(String),
    /// Bytes written to a partition so far.
    Progress { partition: String, done: u64, total: u64 },
    /// Bytes of a firmware file downloaded so far.
    Download { file: String, done: u64, total: u64 },
}

/// What `info` found out about the device.
#[derive(Debug, Clone)]
pub struct Device {
    pub chip: Chip,
    pub emmc: Emmc,
    /// The partitions the device's tables list, and the user-area offset
    /// their scatter addresses are moved by; None if the tables could not be read.
    pub layout: Option<(Vec<ListedPartition>, u64)>,
}

/// Work out what `flash` would write, without touching any device.
///
/// # Arguments
///
/// * `path` - the image (folder, `.zip` or scatter file)
/// * `opts` - what to flash
///
/// # Returns
///
/// The writes, in order.
///
/// # Errors
///
/// Fails if the image cannot be opened or an image file does not fit its partition.
pub fn plan(path: &Path, opts: &Options) -> Result<Vec<Step>> {
    Image::open(path, opts.scatter.as_deref())?.plan(opts)
}

/// Flash an image to the next device that is plugged in, following the eight
/// steps listed at the top of this file.
///
/// # Arguments
///
/// * `path` - the image (folder, `.zip` or scatter file)
/// * `loaders` - folder with download agent files
/// * `opts` - what to flash
/// * `on` - receives log and progress events
/// * `confirm` - asked whether to repartition when the image's partition
///   layout differs from the device's (gets the differing tables). The device
///   is held and nothing is written while it waits. Not asked when
///   `opts.repartition` is set.
///
/// # Errors
///
/// Returns an error describing the step that failed, or saying that the
/// layout differs and repartitioning was declined.
pub fn flash(
    path: &Path,
    loaders: &Path,
    opts: &Options,
    on: &mut dyn FnMut(Event),
    confirm: &mut dyn FnMut(&[String]) -> bool,
) -> Result<()> {
    // 1. Image and plan.
    let mut image = Image::open(path, opts.scatter.as_deref())?;
    let plan = image.plan(opts)?;
    if plan.is_empty() {
        bail!("nothing to flash");
    }
    // 2.-5. Device, chip check, recovery, download agent.
    let mut da = connect(loaders, Some(&mut image), on)?;
    // 6. Layout found (and a different one agreed to), writes fit. Nothing is
    //    written yet, so on failure the session is ended and the device
    //    restarts normally.
    let checked = image.locate(&mut da, on).and_then(|changed| {
        if agree_to_layout(&changed, opts, confirm)? {
            on(Event::Log("Repartitioning the device. Its internal storage is erased.".into()));
        } else {
            image.check_device_partitions(&mut da, on)?;
        }
        image.check_fits(&plan, &da.emmc)
    });
    da.finish_on_err(checked)?;
    // 7. Write.
    for step in &plan {
        image.write(step, &mut da, on)?;
    }
    // 8. Restart.
    da.finish()?;
    on(Event::Log("Done. The player now restarts.".into()));
    Ok(())
}

/// Decide whether a flash may go on when the image's partition layout
/// differs from the device's: only if the whole image is written and the
/// repartition was agreed to, by option or by asking.
///
/// # Arguments
///
/// * `changed` - the differing tables, from [`Image::locate`]
/// * `opts` - what to flash
/// * `confirm` - asked unless `opts.repartition` is set
///
/// # Returns
///
/// True if the flash repartitions the device, false if the layout is unchanged.
///
/// # Errors
///
/// Fails if the layout differs and only some partitions were chosen, or the
/// repartition was declined.
fn agree_to_layout(changed: &[String], opts: &Options, confirm: &mut dyn FnMut(&[String]) -> bool) -> Result<bool> {
    if changed.is_empty() {
        return Ok(false);
    }
    let tables = changed.join(", ");
    if !opts.only.is_empty() {
        bail!(
            "this firmware uses a different partition layout ({tables}). Installing it repartitions \
             the device, which writes the whole image, so it cannot be limited to some partitions"
        );
    }
    if !opts.repartition && !confirm(changed) {
        bail!(
            "this firmware uses a different partition layout ({tables}). Installing it repartitions \
             the device and erases its internal storage. Nothing was written"
        );
    }
    Ok(true)
}

/// Read the chip, the eMMC sizes and the partitions the device's tables list,
/// then let the device restart. Writes nothing. An image is only needed in
/// BootROM mode (for its preloader); if given, it is also checked against
/// the chip, its partition table is located on the device and compared as a
/// flash would (the result is logged), and its scatter file names the
/// partitions. Without one, the tables are looked for where the Y2 has them.
///
/// # Arguments
///
/// * `loaders` - folder with download agent files
/// * `path` - optional image
/// * `scatter` - which scatter file to use if the image has several
/// * `on` - receives log messages
///
/// # Returns
///
/// The chip, eMMC and partition information. A partition table that cannot
/// be read is logged and leaves `layout` at None.
///
/// # Errors
///
/// Fails if no device appears, the image is for another chip, or the download agent cannot be started.
pub fn info(loaders: &Path, path: Option<&Path>, scatter: Option<&str>, on: &mut dyn FnMut(Event)) -> Result<Device> {
    let mut image = path.map(|p| Image::open(p, scatter)).transpose()?;
    let mut da = connect(loaders, image.as_mut(), on)?;
    let layout = match image.as_mut() {
        Some(image) => image_layout(image, &mut da, on),
        None => image::read_device_layout(&mut da),
    };
    let layout = layout
        .map_err(|e| on(Event::Log(format!("The partition table could not be read. {}", describe(&e)))))
        .ok();
    let device = Device { chip: da.chip.clone(), emmc: da.emmc.clone(), layout };
    da.finish()?;
    Ok(device)
}

/// Find the device's partition tables as a flash of `image` would, read the
/// partitions they list, and log how they compare with the image.
///
/// # Arguments
///
/// * `image` - the image whose scatter file names the partitions
/// * `da` - the running download agent
/// * `on` - receives log messages
///
/// # Returns
///
/// The listed partitions and the user-area offset.
///
/// # Errors
///
/// Fails if the tables cannot be found or read. A layout that differs from
/// the image's is only logged.
fn image_layout(image: &mut Image, da: &mut Da, on: &mut dyn FnMut(Event)) -> Result<(Vec<ListedPartition>, u64)> {
    let changed = image.locate(da, on)?;
    let device = image.device_layout(da)?;
    if !changed.is_empty() {
        on(Event::Log("Flashing this image would repartition the device and erase its internal storage.".into()));
    } else if let Err(e) = image.check_layout(&device, on) {
        on(Event::Log(format!("The layout check failed. {}", describe(&e))));
    }
    Ok((device, image.user_offset()))
}

/// What `mount` mounts and how.
#[derive(Debug, Clone, Default)]
pub struct MountOptions {
    /// The partition: its number in `info`'s table, or its name (needs an image).
    pub partition: String,
    /// The directory to mount it on.
    pub dir: PathBuf,
    /// Mount it writable; otherwise the device and the mount are read-only.
    pub writable: bool,
    /// Extra mount options, comma-separated (e.g. "noload").
    pub options: Option<String>,
}

/// Mount one of the device's partitions on a directory (Linux, as root).
///
/// The partition table is read as [`info`] does, the chosen partition is
/// made available as a /dev/nbdN block device and mounted. This then serves
/// the device's reads and writes until Ctrl-C unmounts it, and lets the
/// device restart. Root, the directory and a free nbd device are checked
/// before waiting for the device.
///
/// # Arguments
///
/// * `loaders` - folder with download agent files
/// * `path` - optional image: names the partitions; needed in BootROM mode
/// * `scatter` - which scatter file to use if the image has several
/// * `m` - the partition, directory and mount options
/// * `on` - receives log messages
///
/// # Errors
///
/// Fails if not run as root, the partition is not on the device, the mount
/// fails, or the device stops answering.
#[cfg(target_os = "linux")]
pub fn mount(loaders: &Path, path: Option<&Path>, scatter: Option<&str>, m: &MountOptions, on: &mut dyn FnMut(Event)) -> Result<()> {
    nbd::preflight(&m.dir)?;
    let mut image = path.map(|p| Image::open(p, scatter)).transpose()?;
    let mut da = connect(loaders, image.as_mut(), on)?;
    let layout = match image.as_mut() {
        Some(image) => image_layout(image, &mut da, on),
        None => image::read_device_layout(&mut da),
    };
    let user = da.emmc.user;
    let picked = layout.and_then(|(parts, offset)| {
        let i = pick(&parts, &m.partition)?;
        let start = parts[i].start + offset;
        // MediaTek tables give the last partition a huge length meaning "the rest of the eMMC".
        let size = parts[i].size.min(user.saturating_sub(start));
        if size == 0 {
            bail!("partition {} lies outside the eMMC", i + 1);
        }
        let name = parts[i].name.as_ref().map(|n| format!(" ({n})")).unwrap_or_default();
        Ok((start, size, format!("partition {}{name}", i + 1)))
    });
    let (start, size, label) = da.finish_on_err(picked)?;
    on(Event::Log(format!("Mounting {label}, {} MiB at eMMC address {start:#x}.", size >> 20)));
    let mut disk = PartitionDisk { da: &mut da, start };
    let served = nbd::run(&mut disk, size, &m.dir, m.writable, m.options.as_deref(), on);
    da.finish_on_err(served)?;
    da.finish()?;
    on(Event::Log("Done. The player now restarts.".into()));
    Ok(())
}

/// Choose a partition by its number in `info`'s table or by name.
///
/// # Arguments
///
/// * `parts` - the device's listed partitions, sorted by start
/// * `which` - a number from 1, or a partition name (any case)
///
/// # Returns
///
/// The partition's index in `parts`.
///
/// # Errors
///
/// Fails, listing the choices, if no partition matches.
#[cfg(target_os = "linux")]
fn pick(parts: &[ListedPartition], which: &str) -> Result<usize> {
    if let Ok(n) = which.parse::<usize>() {
        if (1..=parts.len()).contains(&n) {
            return Ok(n - 1);
        }
        bail!("there is no partition {n}. The device lists partitions 1 to {}", parts.len());
    }
    if let Some(i) = parts.iter().position(|p| p.name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(which))) {
        return Ok(i);
    }
    let names: Vec<&str> = parts.iter().filter_map(|p| p.name.as_deref()).collect();
    if names.is_empty() {
        bail!("partitions can only be chosen by name with a firmware image (--image). Use the number from `info` instead");
    }
    bail!("there is no partition named {which}. The device has {}", names.join(", "))
}

/// A partition of the eMMC user area, addressed from its start.
#[cfg(target_os = "linux")]
struct PartitionDisk<'a> {
    da: &'a mut Da,
    /// The partition's eMMC address.
    start: u64,
}

#[cfg(target_os = "linux")]
impl nbd::Disk for PartitionDisk<'_> {
    fn read(&mut self, offset: u64, len: u32) -> Result<Vec<u8>> {
        self.da.read(Area::User, self.start + offset, len as u64)
    }

    fn write(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        self.da.write(Area::User, self.start + offset, data.len() as u64, &mut &data[..], &mut |_| {})
    }
}

/// Describe an error and its causes for people: each part becomes a sentence
/// that starts with a capital letter and ends with a full stop.
///
/// # Arguments
///
/// * `e` - the error
///
/// # Returns
///
/// The sentences, separated by spaces.
pub fn describe(e: &anyhow::Error) -> String {
    e.chain().map(|cause| sentence(&cause.to_string())).collect::<Vec<_>>().join(" ")
}

/// Make a message a sentence: capital first letter, full stop at the end.
///
/// # Arguments
///
/// * `text` - the message
///
/// # Returns
///
/// The sentence.
fn sentence(text: &str) -> String {
    let text = text.trim();
    let mut chars = text.chars();
    let mut out: String = chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default();
    if !out.is_empty() && !out.ends_with(['.', '?', '!']) {
        out.push('.');
    }
    out
}

/// Find the loaders folder: `$Y2FLASH_LOADERS`, else `loaders` next to the
/// executable (or a parent of it), else the source tree's.
///
/// # Returns
///
/// The folder path (which may not exist).
pub fn default_loader_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("Y2FLASH_LOADERS") {
        return dir.into();
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.ancestors().skip(1).take(4).map(|a| a.join("loaders")).find(|d| d.is_dir()) {
            return dir;
        }
    }
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../loaders"))
}

/// Ask for the device to be plugged in, catch it (up to two minutes) and
/// start the download agent. With an image, it is checked against the chip,
/// and in BootROM mode its preloader is started in RAM first.
///
/// # Arguments
///
/// * `loaders` - folder with download agent files
/// * `image` - optional image (needed in BootROM mode)
/// * `on` - receives log messages
///
/// # Returns
///
/// The running download agent.
///
/// # Errors
///
/// Fails if no device appears, the image is for another chip, or the download agent cannot be started.
fn connect(loaders: &Path, image: Option<&mut Image>, on: &mut dyn FnMut(Event)) -> Result<Da> {
    on(Event::Log("Waiting for the device. Power it off, then plug it in.".into()));
    let session = session::catch(Duration::from_secs(120), on)?;
    let preloader = match image {
        Some(image) => {
            image.check_chip(&session.chip)?;
            image.preloader()?
        }
        None => None,
    };
    session.leave_bootrom(preloader.as_deref(), on)?.start_agent(loaders, on)
}

/// Read a little-endian u16 at `offset`.
pub(crate) fn le16(b: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([b[offset], b[offset + 1]])
}

/// Read a little-endian u32 at `offset`.
pub(crate) fn le32(b: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(b[offset..offset + 4].try_into().unwrap())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn part(start: u64, name: Option<&str>) -> ListedPartition {
        ListedPartition { start, size: 512, kind: 0x83, name: name.map(str::to_string) }
    }

    #[test]
    fn pick_by_number_or_name() {
        let parts = [part(0, Some("CACHE")), part(512, Some("FAT"))];
        assert_eq!(pick(&parts, "2").unwrap(), 1);
        assert_eq!(pick(&parts, "fat").unwrap(), 1);
        assert!(pick(&parts, "3").is_err());
        assert!(pick(&parts, "0").is_err());
        assert!(pick(&[part(0, None)], "FAT").is_err());
    }
}
