//! MediaTek scatter files (the `- partition_index:` YAML-style layout, V1.1.x).

use crate::da::Area;
use anyhow::{bail, Context, Result};

/// One partition entry of a scatter file.
#[derive(Debug, Clone)]
pub struct Partition {
    pub name: String,
    /// Image file shipped for this partition, if any.
    pub file: Option<String>,
    pub download: bool,
    pub area: Option<Area>,
    /// Address inside its eMMC area (what the download agent expects).
    pub addr: u64,
    pub size: u64,
}

/// Read all partition entries.
///
/// # Arguments
///
/// * `text` - the scatter file's contents
///
/// # Returns
///
/// The partitions in file order.
///
/// # Errors
///
/// Fails if a number is malformed or no partitions are found.
pub fn parse(text: &str) -> Result<Vec<Partition>> {
    let mut parts = vec![];
    let mut cur: Option<Partition> = None;
    for line in text.lines() {
        let line = line.trim().trim_start_matches("- ");
        let Some((key, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        if key == "partition_index" {
            parts.extend(cur.take());
            cur = Some(Partition { name: String::new(), file: None, download: false, area: None, addr: 0, size: 0 });
            continue;
        }
        let Some(p) = cur.as_mut() else { continue };
        match key {
            "partition_name" => p.name = value.to_string(),
            "file_name" if value != "NONE" => p.file = Some(value.to_string()),
            "is_download" => p.download = value == "true",
            "physical_start_addr" => p.addr = hex(value)?,
            "partition_size" => p.size = hex(value)?,
            "region" => {
                p.area = match value {
                    "EMMC_BOOT_1" => Some(Area::Boot1),
                    "EMMC_BOOT_2" => Some(Area::Boot2),
                    "EMMC_USER" => Some(Area::User),
                    _ => None,
                }
            }
            _ => {}
        }
    }
    parts.extend(cur);
    if parts.is_empty() {
        bail!("no partitions found (only the YAML-style scatter format is supported)");
    }
    Ok(parts)
}

/// Read which chip the image is built for (`platform: MT6582` in the general section).
///
/// # Arguments
///
/// * `text` - the scatter file's contents
///
/// # Returns
///
/// The platform in upper case, or None.
pub fn platform(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.trim().strip_prefix("platform:"))
        .map(|p| p.trim().to_uppercase())
}

/// Parse a hex number with or without `0x`.
///
/// # Arguments
///
/// * `s` - the number as text
///
/// # Returns
///
/// The number.
fn hex(s: &str) -> Result<u64> {
    let digits = s.trim_start_matches("0x").trim_start_matches("0X");
    u64::from_str_radix(digits, 16).with_context(|| format!("bad hex number '{s}'"))
}
