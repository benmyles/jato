//! Placement hints for packed Rust archives. Every frame is still hashed
//! and verified as raw bytes, including archive headers and padding.
use std::fs::File;
use std::os::unix::fs::FileExt;

const MIN_MEMBER: u64 = 64 * 1024;

pub struct Sidecar {
    prefix: [u8; 32],
    len: u64,
}

impl Sidecar {
    pub fn read(file: &File, len: u64) -> Option<Self> {
        if len < MIN_MEMBER {
            return None;
        }
        let mut prefix = [0; 32];
        file.read_exact_at(&mut prefix, 0).ok()?;
        Some(Self { prefix, len })
    }
}

pub fn boundaries(file: &File, len: u64, sidecar: Option<&Sidecar>) -> Option<Vec<u64>> {
    let mut signature = [0; 8];
    file.read_exact_at(&mut signature, 0).ok()?;
    if &signature != b"!<arch>\n" {
        return None;
    }
    let mut offset = 8;
    let mut cuts = Vec::new();
    let mut header = [0; 60];
    let mut probe = [0; 4096];
    for _ in 0..8192 {
        if offset == len {
            cuts.retain(|&n| n < len);
            return Some(cuts);
        }
        file.read_exact_at(&mut header, offset).ok()?;
        if &header[58..] != b"`\n" {
            return None;
        }
        let size: u64 = std::str::from_utf8(&header[48..58])
            .ok()?
            .trim()
            .parse()
            .ok()?;
        let start = offset.checked_add(60)?;
        let end = start.checked_add(size)?;
        if end > len {
            return None;
        }
        if size >= MIN_MEMBER {
            let mut region = start..end;
            if let Some(meta) = sidecar
                && header.starts_with(b"lib.rmeta/")
                && meta.len <= size
            {
                // rustc may wrap the metadata in an object header. Finding
                // the prefix only selects a boundary; the digest of each
                // complete chunk decides whether storage is actually shared.
                file.read_exact_at(&mut probe, start).ok()?;
                if let Some(relative) = memchr::memmem::find(&probe, &meta.prefix) {
                    let pos = start + relative as u64;
                    if meta.len <= end - pos {
                        region = pos..pos + meta.len;
                    }
                }
            }
            cuts.extend([region.start, region.end]);
        }
        offset = end.checked_add(size % 2)?;
    }
    None
}
