//! Byte-level link to the device: reads with a deadline and big-endian helpers.

use anyhow::{bail, Context, Result};
use std::io::{ErrorKind, Read, Write};
use std::time::{Duration, Instant};

/// Anything that moves bytes to and from the device (serial port).
pub trait Port: Read + Write + Send {}
impl<T: Read + Write + Send> Port for T {}

/// Blocking byte link to the device with a read timeout.
pub struct Link {
    io: Box<dyn Port>,
    /// How long `read` waits for the requested bytes.
    pub timeout: Duration,
}

impl Link {
    /// Wrap a port with the default 5 s read timeout.
    ///
    /// # Arguments
    ///
    /// * `io` - the opened serial port
    ///
    /// # Returns
    ///
    /// The link.
    pub fn new(io: Box<dyn Port>) -> Self {
        Self { io, timeout: Duration::from_secs(5) }
    }

    /// Send bytes to the device.
    ///
    /// # Arguments
    ///
    /// * `data` - bytes to send
    ///
    /// # Errors
    ///
    /// Returns an error if the port fails.
    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        self.io.write_all(data).context("USB write failed")?;
        self.io.flush().ok();
        Ok(())
    }

    /// Return whatever the device has sent so far, possibly nothing.
    ///
    /// # Returns
    ///
    /// The bytes received, possibly none.
    ///
    /// # Errors
    ///
    /// Fails if the port reports an error other than a timeout.
    pub fn read_some(&mut self) -> Result<Vec<u8>> {
        let mut buf = [0u8; 256];
        match self.io.read(&mut buf) {
            Ok(n) => Ok(buf[..n].to_vec()),
            Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => Ok(vec![]),
            Err(e) => Err(e).context("USB read failed"),
        }
    }

    /// Read exactly `n` bytes, failing once `timeout` passes.
    ///
    /// # Arguments
    ///
    /// * `n` - number of bytes to read
    ///
    /// # Returns
    ///
    /// The `n` bytes.
    ///
    /// # Errors
    ///
    /// Fails if the port fails or fewer bytes arrive before `timeout`.
    pub fn read(&mut self, n: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; n];
        let mut got = 0;
        let deadline = Instant::now() + self.timeout;
        while got < n {
            match self.io.read(&mut buf[got..]) {
                Ok(k) => got += k,
                Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock | ErrorKind::Interrupted) => {}
                Err(e) => return Err(e).context("USB read failed"),
            }
            if got < n && Instant::now() > deadline {
                bail!("device stopped answering ({got} of {n} bytes received)");
            }
        }
        Ok(buf)
    }

    /// Read one byte.
    ///
    /// # Returns
    ///
    /// The byte.
    ///
    /// # Errors
    ///
    /// Fails if the port fails or no byte arrives before `timeout`.
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.read(1)?[0])
    }

    /// Read a big-endian 16-bit value.
    ///
    /// # Returns
    ///
    /// The value.
    ///
    /// # Errors
    ///
    /// Fails if the port fails or fewer bytes arrive before `timeout`.
    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.read(2)?.try_into().unwrap()))
    }

    /// Read a big-endian 32-bit value.
    ///
    /// # Returns
    ///
    /// The value.
    ///
    /// # Errors
    ///
    /// Fails if the port fails or fewer bytes arrive before `timeout`.
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.read(4)?.try_into().unwrap()))
    }

    /// Send bytes and require the device to echo them back unchanged.
    ///
    /// # Arguments
    ///
    /// * `data` - bytes to send and expect back
    ///
    /// # Errors
    ///
    /// Returns an error if the device answers anything else.
    pub fn echo(&mut self, data: &[u8]) -> Result<()> {
        self.write(data)?;
        let back = self.read(data.len())?;
        if back != data {
            bail!("device answered {back:02x?} instead of echoing {data:02x?}");
        }
        Ok(())
    }

    /// Send a big-endian 32-bit value and require the device to echo it back.
    ///
    /// # Arguments
    ///
    /// * `value` - the value to send and expect back
    ///
    /// # Errors
    ///
    /// Returns an error if the device answers anything else.
    pub fn echo_u32(&mut self, value: u32) -> Result<()> {
        self.echo(&value.to_be_bytes())
    }

    /// Read one byte and require it to be `want`.
    ///
    /// # Arguments
    ///
    /// * `want` - the expected byte
    /// * `what` - what is being waited for, for the error message
    ///
    /// # Errors
    ///
    /// Returns an error if a different byte arrives.
    pub fn expect(&mut self, want: u8, what: &str) -> Result<()> {
        let got = self.u8()?;
        if got != want {
            bail!("{what}: expected {want:#04x}, got {got:#04x}");
        }
        Ok(())
    }
}
