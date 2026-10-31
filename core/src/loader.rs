//! Download agent files (`MTK_DOWNLOAD_AGENT` format) in the loaders folder.
//! Layout ported from mtkclient (`Library/DA/daconfig.py`).

use crate::preloader::Chip;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// One loadable part of an agent: where it sits in the file and where it runs.
#[derive(Debug, Clone)]
pub struct Region {
    pub file_offset: u32,
    pub len: u32,
    pub addr: u32,
    pub sig_len: u32,
}

/// One download agent entry, for one chip.
#[derive(Debug, Clone)]
pub struct Loader {
    pub path: PathBuf,
    pub hw_code: u16,
    pub hw_ver: u16,
    pub sw_ver: u16,
    /// Legacy agents use region 1 (stage 1, SRAM) and region 2 (stage 2, DRAM).
    pub regions: Vec<Region>,
}

impl Loader {
    /// Read one stage's bytes from the loader file.
    ///
    /// # Arguments
    ///
    /// * `index` - region number (1 = stage 1, 2 = stage 2)
    ///
    /// # Returns
    ///
    /// The region and its bytes.
    ///
    /// # Errors
    ///
    /// Fails if the region does not exist or the file is truncated.
    pub fn stage(&self, index: usize) -> Result<(&Region, Vec<u8>)> {
        let r = self.regions.get(index).context("loader has no such stage")?;
        let file = std::fs::read(&self.path)?;
        let start = r.file_offset as usize;
        let data = file.get(start..start + r.len as usize).context("loader file truncated")?;
        Ok((r, data.to_vec()))
    }
}

/// Pick the agent for a chip from every loader file in a folder.
///
/// # Arguments
///
/// * `dir` - the loaders folder
/// * `chip` - the connected chip (code and versions must match)
///
/// # Returns
///
/// The first matching agent.
///
/// # Errors
///
/// Fails if the folder cannot be read or no loader matches the chip.
pub fn find(dir: &Path, chip: &Chip) -> Result<Loader> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("cannot read loaders folder {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("bin")))
        .collect();
    files.sort();
    for path in files {
        for l in parse(&path).unwrap_or_default() {
            let ver_ok = chip.hw_ver == 0 || l.hw_ver <= chip.hw_ver;
            let sw_ok = chip.sw_ver == 0 || l.sw_ver <= chip.sw_ver;
            if l.hw_code == chip.hw_code && ver_ok && sw_ok {
                return Ok(l);
            }
        }
    }
    bail!("no loader for chip {:#06x} in {}", chip.hw_code, dir.display())
}

/// List every agent entry in one `MTK_DOWNLOAD_AGENT` file.
///
/// # Arguments
///
/// * `path` - the loader file
///
/// # Returns
///
/// The entries; MT65xx entries are skipped unless the file is an mt6590 one.
///
/// # Errors
///
/// Fails if the file cannot be read or is not a download agent file.
fn parse(path: &Path) -> Result<Vec<Loader>> {
    let d = std::fs::read(path)?;
    if !d.starts_with(b"MTK_DOWNLOAD_AGENT") || d.len() < 0x6C {
        bail!("{} is not a MediaTek download agent file", path.display());
    }
    let u16_at = |o: usize| crate::le16(&d, o);
    let u32_at = |o: usize| crate::le32(&d, o);

    let count = u32_at(0x68) as usize;
    // Old files have 0xD8-byte entries without the sw_version field.
    let old = d.get(0x6C + 0xD8..0x6C + 0xDA) == Some(b"\xDA\xDA".as_slice());
    let size = if old { 0xD8 } else { 0xDC };
    // Like mtkclient: 65xx chips only trust the agents built for them ("mt6590"
    // files); the generic files carry incompatible newer agents for those codes.
    let for_65xx = path.to_string_lossy().to_lowercase().contains("mt6590");

    let mut out = vec![];
    for i in 0..count {
        let e = 0x6C + i * size;
        if e + size > d.len() {
            break;
        }
        let hw_code = u16_at(e + 2);
        if hw_code & 0xFF00 == 0x6500 && hw_code != 0x6580 && !for_65xx {
            continue;
        }
        let (sw_ver, fields) = if old { (0, e + 8) } else { (u16_at(e + 8), e + 12) };
        let region_count = u16_at(fields + 6) as usize;
        let regions = (0..region_count)
            .map(|r| {
                let o = fields + 8 + r * 20;
                Region { file_offset: u32_at(o), len: u32_at(o + 4), addr: u32_at(o + 8), sig_len: u32_at(o + 16) }
            })
            .collect();
        out.push(Loader { path: path.to_path_buf(), hw_code, hw_ver: u16_at(e + 6), sw_ver, regions });
    }
    Ok(out)
}
