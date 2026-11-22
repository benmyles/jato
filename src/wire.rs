//! The byte layout shared by stored records and by the messages between
//! a build's supervisor and the compiler wrappers it serves.
use crate::scan::Digest;
use anyhow::{Context, Result, bail, ensure};
use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

pub struct Writer(pub Vec<u8>);

impl Writer {
    pub fn number(&mut self, mut value: u64) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                return self.0.push(byte);
            }
            self.0.push(byte | 0x80);
        }
    }
    pub fn bytes(&mut self, bytes: &[u8]) {
        self.number(bytes.len() as u64);
        self.0.extend_from_slice(bytes);
    }
    pub fn path(&mut self, root: u8, path: &Path) {
        self.0.push(root);
        self.bytes(path.as_os_str().as_bytes());
    }
    pub fn optional(&mut self, bytes: Option<&[u8]>) {
        match bytes {
            Some(bytes) => {
                self.0.push(1);
                self.bytes(bytes)
            }
            None => self.0.push(0),
        }
    }
}

pub struct Reader<'a>(pub &'a [u8]);

impl<'a> Reader<'a> {
    pub fn byte(&mut self) -> Result<u8> {
        let (first, rest) = self.0.split_first().context("truncated record")?;
        self.0 = rest;
        Ok(*first)
    }
    pub fn number(&mut self) -> Result<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = self.byte()?;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        bail!("malformed number")
    }
    pub fn count(&mut self) -> Result<usize> {
        let n = self.number()? as usize;
        // Every item takes at least a byte.
        ensure!(n <= self.0.len(), "malformed count");
        Ok(n)
    }
    pub fn bytes(&mut self) -> Result<&'a [u8]> {
        let len = self.number()? as usize;
        ensure!(len <= self.0.len(), "truncated record");
        let (bytes, rest) = self.0.split_at(len);
        self.0 = rest;
        Ok(bytes)
    }
    pub fn path(&mut self) -> Result<(u8, PathBuf)> {
        let root = self.byte()?;
        ensure!(root <= crate::roots::TARGET, "unknown root");
        Ok((root, PathBuf::from(OsStr::from_bytes(self.bytes()?))))
    }
    pub fn optional(&mut self) -> Result<Option<&'a [u8]>> {
        Ok(match self.byte()? {
            0 => None,
            _ => Some(self.bytes()?),
        })
    }
    pub fn digest(&mut self) -> Result<Digest> {
        ensure!(self.0.len() >= 32, "truncated record");
        let (digest, rest) = self.0.split_at(32);
        self.0 = rest;
        Ok(digest.try_into().unwrap())
    }
}

/// The largest message either side accepts.
const MAX_MESSAGE: u32 = 256 << 20;

pub fn send(stream: &mut impl Write, message: &[u8]) -> io::Result<()> {
    let len = u32::try_from(message.len())
        .ok()
        .filter(|len| *len <= MAX_MESSAGE)
        .ok_or_else(|| io::Error::other("message too large"))?;
    stream.write_all(&len.to_le_bytes())?;
    stream.write_all(message)?;
    stream.flush()
}

pub fn receive(stream: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len);
    if len > MAX_MESSAGE {
        return Err(io::Error::other("message too large"));
    }
    let mut message = vec![0u8; len as usize];
    stream.read_exact(&mut message)?;
    Ok(message)
}
