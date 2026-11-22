//! Read a file once to learn its digest and whether it names a directory
//! that would make it wrong to use anywhere else.
use crate::observe::Stamp;
use memchr::memmem::Finder;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

pub type Digest = [u8; 32];

const BLOCK: usize = 512 * 1024;

/// A byte that can continue a file name. A directory's path followed by one
/// of these is the path of something else: `/a/src` does not occur in
/// `/a/src-old/x`.
fn continues_name(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'+' | b'@' | b'~')
}

/// Whether `needle`, a directory path, occurs in `haystack` as that
/// directory or as the start of a path beneath it.
pub fn names_path(haystack: &[u8], needle: &[u8]) -> bool {
    let finder = Finder::new(needle);
    let mut from = 0;
    while let Some(at) = finder.find(&haystack[from..]) {
        let end = from + at + needle.len();
        if haystack.get(end).is_none_or(|b| !continues_name(*b)) {
            return true;
        }
        from += at + 1;
    }
    false
}

/// The digest and length of an open file, and for each needle whether the
/// file names that path. Memory use does not depend on the file's size.
pub fn digest_open(file: &mut File, needles: &[&[u8]]) -> io::Result<(Digest, u64, Vec<bool>)> {
    let finders: Vec<_> = needles.iter().map(Finder::new).collect();
    // Keep enough of the previous block to see a path that straddles two
    // blocks, and the byte that follows it.
    let keep = needles.iter().map(|n| n.len()).max().map_or(0, |n| n + 1);
    let mut found = vec![false; needles.len()];
    let mut buffer = vec![0u8; keep + BLOCK];
    let mut hasher = blake3::Hasher::new();
    let mut carried = 0;
    let mut length = 0u64;
    loop {
        let mut filled = carried;
        while filled < buffer.len() {
            match file.read(&mut buffer[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => (),
                Err(e) => return Err(e),
            }
        }
        let end = filled < buffer.len();
        hasher.update(&buffer[carried..filled]);
        length += (filled - carried) as u64;
        let data = &buffer[..filled];
        for ((finder, needle), found) in finders.iter().zip(needles).zip(&mut found) {
            let mut from = 0;
            while !*found {
                let Some(at) = finder.find(&data[from..]) else {
                    break;
                };
                let after = from + at + needle.len();
                match data.get(after) {
                    Some(b) => *found = !continues_name(*b),
                    // The next block shows what follows.
                    None => *found = end,
                }
                if after >= data.len() {
                    break;
                }
                from += at + 1;
            }
        }
        if end {
            break;
        }
        carried = keep.min(filled);
        buffer.copy_within(filled - carried..filled, 0);
    }
    Ok((*hasher.finalize().as_bytes(), length, found))
}

/// The digest of a regular file, with the stamp that identifies the state
/// that was read.
pub fn digest_file(path: &Path, needles: &[&[u8]]) -> io::Result<(Digest, Stamp, Vec<bool>)> {
    let mut file = File::open(path)?;
    let before = file.metadata()?;
    if !before.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    let (digest, length, found) = digest_open(&mut file, needles)?;
    let stamp = Stamp::of(&file.metadata()?);
    if !Stamp::of(&before).same_contents(&stamp) || length != stamp.size {
        return Err(io::Error::other("file changed while it was read"));
    }
    Ok((digest, stamp, found))
}

/// The program the kernel loads to run this file, which no system call of
/// the command names: a script's interpreter or an executable's loader.
pub fn interpreter(path: &Path) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut head = [0u8; 4096];
    let mut file = File::open(path).ok()?;
    let mut filled = 0;
    while filled < head.len() {
        match file.read(&mut head[filled..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => filled += n,
        }
    }
    let head = &head[..filled];
    if let Some(line) = head.strip_prefix(b"#!") {
        let line = &line[..memchr::memchr(b'\n', line).unwrap_or(line.len())];
        let name = line
            .split(|b| *b == b' ' || *b == b'\t')
            .find(|part| !part.is_empty())?;
        return Some(PathBuf::from(std::ffi::OsStr::from_bytes(name)));
    }
    // A 64-bit little-endian ELF image with its program headers in the
    // first block, which is where every linker puts them.
    if head.len() < 64 || &head[..4] != b"\x7fELF" || head[4] != 2 || head[5] != 1 {
        return None;
    }
    let word = |at: usize, len: usize| -> Option<usize> {
        let bytes = head.get(at..at + len)?;
        let mut value = [0u8; 8];
        value[..len].copy_from_slice(bytes);
        usize::try_from(u64::from_le_bytes(value)).ok()
    };
    let (table, size, count) = (word(32, 8)?, word(54, 2)?, word(56, 2)?);
    for i in 0..count {
        let at = table.checked_add(i.checked_mul(size)?)?;
        // PT_INTERP
        if word(at, 4)? == 3 {
            let (offset, length) = (word(at + 8, 8)?, word(at + 32, 8)?);
            let name = head.get(offset..offset.checked_add(length)?)?;
            let name = &name[..memchr::memchr(0, name).unwrap_or(name.len())];
            return Some(PathBuf::from(std::ffi::OsStr::from_bytes(name)));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn a_path_is_found_only_as_that_directory() {
        assert!(names_path(b"x /a/src/lib.rs", b"/a/src"));
        assert!(names_path(b"/a/src", b"/a/src"));
        assert!(names_path(b"\"/a/src\"", b"/a/src"));
        assert!(!names_path(b"/a/src-old/lib.rs", b"/a/src"));
        assert!(!names_path(b"/a/src2", b"/a/src"));
        assert!(names_path(b"/a/src2 /a/src/x", b"/a/src"));
    }

    #[test]
    fn a_path_across_two_blocks_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let needle = b"/some/checkout";
        for (lead, tail, expect) in [
            (BLOCK - 5, &b"/x"[..], true),
            (BLOCK - 5, &b"-other"[..], false),
            (BLOCK - needle.len(), &b""[..], true),
            (BLOCK - needle.len(), &b"s"[..], false),
            (3 * BLOCK + 17, &b"\0"[..], true),
            (0, &b"/x"[..], true),
        ] {
            let path = dir.path().join("f");
            let mut file = File::create(&path).unwrap();
            let mut all = vec![b'a'; lead];
            all.extend_from_slice(needle);
            all.extend_from_slice(tail);
            all.extend(std::iter::repeat_n(b'z', BLOCK));
            if tail.is_empty() {
                all.truncate(lead + needle.len());
            }
            file.write_all(&all).unwrap();
            drop(file);
            let (digest, stamp, found) = digest_file(&path, &[needle, b"/absent"]).unwrap();
            assert_eq!(digest, *blake3::hash(&all).as_bytes());
            assert_eq!(stamp.size, all.len() as u64);
            assert_eq!(found, [expect, false], "lead {lead} tail {tail:?}");
        }
    }

    #[test]
    fn the_loader_and_a_script_interpreter_are_named() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("s");
        std::fs::write(&script, "#! /bin/sh -e\necho\n").unwrap();
        assert_eq!(interpreter(&script), Some("/bin/sh".into()));
        std::fs::write(&script, "plain").unwrap();
        assert_eq!(interpreter(&script), None);
        if cfg!(target_os = "linux") {
            let loader = interpreter(&std::env::current_exe().unwrap()).unwrap();
            assert!(loader.to_string_lossy().contains("ld-"), "{loader:?}");
        }
    }
}
