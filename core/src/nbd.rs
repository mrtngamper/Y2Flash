//! Mount a partition through the kernel's NBD driver (Linux only, needs root).
//!
//! The driver is attached to one end of a socket pair with its classic ioctl
//! interface (as `nbd-client` does after its handshake), and this process
//! answers the driver's requests on the other end. The device `/dev/nbdN` is
//! then mounted with `mount`. Ctrl-C unmounts it and disconnects the driver,
//! which ends [`run`].

use crate::Event;
use anyhow::{bail, Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

// ioctls from linux/nbd.h: _IO(0xab, n).
const SET_SOCK: u64 = 0xAB00;
const SET_BLKSIZE: u64 = 0xAB01;
const DO_IT: u64 = 0xAB03;
const CLEAR_SOCK: u64 = 0xAB04;
const CLEAR_QUE: u64 = 0xAB05;
const SET_SIZE_BLOCKS: u64 = 0xAB07;
const DISCONNECT: u64 = 0xAB08;
const SET_TIMEOUT: u64 = 0xAB09;
const SET_FLAGS: u64 = 0xAB0A;

const FLAG_HAS_FLAGS: u64 = 1;
const FLAG_READ_ONLY: u64 = 2;
const FLAG_SEND_FLUSH: u64 = 4;

const REQUEST_MAGIC: u32 = 0x2560_9513;
const REPLY_MAGIC: u32 = 0x6744_6698;
const CMD_READ: u32 = 0;
const CMD_WRITE: u32 = 1;
const CMD_DISC: u32 = 2;
const CMD_FLUSH: u32 = 3;

const EIO: u32 = 5;
const EPERM: u32 = 1;
const EINVAL: u32 = 22;

/// Seconds the driver waits for one request before failing it.
const TIMEOUT_SECS: u64 = 120;

/// Set by the SIGINT handler while [`run`] serves.
static STOP: AtomicBool = AtomicBool::new(false);

/// What [`run`] serves: a range of the eMMC, addressed from 0.
pub trait Disk {
    /// Read `len` bytes at `offset`.
    ///
    /// # Arguments
    ///
    /// * `offset` - start inside the disk
    /// * `len` - bytes to read
    ///
    /// # Returns
    ///
    /// The bytes read.
    ///
    /// # Errors
    ///
    /// Fails if the device read fails.
    fn read(&mut self, offset: u64, len: u32) -> Result<Vec<u8>>;

    /// Write `data` at `offset`.
    ///
    /// # Arguments
    ///
    /// * `offset` - start inside the disk
    /// * `data` - the bytes, whole 512-byte sectors
    ///
    /// # Errors
    ///
    /// Fails if the device write fails.
    fn write(&mut self, offset: u64, data: &[u8]) -> Result<()>;
}

/// Messages from the helper threads to the serving loop.
enum Note {
    Log(String),
    /// The mount failed; the driver is being disconnected.
    Failed(anyhow::Error),
}

/// Check, before the device is caught, that a mount can work: root, an
/// existing directory, the nbd module (loaded if needed) and a free device.
///
/// # Arguments
///
/// * `dir` - where the partition will be mounted
///
/// # Errors
///
/// Fails, saying what is missing, if any of these is not there.
pub fn preflight(dir: &Path) -> Result<()> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        bail!("mounting needs root. Run y2flash with sudo");
    }
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    if !Path::new("/sys/block/nbd0").exists() {
        let loaded = Command::new("modprobe").arg("nbd").status().is_ok_and(|s| s.success());
        if !loaded || !Path::new("/sys/block/nbd0").exists() {
            bail!("the nbd kernel module could not be loaded (modprobe nbd)");
        }
    }
    free_device()?;
    Ok(())
}

/// Make `disk` available as a free /dev/nbdN, mount it on `dir`, and answer
/// the driver's requests until Ctrl-C has unmounted it (or the mount failed).
///
/// # Arguments
///
/// * `disk` - what to serve
/// * `size` - its size in bytes, a multiple of 512
/// * `dir` - where to mount it
/// * `writable` - allow writes; otherwise the device and mount are read-only
/// * `options` - extra mount options, comma-separated
/// * `on` - receives log messages
///
/// # Errors
///
/// Fails if the driver cannot be attached, the mount fails, or the socket breaks.
pub fn run(
    disk: &mut dyn Disk,
    size: u64,
    dir: &Path,
    writable: bool,
    options: Option<&str>,
    on: &mut dyn FnMut(Event),
) -> Result<()> {
    let dev = free_device()?;
    let file = OpenOptions::new().read(true).write(true).open(&dev).with_context(|| format!("opening {}", dev.display()))?;
    let (mut ours, theirs) = UnixStream::pair()?;
    let flags = FLAG_HAS_FLAGS | FLAG_SEND_FLUSH | if writable { 0 } else { FLAG_READ_ONLY };
    ioctl(&file, SET_BLKSIZE, 512)?;
    ioctl(&file, SET_SIZE_BLOCKS, size / 512)?;
    ioctl(&file, SET_FLAGS, flags)?;
    ioctl(&file, CLEAR_SOCK, 0)?;
    ioctl(&file, SET_SOCK, theirs.as_raw_fd() as u64)?;
    ioctl(&file, SET_TIMEOUT, TIMEOUT_SECS)?;

    // The driver runs inside this ioctl until it is disconnected.
    let doit_file = file.try_clone()?;
    // Helper threads block SIGINT: a Ctrl-C that reached DO_IT would stop the driver.
    let doit = spawn_without_sigint(move || {
        let result = ioctl(&doit_file, DO_IT, 0);
        ioctl(&doit_file, CLEAR_QUE, 0).ok();
        ioctl(&doit_file, CLEAR_SOCK, 0).ok();
        drop(theirs);
        result
    });

    let (tx, notes) = mpsc::channel();
    let mount = {
        let (tx, dev, dir) = (tx.clone(), dev.clone(), dir.to_path_buf());
        let mut opts = vec![if writable { "rw" } else { "ro" }.to_string()];
        opts.extend(options.map(str::to_string));
        spawn_without_sigint(move || mount_when_ready(&dev, &dir, &opts.join(","), &tx))
    };

    set_sigint(true);
    let served = serve(&mut ours, disk, size, writable, &notes, &mut |stop| {
        if stop {
            let (tx, dev, dir) = (tx.clone(), dev.clone(), dir.to_path_buf());
            spawn_without_sigint(move || unmount(&dev, &dir, &tx));
        }
    }, on);
    set_sigint(false);
    drop(ours);
    mount.join().ok();
    if let Ok(Err(e)) = doit.join() {
        on(Event::Log(format!("The nbd driver stopped with an error. {}", crate::describe(&e))));
    }
    let mut failed = served?;
    for note in notes.try_iter() {
        match note {
            Note::Log(m) => on(Event::Log(m)),
            Note::Failed(e) => failed = Some(e),
        }
    }
    failed.map_or(Ok(()), Err)
}

/// Answer the driver's requests until it disconnects or closes the socket.
/// Between requests, log the helper threads' notes and report Ctrl-C.
///
/// # Arguments
///
/// * `sock` - our end of the socket pair
/// * `disk` - what to serve
/// * `size` - its size in bytes
/// * `writable` - whether writes are allowed
/// * `notes` - messages from the helper threads
/// * `on_stop` - called with true when Ctrl-C was pressed
/// * `on` - receives log messages
///
/// # Returns
///
/// The mount failure a helper thread reported, if any.
///
/// # Errors
///
/// Fails if the socket breaks or a request is malformed.
fn serve(
    sock: &mut UnixStream,
    disk: &mut dyn Disk,
    size: u64,
    writable: bool,
    notes: &mpsc::Receiver<Note>,
    on_stop: &mut dyn FnMut(bool),
    on: &mut dyn FnMut(Event),
) -> Result<Option<anyhow::Error>> {
    let mut failed = None;
    loop {
        for note in notes.try_iter() {
            match note {
                Note::Log(m) => on(Event::Log(m)),
                Note::Failed(e) => failed = Some(e),
            }
        }
        on_stop(STOP.swap(false, Ordering::SeqCst));
        if !readable(sock, Duration::from_millis(200))? {
            continue;
        }
        let mut h = [0u8; 28];
        match sock.read_exact(&mut h) {
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => break,
            r => r.context("reading an nbd request")?,
        }
        let be32 = |o: usize| u32::from_be_bytes(h[o..o + 4].try_into().unwrap());
        if be32(0) != REQUEST_MAGIC {
            bail!("bad nbd request magic {:#x}", be32(0));
        }
        let cmd = be32(4) & 0xFFFF;
        let handle: [u8; 8] = h[8..16].try_into().unwrap();
        let offset = u64::from_be_bytes(h[16..24].try_into().unwrap());
        let len = be32(24);
        let in_range = offset.checked_add(len as u64).is_some_and(|end| end <= size);
        match cmd {
            CMD_READ => {
                let data = if in_range { disk.read(offset, len).map_err(Some) } else { Err(None) };
                match data {
                    Ok(data) => {
                        reply(sock, handle, 0)?;
                        sock.write_all(&data)?;
                    }
                    Err(e) => {
                        if let Some(e) = e {
                            on(Event::Log(format!("Reading {len} bytes at {offset:#x} failed. {}", crate::describe(&e))));
                        }
                        reply(sock, handle, if in_range { EIO } else { EINVAL })?;
                    }
                }
            }
            CMD_WRITE => {
                let mut data = vec![0u8; len as usize];
                sock.read_exact(&mut data)?;
                let error = if !writable {
                    EPERM
                } else if !in_range {
                    EINVAL
                } else if let Err(e) = disk.write(offset, &data) {
                    on(Event::Log(format!("Writing {len} bytes at {offset:#x} failed. {}", crate::describe(&e))));
                    EIO
                } else {
                    0
                };
                reply(sock, handle, error)?;
            }
            CMD_DISC => break,
            CMD_FLUSH => reply(sock, handle, 0)?, // writes are done when acknowledged
            _ => reply(sock, handle, EINVAL)?,
        }
    }
    Ok(failed)
}

/// Send a reply header.
///
/// # Arguments
///
/// * `sock` - our end of the socket pair
/// * `handle` - the request's handle
/// * `error` - 0 or an errno value
///
/// # Errors
///
/// Fails if the socket breaks.
fn reply(sock: &mut UnixStream, handle: [u8; 8], error: u32) -> Result<()> {
    let mut r = Vec::with_capacity(16);
    r.extend(REPLY_MAGIC.to_be_bytes());
    r.extend(error.to_be_bytes());
    r.extend(handle);
    sock.write_all(&r)?;
    Ok(())
}

/// Wait until the driver runs and the device has its size, then mount it.
/// On failure, disconnect the driver so that [`serve`] ends.
///
/// # Arguments
///
/// * `dev` - the nbd device
/// * `dir` - where to mount it
/// * `options` - mount options, comma-separated
/// * `tx` - receives the outcome
fn mount_when_ready(dev: &Path, dir: &Path, options: &str, tx: &mpsc::Sender<Note>) {
    let name = dev.file_name().unwrap_or_default().to_string_lossy();
    let sys = PathBuf::from(format!("/sys/block/{name}"));
    // The driver creates `pid` just before it sets the size, so wait for both:
    // a mount in between sees an empty device.
    let ready = || {
        let size = std::fs::read_to_string(sys.join("size")).unwrap_or_default();
        sys.join("pid").exists() && size.trim().parse::<u64>().is_ok_and(|s| s > 0)
    };
    let mut waited = 0;
    while !ready() {
        if waited >= 100 {
            tx.send(Note::Failed(anyhow::anyhow!("{} did not start within 10 seconds", dev.display()))).ok();
            disconnect(dev, tx);
            return;
        }
        thread::sleep(Duration::from_millis(100));
        waited += 1;
    }
    let result = Command::new("mount").args(["-o", options]).arg(dev).arg(dir).output();
    match result {
        Ok(out) if out.status.success() => {
            tx.send(Note::Log(format!(
                "Mounted {} on {} ({options}). Press Ctrl-C to unmount it and let the player restart.",
                dev.display(),
                dir.display()
            )))
            .ok();
        }
        other => {
            let why = match other {
                Ok(out) => String::from_utf8_lossy(&out.stderr).trim().to_string(),
                Err(e) => e.to_string(),
            };
            tx.send(Note::Failed(anyhow::anyhow!("mounting {} failed: {why}", dev.display()))).ok();
            disconnect(dev, tx);
        }
    }
}

/// Unmount the device if it is mounted, then disconnect the driver. If the
/// unmount fails (the directory is busy), say so and keep serving.
///
/// # Arguments
///
/// * `dev` - the nbd device
/// * `dir` - where it is mounted
/// * `tx` - receives messages
fn unmount(dev: &Path, dir: &Path, tx: &mpsc::Sender<Note>) {
    if is_mounted(dev) {
        tx.send(Note::Log(format!("Unmounting {}.", dir.display()))).ok();
        match Command::new("umount").arg(dir).output() {
            Ok(out) if out.status.success() => {}
            other => {
                let why = match other {
                    Ok(out) => String::from_utf8_lossy(&out.stderr).trim().to_string(),
                    Err(e) => e.to_string(),
                };
                tx.send(Note::Log(format!(
                    "Unmounting failed: {why}. Close what uses {} and press Ctrl-C again.",
                    dir.display()
                )))
                .ok();
                return;
            }
        }
    }
    disconnect(dev, tx);
}

/// Ask the driver to disconnect; it then sends a disconnect request.
///
/// # Arguments
///
/// * `dev` - the nbd device
/// * `tx` - receives a message if it fails
fn disconnect(dev: &Path, tx: &mpsc::Sender<Note>) {
    let result = File::open(dev).map_err(anyhow::Error::from).and_then(|f| ioctl(&f, DISCONNECT, 0));
    if let Err(e) = result {
        tx.send(Note::Log(format!("Disconnecting {} failed. {}", dev.display(), crate::describe(&e)))).ok();
    }
}

/// Tell whether a device is mounted anywhere.
///
/// # Arguments
///
/// * `dev` - the device
///
/// # Returns
///
/// True if /proc/mounts lists it.
fn is_mounted(dev: &Path) -> bool {
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    mounts.lines().any(|l| l.split_whitespace().next() == Some(&*dev.to_string_lossy()))
}

/// Find an nbd device no driver is attached to.
///
/// # Returns
///
/// Its /dev path.
///
/// # Errors
///
/// Fails if every nbd device is in use.
fn free_device() -> Result<PathBuf> {
    for i in 0.. {
        let sys = PathBuf::from(format!("/sys/block/nbd{i}"));
        if !sys.exists() {
            break;
        }
        let size = std::fs::read_to_string(sys.join("size")).unwrap_or_default();
        if !sys.join("pid").exists() && size.trim() == "0" {
            return Ok(PathBuf::from(format!("/dev/nbd{i}")));
        }
    }
    bail!("every /dev/nbd device is in use")
}

/// Run an NBD ioctl that takes an integer argument.
///
/// # Arguments
///
/// * `dev` - the open nbd device
/// * `request` - the ioctl number
/// * `arg` - its argument
///
/// # Errors
///
/// Fails with the OS error if the ioctl fails.
fn ioctl(dev: &File, request: u64, arg: u64) -> Result<()> {
    // SAFETY: these NBD ioctls take an integer argument, never a pointer.
    let r = unsafe { libc::ioctl(dev.as_raw_fd(), request as _, arg as libc::c_ulong) };
    if r < 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("nbd ioctl {request:#x}"));
    }
    Ok(())
}

/// Wait until the socket has data or is closed.
///
/// # Arguments
///
/// * `sock` - our end of the socket pair
/// * `timeout` - how long to wait
///
/// # Returns
///
/// True if a read would not block.
///
/// # Errors
///
/// Fails if poll fails for another reason than a signal.
fn readable(sock: &UnixStream, timeout: Duration) -> Result<bool> {
    let mut fd = libc::pollfd { fd: sock.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    // SAFETY: one valid pollfd for the call's duration.
    let r = unsafe { libc::poll(&mut fd, 1, timeout.as_millis() as libc::c_int) };
    if r < 0 {
        let e = std::io::Error::last_os_error();
        return if e.kind() == ErrorKind::Interrupted { Ok(false) } else { Err(e.into()) };
    }
    Ok(r > 0)
}

/// Start a thread that never receives SIGINT, so Ctrl-C reaches the serving thread.
///
/// # Arguments
///
/// * `f` - the thread's work
///
/// # Returns
///
/// The thread's handle.
fn spawn_without_sigint<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> thread::JoinHandle<T> {
    // SAFETY: plain signal-mask calls on a zero-initialised sigset_t.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        let mut old: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        // The new thread inherits the blocked mask; this thread gets its own back.
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut old);
        let handle = thread::spawn(f);
        libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
        handle
    }
}

/// Make Ctrl-C set [`STOP`] instead of ending the process, or restore the default.
///
/// # Arguments
///
/// * `catch` - true to catch SIGINT, false for the default action
fn set_sigint(catch: bool) {
    extern "C" fn on_sigint(_: libc::c_int) {
        STOP.store(true, Ordering::SeqCst);
    }
    let handler = if catch { on_sigint as extern "C" fn(libc::c_int) as libc::sighandler_t } else { libc::SIG_DFL };
    // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
    unsafe {
        libc::signal(libc::SIGINT, handler);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An in-memory disk.
    struct Mem(Vec<u8>);

    impl Disk for Mem {
        fn read(&mut self, offset: u64, len: u32) -> Result<Vec<u8>> {
            Ok(self.0[offset as usize..offset as usize + len as usize].to_vec())
        }
        fn write(&mut self, offset: u64, data: &[u8]) -> Result<()> {
            self.0[offset as usize..offset as usize + data.len()].copy_from_slice(data);
            Ok(())
        }
    }

    fn request(cmd: u32, handle: u64, offset: u64, len: u32) -> Vec<u8> {
        let mut r = REQUEST_MAGIC.to_be_bytes().to_vec();
        r.extend(cmd.to_be_bytes());
        r.extend(handle.to_be_bytes());
        r.extend(offset.to_be_bytes());
        r.extend(len.to_be_bytes());
        r
    }

    fn read_reply(sock: &mut UnixStream) -> (u32, u64) {
        let mut r = [0u8; 16];
        sock.read_exact(&mut r).unwrap();
        assert_eq!(&r[..4], REPLY_MAGIC.to_be_bytes());
        (u32::from_be_bytes(r[4..8].try_into().unwrap()), u64::from_be_bytes(r[8..].try_into().unwrap()))
    }

    /// Serve a 4 KiB disk on a thread and return the kernel's end and the thread.
    /// What the serving thread returns: the outcome and the disk's final bytes.
    type Served = (Result<Option<anyhow::Error>>, Vec<u8>);

    fn start(writable: bool) -> (UnixStream, thread::JoinHandle<Served>) {
        let (mut ours, theirs) = UnixStream::pair().unwrap();
        let handle = thread::spawn(move || {
            let mut disk = Mem((0..4096u32).map(|i| i as u8).collect());
            let (_tx, notes) = mpsc::channel();
            let r = serve(&mut ours, &mut disk, 4096, writable, &notes, &mut |_| {}, &mut |_| {});
            (r, disk.0)
        });
        (theirs, handle)
    }

    #[test]
    fn serves_reads_writes_and_disconnect() {
        let (mut k, server) = start(true);
        k.write_all(&request(CMD_READ, 7, 512, 4)).unwrap();
        assert_eq!(read_reply(&mut k), (0, 7));
        let mut data = [0u8; 4];
        k.read_exact(&mut data).unwrap();
        assert_eq!(data, [0, 1, 2, 3]);

        k.write_all(&request(CMD_WRITE, 8, 1024, 512)).unwrap();
        k.write_all(&[0xEE; 512]).unwrap();
        assert_eq!(read_reply(&mut k), (0, 8));

        k.write_all(&request(CMD_READ, 9, 4000, 512)).unwrap();
        assert_eq!(read_reply(&mut k), (EINVAL, 9));

        k.write_all(&request(CMD_DISC, 10, 0, 0)).unwrap();
        let (r, disk) = server.join().unwrap();
        assert!(r.unwrap().is_none());
        assert!(disk[1024..1536].iter().all(|&b| b == 0xEE));
    }

    /// Mount a FAT image through the real nbd driver, read a file on it, and
    /// unmount it as Ctrl-C would. Needs root and mkfs.vfat/mcopy, so it is
    /// ignored by default: build with `cargo test --no-run`, then run the test
    /// binary it names with `sudo <binary> --ignored mounts_fat_image`.
    #[test]
    #[ignore]
    fn mounts_fat_image() {
        let tmp = std::env::temp_dir().join(format!("y2flash-nbd-test-{}", std::process::id()));
        let (img, dir) = (tmp.join("fat.img"), tmp.join("mnt"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&img, vec![0u8; 8 << 20]).unwrap();
        std::fs::write(tmp.join("hello.txt"), "hello from y2flash\n").unwrap();
        assert!(Command::new("mkfs.vfat").arg(&img).status().unwrap().success());
        assert!(Command::new("mcopy").arg("-i").arg(&img).arg(tmp.join("hello.txt")).arg("::/").status().unwrap().success());
        preflight(&dir).unwrap();

        let mut disk = Mem(std::fs::read(&img).unwrap());
        let file = dir.join("HELLO.TXT");
        let checker = {
            let (dir, file) = (dir.clone(), file.clone());
            spawn_without_sigint(move || {
                let mounted = || std::fs::read_to_string("/proc/mounts").unwrap().contains(&*dir.to_string_lossy());
                for _ in 0..100 {
                    if mounted() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
                let text = std::fs::read_to_string(&file).or_else(|_| std::fs::read_to_string(dir.join("hello.txt")));
                STOP.store(true, Ordering::SeqCst);
                text
            })
        };
        let size = disk.0.len() as u64;
        run(&mut disk, size, &dir, false, None, &mut |e| println!("{e:?}")).unwrap();
        assert_eq!(checker.join().unwrap().unwrap(), "hello from y2flash\n");
        assert!(!std::fs::read_to_string("/proc/mounts").unwrap().contains(&*dir.to_string_lossy()));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn refuses_writes_when_read_only() {
        let (mut k, server) = start(false);
        k.write_all(&request(CMD_WRITE, 1, 0, 512)).unwrap();
        k.write_all(&[0xEE; 512]).unwrap();
        assert_eq!(read_reply(&mut k), (EPERM, 1));
        drop(k);
        let (r, disk) = server.join().unwrap();
        assert!(r.unwrap().is_none());
        assert_eq!(disk[0], 0);
    }
}
