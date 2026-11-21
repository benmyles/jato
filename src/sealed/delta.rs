//! Small edits to a large authenticated base. Unmentioned bytes come from
//! its independently writable clone. The complete reconstructed digest is verified.
use super::proof;
use crate::store::{Damaged, Digest};
use anyhow::{Result, ensure};
use std::fs::File;
use std::io::{Seek, Write};
use std::os::unix::fs::FileExt;

pub const LIMIT: usize = 128 << 10;
pub const MAX_RESIZE: u64 = 64 << 10;
const WINDOW: usize = 64 << 10;
const RADIUS: usize = 4096;
const PAGE: usize = 4096;
const LITERAL: u64 = u64::MAX;
const HEADER: usize = 24;
const STREAM: &[u8; 8] = &(u64::MAX - 1).to_le_bytes();

pub struct Patch {
    pub result: Digest,
    pub data: Vec<u8>,
}

#[derive(Default)]
struct Changes {
    data: Vec<u8>,
    records: usize,
    copied: u64,
    // Index of the most recent copy header, for joining adjacent ranges.
    last_copy: Option<(usize, u64, u64)>,
}

impl Changes {
    fn record(&mut self, dst: u64, len: u64, src: u64) {
        self.data.extend(dst.to_le_bytes());
        self.data.extend(len.to_le_bytes());
        self.data.extend(src.to_le_bytes());
        self.records += 1;
    }

    fn copy(&mut self, dst: u64, len: u64, src: u64) {
        if src == dst || len == 0 {
            return;
        }
        self.copied += len;
        if let Some((i, end, next)) = self.last_copy
            && end == dst
            && next == src
        {
            let start = word(&self.data[i..]);
            self.data[i + 8..i + 16].copy_from_slice(&(dst + len - start).to_le_bytes());
            self.last_copy = Some((i, dst + len, src + len));
            return;
        }
        self.last_copy = Some((self.data.len(), dst + len, src + len));
        self.record(dst, len, src);
    }

    // A changed page can still have a long shifted prefix and suffix.
    // Record just the middle literal, keeping copy operations disjoint.
    fn edit(&mut self, dst: u64, page: &[u8], base: &[u8], at: usize, left: u64, shift: isize) {
        let mut src = dst;
        let mut diff = differences(page, base.get(at..at + page.len()));
        if shift != 0
            && let Some(i) = at.checked_add_signed(shift)
            && let Some(old) = base.get(i..i + page.len())
        {
            let shifted = differences(page, Some(old));
            if shifted.len() + 2 * HEADER < diff.len() {
                diff = shifted;
                src = left + i as u64;
            }
        }
        self.copy(dst, diff.start as u64, src);
        if !diff.is_empty() {
            self.last_copy = None;
            self.record(dst + diff.start as u64, diff.len() as u64, LITERAL);
            self.data.extend_from_slice(&page[diff.clone()]);
        }
        self.copy(
            dst + diff.end as u64,
            (page.len() - diff.end) as u64,
            src + diff.end as u64,
        );
    }
}

fn differences(page: &[u8], old: Option<&[u8]>) -> std::ops::Range<usize> {
    let Some(old) = old else { return 0..page.len() };
    let Some(start) = page.iter().zip(old).position(|(a, b)| a != b) else {
        return 0..0;
    };
    let suffix = page[start..]
        .iter()
        .rev()
        .zip(old[start..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    start..page.len() - suffix
}

fn word(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes[..8].try_into().expect("validated patch header"))
}

// Prefer the previous displacement and the original offset. A short anchor
// finds a shifted page after an insertion. The complete page must then match.
fn matching(base: &[u8], at: usize, shift: isize, page: &[u8]) -> Option<usize> {
    let matches = |i: usize| base.get(i..i + page.len()) == Some(page);
    if let Some(i) = at.checked_add_signed(shift)
        && matches(i)
    {
        return Some(i);
    }
    if shift != 0 && matches(at) {
        return Some(at);
    }
    let anchor = page.get(..32)?;
    let start = at.saturating_sub(RADIUS);
    let end = base.len().min(at + RADIUS + anchor.len());
    memchr::memmem::find_iter(base.get(start..end)?, anchor)
        .take(4)
        .map(|offset| start + offset)
        .find(|&i| matches(i))
}

pub fn capture(
    source: &File,
    size: u64,
    original: &File,
    original_size: u64,
) -> Result<Option<Patch>> {
    let budget = LIMIT.min((size / 32) as usize);
    let mut block = vec![0; WINDOW];
    let mut base = vec![0; WINDOW + 2 * RADIUS];
    let mut changes = Changes::default();
    let mut witness = (size == original_size).then(|| proof::Builder::new(size));
    let mut hash = blake3::Hasher::new();
    let mut shift = 0;
    let mut pos = 0;
    while pos < size {
        let n = (size - pos).min(WINDOW as u64) as usize;
        let left = pos.saturating_sub(RADIUS as u64);
        let right = original_size.min(pos + n as u64 + RADIUS as u64);
        let old = &mut base[..right.saturating_sub(left) as usize];
        original.read_exact_at(old, left)?;
        if let Some(witness) = &mut witness {
            let at = (pos - left) as usize;
            witness.update(pos, &old[at..at + n]);
        }
        source.read_exact_at(&mut block[..n], pos)?;
        hash.update(&block[..n]);
        for (i, page) in block[..n].chunks(PAGE).enumerate() {
            let dst = pos + (i * PAGE) as u64;
            let at = (dst - left) as usize;
            if let Some(found) = matching(old, at, shift, page) {
                shift = found as isize - at as isize;
                changes.copy(dst, page.len() as u64, left + found as u64);
            } else {
                changes.edit(dst, page, old, at, left, shift);
            }
            if changes.data.len() > budget || changes.records > 1024 {
                return Ok(None);
            }
        }
        pos += n as u64;
    }
    if changes.copied == 0
        && let Some(witness) = witness
    {
        witness.prepend(&mut changes.data, changes.records, budget);
    }
    if changes.copied > size / 3 && changes.data.len() + STREAM.len() <= budget {
        let mut program = Vec::with_capacity(changes.data.len() + STREAM.len());
        program.extend_from_slice(STREAM);
        program.append(&mut changes.data);
        changes.data = program;
    }
    Ok(Some(Patch {
        result: *hash.finalize().as_bytes(),
        data: changes.data,
    }))
}

pub fn streams(data: &[u8]) -> bool {
    data.starts_with(STREAM)
}

enum Contents<'a> {
    Literal(&'a [u8]),
    Base(u64),
}

struct Edit<'a> {
    at: u64,
    len: u64,
    contents: Contents<'a>,
}

fn edits(mut data: &[u8], size: u64, base_size: u64) -> Result<Vec<Edit<'_>>> {
    let mut result = Vec::new();
    let mut end = 0;
    while !data.is_empty() {
        ensure!(data.len() >= HEADER && result.len() < 1024, Damaged::Entry);
        let at = word(data);
        let len = word(&data[8..]);
        let src = word(&data[16..]);
        data = &data[HEADER..];
        ensure!(
            at >= end && at <= size && len != 0 && len <= size - at,
            Damaged::Entry
        );
        end = at + len;
        let contents = if src == LITERAL {
            ensure!(len <= data.len() as u64, Damaged::Entry);
            let (bytes, rest) = data.split_at(len as usize);
            data = rest;
            Contents::Literal(bytes)
        } else {
            ensure!(src <= base_size && len <= base_size - src, Damaged::Entry);
            Contents::Base(src)
        };
        result.push(Edit { at, len, contents });
    }
    Ok(result)
}

pub fn apply(
    source: &File,
    dest: &mut File,
    size: u64,
    original_size: u64,
    mut data: &[u8],
    expected: &Digest,
    base_hash: &Digest,
) -> Result<()> {
    ensure!(
        size.abs_diff(original_size) <= MAX_RESIZE && data.len() <= LIMIT,
        Damaged::Entry
    );
    if let Some(program) = data.strip_prefix(STREAM) {
        return stream(
            source,
            dest,
            size,
            original_size,
            edits(program, size, original_size)?,
            expected,
        );
    }
    let mut proof = proof::Proof::take(&mut data, size, original_size, base_hash)?;
    dest.set_len(size)?;
    let mut buffer = Vec::new();
    for edit in edits(data, size, original_size)? {
        if let Some(proof) = &mut proof {
            proof.changed(edit.at, edit.at + edit.len);
        }
        match edit.contents {
            Contents::Literal(bytes) => dest.write_all_at(bytes, edit.at)?,
            Contents::Base(src) => {
                buffer.resize(WINDOW, 0);
                copy_range(source, dest, src, edit.at, edit.len, &mut buffer)?;
            }
        }
    }
    if let Some(proof) = proof {
        return proof.verify(dest, expected);
    }
    dest.rewind()?;
    ensure!(
        blake3::Hasher::new()
            .update_reader(dest)?
            .finalize()
            .as_bytes()
            == expected,
        Damaged::Entry
    );
    Ok(())
}

// When most bytes have moved, reading and hashing the authenticated source
// while writing is cheaper than modifying a reflink and rereading the result.
fn stream(
    source: &File,
    dest: &mut File,
    size: u64,
    base_size: u64,
    edits: Vec<Edit<'_>>,
    expected: &Digest,
) -> Result<()> {
    let mut buffer = vec![0; WINDOW];
    let mut hash = blake3::Hasher::new();
    dest.set_len(0)?;
    dest.rewind()?;
    let mut emit =
        |dest: &mut File, mut offset: u64, mut len: u64, hash: &mut blake3::Hasher| -> Result<()> {
            while len != 0 {
                let n = len.min(WINDOW as u64) as usize;
                let available = base_size.saturating_sub(offset).min(n as u64) as usize;
                source.read_exact_at(&mut buffer[..available], offset)?;
                buffer[available..n].fill(0);
                dest.write_all(&buffer[..n])?;
                hash.update(&buffer[..n]);
                offset += n as u64;
                len -= n as u64;
            }
            Ok(())
        };
    let mut at = 0;
    for edit in edits {
        emit(dest, at, edit.at - at, &mut hash)?;
        match edit.contents {
            Contents::Literal(bytes) => {
                dest.write_all(bytes)?;
                hash.update(bytes);
            }
            Contents::Base(src) => emit(dest, src, edit.len, &mut hash)?,
        }
        at = edit.at + edit.len;
    }
    emit(dest, at, size - at, &mut hash)?;
    ensure!(hash.finalize().as_bytes() == expected, Damaged::Entry);
    Ok(())
}

fn copy_range(
    source: &File,
    dest: &File,
    mut src: u64,
    mut dst: u64,
    mut len: u64,
    buf: &mut [u8],
) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let mut from = src as libc::loff_t;
        let mut to = dst as libc::loff_t;
        while len != 0 {
            let n = unsafe {
                libc::copy_file_range(
                    source.as_raw_fd(),
                    &mut from,
                    dest.as_raw_fd(),
                    &mut to,
                    len.min(1 << 30) as usize,
                    0,
                )
            };
            if n <= 0 {
                if n < 0
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                {
                    continue;
                }
                break;
            }
            len -= n as u64;
        }
        src = from as u64;
        dst = to as u64;
    }
    while len != 0 {
        let n = len.min(buf.len() as u64) as usize;
        source.read_exact_at(&mut buf[..n], src)?;
        dest.write_all_at(&buf[..n], dst)?;
        src += n as u64;
        dst += n as u64;
        len -= n as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn bounded_patches_round_trip_shifted_growing_and_shortened_files() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let file = tmp.path().join("base");
        let mut base = vec![0; (5 << 20) + 81];
        blake3::Hasher::new()
            .update(b"edit history")
            .finalize_xof()
            .fill(&mut base);
        fs::write(&file, &base)?;
        let original = File::open(&file)?;
        let input = tmp.path().join("new");
        let out = tmp.path().join("restored");
        let front = 13 * PAGE + 79;
        for (len_delta, at) in [
            (0isize, front),
            (1, front),
            (317, front),
            (-17, front),
            (317, base.len() - 5 * PAGE - 1),
        ] {
            let mut data = base.clone();
            if len_delta < 0 {
                data.drain(at..at + (-len_delta) as usize);
            } else {
                data.splice(at..at, std::iter::repeat_n(17, len_delta as usize));
            }
            data[WINDOW + 133] = 7;
            data[2 * WINDOW - 3..2 * WINDOW + 3].fill(31);
            fs::write(&input, &data)?;
            let patch = capture(
                &File::open(&input)?,
                data.len() as u64,
                &original,
                base.len() as u64,
            )?
            .unwrap();
            assert!(patch.data.len() < 16 << 10);
            assert_eq!(patch.result, *blake3::hash(&data).as_bytes());
            if len_delta == 0 {
                assert!(patch.data.starts_with(&u64::MAX.to_le_bytes()));
                let mut corrupt = patch.data.clone();
                corrupt[8] ^= 1;
                let mut bytes = corrupt.as_slice();
                assert!(
                    proof::Proof::take(
                        &mut bytes,
                        base.len() as u64,
                        base.len() as u64,
                        blake3::hash(&base).as_bytes()
                    )
                    .is_err()
                );
            } else if at < base.len() / 2 {
                assert!(streams(&patch.data));
            } else {
                assert!(!streams(&patch.data));
            }
            fs::copy(&file, &out)?;
            let mut dest = File::options().read(true).write(true).open(&out)?;
            apply(
                &original,
                &mut dest,
                data.len() as u64,
                base.len() as u64,
                &patch.data,
                &patch.result,
                blake3::hash(&base).as_bytes(),
            )?;
            assert_eq!(fs::read(&out)?, data);
            if len_delta == 0 {
                // Even a byte outside the literal must be verified when it
                // lies in a stripe touched by that operation.
                fs::copy(&file, &out)?;
                let n = WINDOW + 111;
                dest.write_all_at(&[!base[n]], n as u64)?;
                assert!(
                    apply(
                        &original,
                        &mut dest,
                        data.len() as u64,
                        base.len() as u64,
                        &patch.data,
                        &patch.result,
                        blake3::hash(&base).as_bytes()
                    )
                    .is_err()
                );
            }
            fs::copy(&file, &out)?;
            assert!(
                apply(
                    &original,
                    &mut dest,
                    data.len() as u64,
                    base.len() as u64,
                    &patch.data,
                    &[9; 32],
                    blake3::hash(&base).as_bytes(),
                )
                .is_err()
            );
        }
        let different = vec![31; base.len()];
        fs::write(&input, &different)?;
        assert!(
            capture(
                &File::open(input)?,
                base.len() as u64,
                &original,
                base.len() as u64
            )?
            .is_none()
        );
        Ok(())
    }
}
