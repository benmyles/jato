//! BLAKE3 witnesses for regions of an authenticated, independently cloned
//! base. Both the base and the reconstructed file must match their full roots.
use crate::store::{Damaged, Digest};
use anyhow::{Result, ensure};
use blake3::hazmat::HasherExt;
use std::fs::File;
use std::os::unix::fs::FileExt;

const TAG: &[u8; 8] = &u64::MAX.to_le_bytes();
const MAX_STRIPES: usize = 128;

fn width(size: u64) -> u64 {
    size.div_ceil(MAX_STRIPES as u64)
        .next_power_of_two()
        .max(64 << 10)
}

pub(super) struct Builder {
    size: u64,
    width: u64,
    hash: blake3::Hasher,
    leaves: Vec<Digest>,
}

impl Builder {
    pub fn new(size: u64) -> Self {
        Self {
            size,
            width: width(size),
            hash: blake3::Hasher::new(),
            leaves: Vec::new(),
        }
    }

    // Capture supplies consecutive 64 KiB windows; a stripe is an aligned
    // power-of-two multiple of that window even on very large files.
    pub fn update(&mut self, pos: u64, bytes: &[u8]) {
        if pos.is_multiple_of(self.width) {
            self.hash = blake3::Hasher::new();
            self.hash.set_input_offset(pos);
        }
        self.hash.update(bytes);
        let end = pos + bytes.len() as u64;
        if end == self.size || end.is_multiple_of(self.width) {
            self.leaves.push(self.hash.finalize_non_root());
        }
    }

    pub fn prepend(self, program: &mut Vec<u8>, records: usize, budget: usize) {
        let bytes = TAG.len() + self.leaves.len() * 32;
        if self.leaves.len() < 2
            || records > self.leaves.len() / 4
            || program.len() + bytes > budget
        {
            return;
        }
        let mut encoded = Vec::with_capacity(bytes + program.len());
        encoded.extend_from_slice(TAG);
        for v in self.leaves {
            encoded.extend(v);
        }
        encoded.append(program);
        *program = encoded;
    }
}

pub(super) struct Proof {
    leaves: Vec<Digest>,
    changed: [bool; MAX_STRIPES],
    size: u64,
    width: u64,
}

impl Proof {
    pub fn take(
        data: &mut &[u8],
        size: u64,
        base_size: u64,
        base_hash: &Digest,
    ) -> Result<Option<Self>> {
        let Some(rest) = data.strip_prefix(TAG) else {
            return Ok(None);
        };
        ensure!(size == base_size && size >= 4 << 20, Damaged::Entry);
        let width = width(size);
        let count = size.div_ceil(width) as usize;
        ensure!(
            (2..=MAX_STRIPES).contains(&count) && rest.len() >= 32 * count,
            Damaged::Entry
        );
        let (hashes, program) = rest.split_at(32 * count);
        let leaves = hashes.as_chunks::<32>().0.to_vec();
        ensure!(crate::content::root(&leaves) == *base_hash, Damaged::Entry);
        *data = program;
        Ok(Some(Self {
            leaves,
            changed: [false; MAX_STRIPES],
            size,
            width,
        }))
    }

    pub fn changed(&mut self, start: u64, end: u64) {
        self.changed[(start / self.width) as usize..end.div_ceil(self.width) as usize].fill(true);
    }

    pub fn verify(mut self, file: &File, expected: &Digest) -> Result<()> {
        let mut buffer = vec![0; 64 << 10];
        for (i, leaf) in self.leaves.iter_mut().enumerate() {
            if !self.changed[i] {
                continue;
            }
            let mut pos = i as u64 * self.width;
            let end = self.size.min(pos + self.width);
            let mut hash = blake3::Hasher::new();
            hash.set_input_offset(pos);
            while pos < end {
                let n = (end - pos).min(buffer.len() as u64) as usize;
                file.read_exact_at(&mut buffer[..n], pos)?;
                hash.update(&buffer[..n]);
                pos += n as u64;
            }
            *leaf = hash.finalize_non_root();
        }
        ensure!(
            crate::content::root(&self.leaves) == *expected,
            Damaged::Entry
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[test]
    fn witnesses_match_standard_hashes_at_stripe_and_final_block_boundaries() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("staged");
        for size in [4 << 20, (4 << 20) + 1, (8 << 20) - 1, (8 << 20) + 1024] {
            let mut bytes = vec![0; size];
            blake3::Hasher::new()
                .update(b"subtree proof")
                .finalize_xof()
                .fill(&mut bytes);
            let base = *blake3::hash(&bytes).as_bytes();
            let mut build = Builder::new(size as u64);
            for (i, chunk) in bytes.chunks(64 << 10).enumerate() {
                build.update((i as u64) << 16, chunk)
            }
            let mut program = vec![7];
            build.prepend(&mut program, 2, 128 << 10);
            let mut remaining = program.as_slice();
            let mut proof = Proof::take(&mut remaining, size as u64, size as u64, &base)?.unwrap();
            assert_eq!(remaining, [7]);
            let edge = proof.width as usize;
            bytes[edge - 1..edge + 1].fill(77);
            bytes[size - 1] ^= 0xf0;
            proof.changed((edge - 1) as u64, (edge + 1) as u64);
            proof.changed((size - 1) as u64, size as u64);
            fs::write(&path, &bytes)?;
            proof.verify(&File::open(&path)?, blake3::hash(&bytes).as_bytes())?;
        }
        Ok(())
    }
}
