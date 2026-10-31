//! Android sparse images (`system.img`, `userdata.img`), expanded on the fly.

use crate::{le16, le32};
use std::io::{self, Read};

const MAGIC: u32 = 0xED26_FF3A;
const RAW: u16 = 0xCAC1;
const FILL: u16 = 0xCAC2;
const DONT_CARE: u16 = 0xCAC3;
const CRC: u16 = 0xCAC4;

pub const HEADER_LEN: usize = 28;

/// Compute the expanded size, if the bytes start a sparse image.
///
/// # Arguments
///
/// * `header` - the file's first bytes (at least `HEADER_LEN`)
///
/// # Returns
///
/// The size in bytes, or None if it is not a sparse image.
pub fn expanded_size(header: &[u8]) -> Option<u64> {
    if header.len() < HEADER_LEN || le32(header, 0) != MAGIC {
        return None;
    }
    Some(le32(header, 12) as u64 * le32(header, 16) as u64)
}

/// Reads a sparse image (whose 28-byte header was already consumed) as raw bytes.
pub struct SparseReader<R> {
    inner: R,
    block_size: u64,
    file_header_extra: usize,
    chunk_header_len: usize,
    chunks_left: u32,
    kind: u16,
    left: u64,
    fill: [u8; 4],
    pos: u64,
}

impl<R: Read> SparseReader<R> {
    /// Start reading the chunks that follow the header.
    ///
    /// # Arguments
    ///
    /// * `inner` - the file, positioned right after its header
    /// * `header` - the 28-byte header already read from it
    ///
    /// # Returns
    ///
    /// The reader.
    pub fn new(inner: R, header: &[u8]) -> Self {
        let file_header_len = le16(header, 8) as usize;
        Self {
            inner,
            block_size: le32(header, 12) as u64,
            file_header_extra: file_header_len.saturating_sub(HEADER_LEN),
            chunk_header_len: le16(header, 10) as usize,
            chunks_left: le32(header, 20),
            kind: DONT_CARE,
            left: 0,
            fill: [0; 4],
            pos: 0,
        }
    }

    /// Read the next chunk header; returns false at the end of the image.
    ///
    /// # Returns
    ///
    /// True if a chunk with data follows.
    fn next_chunk(&mut self) -> io::Result<bool> {
        if self.file_header_extra > 0 {
            skip(&mut self.inner, std::mem::take(&mut self.file_header_extra) as u64)?;
        }
        loop {
            if self.chunks_left == 0 {
                return Ok(false);
            }
            self.chunks_left -= 1;
            let mut h = vec![0u8; self.chunk_header_len.max(12)];
            self.inner.read_exact(&mut h)?;
            let (kind, blocks, total) = (le16(&h, 0), le32(&h, 4) as u64, le32(&h, 8) as u64);
            let body = total.saturating_sub(self.chunk_header_len as u64);
            self.kind = kind;
            self.left = blocks * self.block_size;
            self.pos = 0;
            match kind {
                RAW => {}
                FILL => {
                    self.inner.read_exact(&mut self.fill)?;
                    skip(&mut self.inner, body.saturating_sub(4))?;
                }
                DONT_CARE => skip(&mut self.inner, body)?,
                CRC => {
                    skip(&mut self.inner, body)?;
                    continue;
                }
                k => return Err(io::Error::new(io::ErrorKind::InvalidData, format!("unknown sparse chunk {k:#x}"))),
            }
            if self.left > 0 {
                return Ok(true);
            }
        }
    }
}

impl<R: Read> Read for SparseReader<R> {
    /// Produce expanded bytes: raw data, the fill pattern, or zeros.
    ///
    /// # Arguments
    ///
    /// * `buf` - where to put them
    ///
    /// # Returns
    ///
    /// The number of bytes produced; 0 at the end of the image.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.left == 0 && !self.next_chunk()? {
            return Ok(0);
        }
        let n = (buf.len() as u64).min(self.left) as usize;
        let n = match self.kind {
            RAW => self.inner.read(&mut buf[..n])?,
            FILL => {
                for (i, b) in buf[..n].iter_mut().enumerate() {
                    *b = self.fill[((self.pos + i as u64) % 4) as usize];
                }
                n
            }
            _ => {
                buf[..n].fill(0);
                n
            }
        };
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.left -= n as u64;
        self.pos += n as u64;
        Ok(n)
    }
}

/// Read and discard bytes.
///
/// # Arguments
///
/// * `r` - the source
/// * `n` - how many bytes to skip
///
/// # Errors
///
/// Returns an error if the source ends first.
fn skip(r: &mut impl Read, n: u64) -> io::Result<()> {
    let copied = io::copy(&mut r.take(n), &mut io::sink())?;
    if copied != n {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(())
}
