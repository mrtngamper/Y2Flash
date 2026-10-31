//! A firmware image: a folder or `.zip` with one scatter file and the files it names.

use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Where an image's files live.
pub enum Package {
    /// The folder containing the scatter file.
    Dir(PathBuf),
    /// A zip; `prefix` is the folder of the scatter file inside it.
    Zip { zip: zip::ZipArchive<File>, prefix: String },
}

impl Package {
    /// Open an image and return it with its scatter file's text.
    ///
    /// # Arguments
    ///
    /// * `path` - a folder, a `.zip`, or the scatter file itself
    /// * `scatter` - file name of the scatter to use, for images that carry
    ///   several (e.g. one for a first install and one that keeps user data)
    ///
    /// # Returns
    ///
    /// The package and its scatter file's text.
    ///
    /// # Errors
    ///
    /// Fails if the image cannot be read, is not a valid zip, has no scatter
    /// file, or has several and `scatter` does not pick one.
    pub fn open(path: &Path, scatter: Option<&str>) -> Result<(Package, String)> {
        if path.is_file() && path.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip")) {
            let mut zip = zip::ZipArchive::new(File::open(path)?).context("not a valid zip file")?;
            let names: Vec<String> = zip.file_names().filter(|n| is_scatter(n)).map(String::from).collect();
            let scatter = pick(names, |n| n.clone(), scatter, path)?;
            let prefix = scatter.rsplit_once('/').map(|(d, _)| format!("{d}/")).unwrap_or_default();
            let mut text = String::new();
            zip.by_name(&scatter)?.read_to_string(&mut text)?;
            return Ok((Package::Zip { zip, prefix }, text));
        }
        let scatter = if path.is_file() {
            path.to_path_buf()
        } else {
            pick(find_scatters(path)?, |p| p.to_string_lossy().into_owned(), scatter, path)?
        };
        let text = std::fs::read_to_string(&scatter).with_context(|| format!("cannot read {}", scatter.display()))?;
        Ok((Package::Dir(scatter.parent().unwrap_or(Path::new(".")).to_path_buf()), text))
    }

    /// Look up the size of a file next to the scatter.
    ///
    /// # Arguments
    ///
    /// * `name` - file name as written in the scatter
    ///
    /// # Returns
    ///
    /// The size in bytes, or None.
    pub fn size(&mut self, name: &str) -> Option<u64> {
        match self {
            Package::Dir(dir) => std::fs::metadata(dir.join(name)).ok().map(|m| m.len()),
            Package::Zip { zip, prefix } => zip.by_name(&format!("{prefix}{name}")).ok().map(|f| f.size()),
        }
    }

    /// Run a function with a reader over a file next to the scatter.
    ///
    /// # Arguments
    ///
    /// * `name` - file name as written in the scatter
    /// * `f` - gets the reader and returns the result
    ///
    /// # Returns
    ///
    /// What `f` returns.
    ///
    /// # Errors
    ///
    /// Fails if the file is missing, or with the error `f` returns.
    pub fn read<T>(&mut self, name: &str, f: impl FnOnce(&mut dyn Read) -> Result<T>) -> Result<T> {
        match self {
            Package::Dir(dir) => f(&mut File::open(dir.join(name)).with_context(|| format!("missing {name}"))?),
            Package::Zip { zip, prefix } => f(&mut zip.by_name(&format!("{prefix}{name}"))?),
        }
    }
}

/// Check whether a path looks like a scatter file (`*scatter*.txt`).
///
/// # Arguments
///
/// * `name` - file path or zip entry name
///
/// # Returns
///
/// True for scatter files.
fn is_scatter(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.ends_with(".txt") && lower.rsplit('/').next().is_some_and(|n| n.contains("scatter"))
}

/// Find all scatter files in a folder and its subfolders.
///
/// # Arguments
///
/// * `dir` - folder to search
///
/// # Returns
///
/// The paths found.
fn find_scatters(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut found = vec![];
    for entry in std::fs::read_dir(dir).with_context(|| format!("cannot open {}", dir.display()))? {
        let p = entry?.path();
        if p.is_dir() {
            found.extend(find_scatters(&p)?);
        } else if is_scatter(&p.to_string_lossy()) {
            found.push(p);
        }
    }
    Ok(found)
}

/// Choose the scatter file: the one named `wanted` (file name, any case),
/// or else the only one found.
///
/// # Arguments
///
/// * `items` - the scatter files found
/// * `path_of` - gives an item's path
/// * `wanted` - the file name asked for, if any
/// * `image` - the image, for the error message
///
/// # Returns
///
/// The chosen item.
///
/// # Errors
///
/// Fails if none is found, `wanted` matches none, or there are several and
/// `wanted` is not given.
fn pick<T>(mut items: Vec<T>, path_of: impl Fn(&T) -> String, wanted: Option<&str>, image: &Path) -> Result<T> {
    let name = |item: &T| path_of(item).rsplit(['/', '\\']).next().unwrap_or_default().to_string();
    let names = || items.iter().map(name).collect::<Vec<_>>().join(", ");
    if let Some(wanted) = wanted {
        return match items.iter().position(|i| name(i).eq_ignore_ascii_case(wanted)) {
            Some(i) => Ok(items.swap_remove(i)),
            None => bail!("{} has no scatter file {wanted}. It has {}", image.display(), names()),
        };
    }
    match items.len() {
        1 => Ok(items.remove(0)),
        0 => bail!("no scatter file found in {}", image.display()),
        _ => bail!(
            "{} has several scatter files: {}. Choose one with --scatter. Check the firmware's \
             instructions: one may be for a first install and another for keeping your data",
            image.display(),
            names()
        ),
    }
}
