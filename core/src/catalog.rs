//! Online firmware: the community catalogue (the same manifest Updater CE
//! uses) and GitHub releases, downloaded into a cache and checked against
//! GitHub's SHA-256 digests.

use crate::Event;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// The catalogue of installable software, maintained by the Updater CE project.
pub const MANIFEST_URL: &str = "https://innioasis.app/slidia_manifest.xml";
const GITHUB_API: &str = "https://api.github.com";

/// One installable software for one device, as listed in the catalogue.
#[derive(Debug, Clone)]
pub struct Software {
    pub name: String,
    /// GitHub repository, e.g. "SPYCEMAGIC/Y2-Rockbox".
    pub repo: String,
    /// The repository's web page.
    pub url: String,
}

/// One GitHub release with the firmware files that fit the device.
#[derive(Debug, Clone)]
pub struct Release {
    pub tag: String,
    pub title: String,
    /// Publication date, YYYY-MM-DD.
    pub date: String,
    pub prerelease: bool,
    /// The release's web page on GitHub.
    pub url: String,
    pub files: Vec<Asset>,
}

/// One downloadable firmware file of a release.
#[derive(Debug, Clone)]
pub struct Asset {
    pub name: String,
    pub size: u64,
    pub url: String,
    /// SHA-256 in hex, as reported by GitHub.
    pub sha256: Option<String>,
}

/// Download the catalogue and list the software available for a device.
///
/// # Arguments
///
/// * `device` - device model as named in the catalogue, e.g. "Y2"
///
/// # Returns
///
/// The software for that device, without duplicates, in catalogue order.
///
/// # Errors
///
/// Fails if the catalogue cannot be downloaded.
pub fn software(device: &str) -> Result<Vec<Software>> {
    let xml = ureq::get(MANIFEST_URL).call().context("cannot download the catalogue")?.into_string()?;
    let mut out: Vec<Software> = vec![];
    for attrs in package_tags(&xml) {
        let get = |k: &str| attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
        let (Some(name), Some(repo), Some(dev)) = (get("name"), get("repo"), get("device")) else { continue };
        // Entries with device_type="0" only tell old app versions to update.
        if get("device_type").as_deref() == Some("0") || !dev.eq_ignore_ascii_case(device) {
            continue;
        }
        if !out.iter().any(|s| s.repo.eq_ignore_ascii_case(&repo)) {
            let url = format!("https://github.com/{repo}");
            out.push(Software { name, repo, url });
        }
    }
    Ok(out)
}

/// List a repository's releases with the firmware files that fit a device.
///
/// # Arguments
///
/// * `repo` - GitHub repository, e.g. "SPYCEMAGIC/Y2-Rockbox"
/// * `device` - device model, used to drop files made for another model
///
/// # Returns
///
/// The releases that have at least one fitting firmware file, newest first.
///
/// # Errors
///
/// Fails if GitHub cannot be reached or refuses the request (it allows 60
/// unauthenticated requests per hour).
pub fn releases(repo: &str, device: &str) -> Result<Vec<Release>> {
    #[derive(Deserialize)]
    struct GhRelease {
        tag_name: String,
        name: Option<String>,
        published_at: Option<String>,
        prerelease: bool,
        draft: bool,
        html_url: String,
        assets: Vec<GhAsset>,
    }
    #[derive(Deserialize)]
    struct GhAsset {
        name: String,
        size: u64,
        browser_download_url: String,
        digest: Option<String>,
    }

    let url = format!("{GITHUB_API}/repos/{repo}/releases?per_page=30");
    let list: Vec<GhRelease> = ureq::get(&url)
        .set("Accept", "application/vnd.github+json")
        .call()
        .with_context(|| format!("cannot list the releases of {repo}"))?
        .into_json()?;

    let mut out: Vec<Release> = list
        .into_iter()
        .filter(|r| !r.draft)
        .filter_map(|r| {
            let assets: Vec<Asset> = r
                .assets
                .into_iter()
                .map(|a| Asset {
                    name: a.name,
                    size: a.size,
                    url: a.browser_download_url,
                    sha256: a.digest.and_then(|d| d.strip_prefix("sha256:").map(String::from)),
                })
                .collect();
            let files = firmware_files(assets, device);
            (!files.is_empty()).then(|| Release {
                title: r.name.filter(|n| !n.is_empty()).unwrap_or_else(|| r.tag_name.clone()),
                date: r.published_at.unwrap_or_default().chars().take(10).collect(),
                tag: r.tag_name,
                prerelease: r.prerelease,
                url: r.html_url,
                files,
            })
        })
        .collect();

    // Repos like the stock ROM archive mix models: once any release names the
    // device in a file name, unmarked files elsewhere belong to other models.
    let dev = device.to_lowercase();
    let marked = |a: &Asset| a.name.to_lowercase().contains(&dev);
    if out.iter().any(|r| r.files.iter().any(marked)) {
        for r in &mut out {
            r.files.retain(marked);
        }
        out.retain(|r| !r.files.is_empty());
    }
    Ok(out)
}

/// Find a software in the catalogue by name or repository. An "owner/repo"
/// not in the catalogue is accepted as is.
///
/// # Arguments
///
/// * `query` - software name (case-insensitive) or GitHub repository
/// * `device` - device model, e.g. "Y2"
///
/// # Returns
///
/// The matching software.
///
/// # Errors
///
/// Fails if the catalogue cannot be downloaded or nothing matches.
pub fn find(query: &str, device: &str) -> Result<Software> {
    let list = software(device)?;
    if let Some(s) = list.iter().find(|s| s.name.eq_ignore_ascii_case(query) || s.repo.eq_ignore_ascii_case(query)) {
        return Ok(s.clone());
    }
    if query.contains('/') {
        let url = format!("https://github.com/{query}");
        return Ok(Software { name: query.to_string(), repo: query.to_string(), url });
    }
    let names: Vec<&str> = list.iter().map(|s| s.name.as_str()).collect();
    bail!("there is no software called '{query}' for {device}. Available are {}", names.join(", "))
}

/// Pick a release and one of its firmware files: the given ones, or else the
/// newest release and its first fitting file.
///
/// # Arguments
///
/// * `repo` - GitHub repository
/// * `device` - device model, e.g. "Y2"
/// * `tag` - release tag, or None for the newest
/// * `file` - file name, or None for the first fitting one
///
/// # Returns
///
/// The release and the chosen file.
///
/// # Errors
///
/// Fails if GitHub cannot be reached, or the tag or file does not exist for
/// this device.
pub fn choose(repo: &str, device: &str, tag: Option<&str>, file: Option<&str>) -> Result<(Release, Asset)> {
    let list = releases(repo, device)?;
    let release = match tag {
        Some(t) => list.into_iter().find(|r| r.tag == t).with_context(|| format!("{repo} has no release {t} for {device}"))?,
        None => list.into_iter().next().with_context(|| format!("{repo} has no firmware release for {device}"))?,
    };
    let asset = match file {
        Some(f) => release.files.iter().find(|a| a.name == f).with_context(|| format!("release {} has no file {f}", release.tag))?,
        None => &release.files[0],
    }
    .clone();
    Ok((release, asset))
}

/// Download a release file into the cache, resuming a partial download, and
/// verify it against GitHub's SHA-256. A complete, verified copy is reused.
///
/// # Arguments
///
/// * `repo` - GitHub repository the file belongs to
/// * `tag` - release tag the file belongs to
/// * `file` - the file to download
/// * `on` - receives download progress
///
/// # Returns
///
/// Path of the verified file in the cache.
///
/// # Errors
///
/// Fails if the download fails or the file does not match its checksum.
pub fn download(repo: &str, tag: &str, file: &Asset, on: &mut dyn FnMut(Event)) -> Result<PathBuf> {
    let dest = cache_path(repo, tag, file);
    let dir = dest.parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    if dest.is_file() && verify(&dest, file)? {
        on(Event::Log(format!("Using the cached file {}.", dest.display())));
        return Ok(dest);
    }

    let part = dir.join(format!("{}.part", file.name));
    let have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    let mut req = ureq::get(&file.url);
    if have > 0 && have < file.size {
        req = req.set("Range", &format!("bytes={have}-"));
    }
    let resp = req.call().with_context(|| format!("cannot download {}", file.name))?;
    let resumed = resp.status() == 206;
    let mut out = OpenOptions::new().create(true).write(true).append(resumed).truncate(!resumed).open(&part)?;
    let mut done = if resumed { have } else { 0 };
    on(Event::Log(format!("Downloading {} ({} MiB).", file.name, file.size >> 20)));

    let mut body = resp.into_reader();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = body.read(&mut buf).context("the download was interrupted. Start it again to resume it")?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        done += n as u64;
        on(Event::Download { file: file.name.clone(), done, total: file.size });
    }
    drop(out);

    if !verify(&part, file)? {
        std::fs::remove_file(&part).ok();
        bail!("{} is corrupt because its size or SHA-256 does not match. It was deleted", file.name);
    }
    std::fs::rename(&part, &dest)?;
    on(Event::Log(format!("Saved and verified {}.", dest.display())));
    Ok(dest)
}

/// Build the path a release file is downloaded to.
///
/// # Arguments
///
/// * `repo` - GitHub repository the file belongs to
/// * `tag` - release tag the file belongs to
/// * `file` - the file
///
/// # Returns
///
/// `<cache>/<owner>_<repo>/<tag>/<file name>`.
fn cache_path(repo: &str, tag: &str, file: &Asset) -> PathBuf {
    cache_dir().join(repo.replace('/', "_")).join(tag).join(&file.name)
}

/// Find the folder downloads are kept in: `$Y2FLASH_CACHE`, else the user's
/// cache folder (e.g. `~/.cache/y2flash` on Linux).
///
/// # Returns
///
/// The folder path (which may not exist yet).
pub fn cache_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("Y2FLASH_CACHE") {
        return dir.into();
    }
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("y2flash")
}

/// Keep the zip files that are firmware images for the device: source
/// archives are dropped, and if any name marks a model ("y1", "y2"), files
/// marked for another model or unmarked Y1-era names are dropped too.
///
/// # Arguments
///
/// * `assets` - all files of a release
/// * `device` - the device model, e.g. "Y2"
///
/// # Returns
///
/// The fitting firmware files, those naming the device first.
fn firmware_files(assets: Vec<Asset>, device: &str) -> Vec<Asset> {
    let dev = device.to_lowercase();
    let mut zips: Vec<Asset> = assets
        .into_iter()
        .filter(|a| a.name.to_lowercase().ends_with(".zip") && !a.name.to_lowercase().contains("source"))
        .collect();
    let names_device = |a: &Asset| a.name.to_lowercase().contains(&dev);
    if zips.iter().any(names_device) {
        zips.retain(names_device);
    } else {
        // Without a marker for this device, files marked for another model
        // cannot be right ("rom_y2.zip" for a Y1; "rom_y1_…", "rom_type_b.zip"
        // for anything else).
        zips.retain(|a| {
            let n = a.name.to_lowercase();
            if dev == "y1" {
                !n.contains("y2")
            } else {
                !n.contains("y1") && !n.contains("type_b")
            }
        });
    }
    zips.sort_by_key(|a| !names_device(a));
    zips
}

/// Check a downloaded file's size and, if GitHub reported one, its SHA-256.
///
/// # Arguments
///
/// * `path` - the downloaded file
/// * `file` - the release file it should be
///
/// # Returns
///
/// True if the file matches.
///
/// # Errors
///
/// Fails if the file cannot be read.
fn verify(path: &Path, file: &Asset) -> Result<bool> {
    if std::fs::metadata(path)?.len() != file.size {
        return Ok(false);
    }
    let Some(want) = &file.sha256 else { return Ok(true) };
    let mut hasher = Sha256::new();
    std::io::copy(&mut File::open(path)?, &mut hasher)?;
    let got: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok(got.eq_ignore_ascii_case(want))
}

/// Extract the attributes of every `<package …>` tag outside XML comments.
///
/// # Arguments
///
/// * `xml` - the catalogue file's text
///
/// # Returns
///
/// One list of (attribute, value) pairs per package tag.
fn package_tags(xml: &str) -> Vec<Vec<(String, String)>> {
    let mut text = xml.to_string();
    while let Some(start) = text.find("<!--") {
        let end = text[start..].find("-->").map(|e| start + e + 3).unwrap_or(text.len());
        text.replace_range(start..end, "");
    }
    text.split("<package")
        .skip(1)
        .map(|tag| {
            let tag = &tag[..tag.find('>').unwrap_or(tag.len())];
            let mut attrs = vec![];
            let mut rest = tag;
            while let Some(eq) = rest.find("=\"") {
                let key = rest[..eq].trim().rsplit(char::is_whitespace).next().unwrap_or("").to_string();
                let after = &rest[eq + 2..];
                let Some(close) = after.find('"') else { break };
                attrs.push((key, after[..close].to_string()));
                rest = &after[close + 1..];
            }
            attrs
        })
        .collect()
}
