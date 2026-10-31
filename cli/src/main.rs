use anyhow::{bail, Result};
use clap::{Args, Parser, Subcommand};
use y2flash::{catalog, Event, Options};
use std::io::{BufRead, Write};
use std::path::PathBuf;

/// Flash MediaTek firmware images (e.g. Innioasis Y2) over USB.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Folder with download agent files [default: ./loaders next to the program]
    #[arg(long, global = true)]
    loaders: Option<PathBuf>,
    /// Device model, as named in the online catalogue
    #[arg(long, global = true, default_value = "Y2")]
    device: String,
    /// Scatter file to use when the firmware has several, by file name
    /// (e.g. MT6582_preserve_data_scatter.txt)
    #[arg(long, global = true)]
    scatter: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

/// The subcommands.
#[derive(Subcommand)]
enum Cmd {
    /// List the software available online for the device
    Software,
    /// List a software's releases and their firmware files
    Releases {
        /// Software name (see `software`) or GitHub "owner/repo"
        software: String,
    },
    /// Download a firmware file into the cache and print its path
    Download {
        /// Software name (see `software`) or GitHub "owner/repo"
        software: String,
        #[command(flatten)]
        pick: Pick,
    },
    /// Show chip and eMMC information (writes nothing). With a firmware image,
    /// also locate and compare its partition table as a flash would. In
    /// BootROM mode the image is needed: its preloader is started in RAM.
    Info { image: Option<PathBuf> },
    /// Flash a firmware image: a folder, a .zip, a scatter file, or (with
    /// --software) a file downloaded from the online catalogue
    Flash {
        #[arg(required_unless_present = "software")]
        image: Option<PathBuf>,
        /// Download this software (name or "owner/repo") instead of giving an image
        #[arg(long, conflicts_with = "image")]
        software: Option<String>,
        #[command(flatten)]
        pick: Pick,
        /// Only these partitions, comma-separated (e.g. BOOTIMG,RECOVERY)
        #[arg(long, value_delimiter = ',')]
        only: Vec<String>,
        /// Also write the preloader. This is rarely needed, and BootROM mode recovers a bad one
        #[arg(long)]
        with_preloader: bool,
        /// Repartition the player without asking if the firmware's partition layout differs
        /// from the player's. This erases its internal storage, including all music on it
        #[arg(long)]
        repartition: bool,
        /// Print the plan and stop
        #[arg(long)]
        dry_run: bool,
        /// Don't ask before writing. A repartition still needs --repartition
        #[arg(short, long)]
        yes: bool,
    },
}

/// Which release and file of an online software to use.
#[derive(Args)]
struct Pick {
    /// Release tag [default: the newest release]
    #[arg(long, requires = "software")]
    release: Option<String>,
    /// File name [default: the first firmware file for the device]
    #[arg(long, requires = "software")]
    file: Option<String>,
}

/// Run the chosen command and print a failure as sentences.
///
/// # Returns
///
/// Success, or failure if the command failed.
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {}", y2flash::describe(&e));
            std::process::ExitCode::FAILURE
        }
    }
}

/// Parse the command line and run the chosen command.
///
/// # Errors
///
/// Returns an error to print if the command fails.
fn run() -> Result<()> {
    let cli = Cli::parse();
    let loaders = cli.loaders.unwrap_or_else(y2flash::default_loader_dir);
    let device = cli.device;
    let scatter = cli.scatter;
    match cli.cmd {
        Cmd::Software => {
            println!("From {}\n", catalog::MANIFEST_URL);
            for s in catalog::software(&device)? {
                println!("{:<24} {}", s.name, s.url);
            }
        }
        Cmd::Releases { software } => {
            let sw = catalog::find(&software, &device)?;
            println!("{} - {}\n", sw.name, sw.url);
            for r in catalog::releases(&sw.repo, &device)? {
                let pre = if r.prerelease { " (prerelease)" } else { "" };
                println!("{}  {}  {}{pre}\n  {}", r.tag, r.date, r.title, r.url);
                for f in &r.files {
                    println!("    {:<48} {:>6} MiB", f.name, f.size >> 20);
                    println!("      {}", f.url);
                    if let Some(sha) = &f.sha256 {
                        println!("      sha256 {sha}");
                    }
                }
            }
        }
        Cmd::Download { software, pick } => {
            let path = download(&software, &device, &pick)?;
            println!("{}", path.display());
        }
        Cmd::Info { image } => {
            let d = y2flash::info(&loaders, image.as_deref(), scatter.as_deref(), &mut print_event)?;
            println!("{d:#?}");
        }
        Cmd::Flash { image, software, pick, only, with_preloader, repartition, dry_run, yes } => {
            let image = match (image, software) {
                (Some(path), _) => path,
                (None, Some(sw)) => download(&sw, &device, &pick)?,
                (None, None) => unreachable!("clap requires an image or --software"),
            };
            let opts = Options { scatter, only, with_preloader, repartition };
            let steps = y2flash::plan(&image, &opts)?;
            if steps.is_empty() {
                bail!("there is nothing to flash. None of the scatter's image files are in {}", image.display());
            }
            println!("{:<12} {:<34} {:>6} {:>12} {:>10}", "PARTITION", "FILE", "AREA", "ADDRESS", "MiB");
            for s in &steps {
                let file = if s.sparse { format!("{} (sparse)", s.file) } else { s.file.clone() };
                println!("{:<12} {:<34} {:>6?} {:>#12x} {:>10.1}", s.partition, file, s.area, s.addr, s.len as f64 / 1048576.0);
            }
            if dry_run {
                return Ok(());
            }
            if !yes && !confirm("Write these partitions?")? {
                bail!("cancelled");
            }
            // Asked while the player is held and nothing is written yet.
            let mut ask_repartition = |tables: &[String]| {
                println!(
                    "\nWARNING: This firmware uses a different partition layout from the player's ({}).\n\
                     Installing it repartitions the player. Everything in its internal storage is erased,\n\
                     including all music on it. Copy anything you want to keep off the player first.",
                    tables.join(", ")
                );
                !yes && confirm("Repartition the player and erase its internal storage?").unwrap_or(false)
            };
            y2flash::flash(&image, &loaders, &opts, &mut print_event, &mut ask_repartition)?;
        }
    }
    Ok(())
}

/// Resolve a software, pick its release and file, and download it.
///
/// # Arguments
///
/// * `software` - software name or GitHub "owner/repo"
/// * `device` - device model, e.g. "Y2"
/// * `pick` - release tag and file name, each None for the default
///
/// # Returns
///
/// Path of the verified download.
///
/// # Errors
///
/// Fails if the software, release or file cannot be found or downloaded.
fn download(software: &str, device: &str, pick: &Pick) -> Result<PathBuf> {
    let sw = catalog::find(software, device)?;
    let (rel, asset) = catalog::choose(&sw.repo, device, pick.release.as_deref(), pick.file.as_deref())?;
    println!("{} {} ({}): {}", sw.name, rel.tag, rel.date, asset.name);
    catalog::download(&sw.repo, &rel.tag, &asset, &mut print_event)
}

/// Ask a question on the terminal before writing.
///
/// # Arguments
///
/// * `question` - the question, ending with a question mark
///
/// # Returns
///
/// True only if the user typed "yes".
///
/// # Errors
///
/// Fails if the terminal cannot be read or written.
fn confirm(question: &str) -> Result<bool> {
    print!("{question} Type 'yes': ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim() == "yes")
}

/// Print a log line, or update a progress line in place.
///
/// # Arguments
///
/// * `e` - the event to show
fn print_event(e: Event) {
    let (what, done, total) = match e {
        Event::Log(m) => return println!("{m}"),
        Event::Progress { partition, done, total } => (partition, done, total),
        Event::Download { file, done, total } => (file, done, total),
    };
    print!("\r  {what}: {:>3}% ({} / {} MiB)", done * 100 / total.max(1), done >> 20, total >> 20);
    if done >= total {
        println!();
    }
    std::io::stdout().flush().ok();
}
