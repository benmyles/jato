//! Read a file once to learn its digest and whether it names a directory
//! that would make it wrong to use anywhere else.
use crate::observe::Stamp;
use memchr::memmem::Finder;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub type Digest = [u8; 32];

const BLOCK: usize = 512 * 1024;

thread_local! {
    static DIGEST_BUF: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

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
    DIGEST_BUF.with(|cell| {
        let mut guard = cell.borrow_mut();
        digest_open_with(file, needles, &mut guard)
    })
}

pub fn digest_open_with(
    file: &mut File,
    needles: &[&[u8]],
    buffer: &mut Vec<u8>,
) -> io::Result<(Digest, u64, Vec<bool>)> {
    let finders: Vec<_> = needles.iter().map(Finder::new).collect();
    // Keep enough of the previous block to see a path that straddles two
    // blocks, and the byte that follows it.
    let keep = needles.iter().map(|n| n.len()).max().map_or(0, |n| n + 1);
    let mut found = vec![false; needles.len()];
    let want = keep + BLOCK;
    if buffer.len() < want {
        buffer.resize(want, 0);
    }
    let buf = &mut buffer[..want];
    let mut hasher = blake3::Hasher::new();
    let mut carried = 0;
    let mut length = 0u64;
    loop {
        let mut filled = carried;
        while filled < buf.len() {
            match file.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => (),
                Err(e) => return Err(e),
            }
        }
        let end = filled < buf.len();
        hasher.update(&buf[carried..filled]);
        length += (filled - carried) as u64;
        let data = &buf[..filled];
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
        buf.copy_within(filled - carried..filled, 0);
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

/// If `name` is an incremental rustc CGU object inside an `.rlib`
/// (`<crate>-<hash>.<cgu>.<session>.rcgu.o`), return its session-independent
/// form (`<crate>-<hash>.<cgu>.rcgu.o`). Non-incremental names (`-cgu.0.rcgu.o`)
/// are already deterministic and return `None`.
fn normalized_rcgu_name(name: &[u8]) -> Option<Vec<u8>> {
    let stem = name.strip_suffix(b".rcgu.o")?;
    let dot = stem.iter().rposition(|&b| b == b'.')?;
    let prefix = &stem[..dot];
    let suffix = &stem[dot + 1..];
    if suffix.is_empty()
        || !suffix.iter().all(u8::is_ascii_alphanumeric)
        || prefix.ends_with(b"-cgu")
    {
        return None;
    }
    let mut out = Vec::with_capacity(prefix.len() + 7);
    out.extend_from_slice(prefix);
    out.extend_from_slice(b".rcgu.o");
    Some(out)
}

/// Normalize the `.rmeta-link` ELF object inside a rustc `.rlib` by
/// extracting its `.note.gnu.property` and `.rmeta-link` sections and
/// replacing only the exact `.rcgu.o` member names from this archive
/// (and their immediate length prefix) with their session-independent forms.
/// Returns `None` if the ELF contains any unexpected sections or symbols.
fn normalized_rmeta_link(bytes: &[u8], renames: &[(Vec<u8>, Vec<u8>)]) -> Option<Vec<u8>> {
    if bytes.len() < 64 || &bytes[..4] != b"\x7fELF" || bytes[4] != 2 || bytes[5] != 1 {
        return None;
    }
    let word = |at: usize, len: usize| -> Option<usize> {
        let slice = bytes.get(at..at + len)?;
        let mut buf = [0u8; 8];
        buf[..len].copy_from_slice(slice);
        usize::try_from(u64::from_le_bytes(buf)).ok()
    };
    let shoff = word(40, 8)?;
    let shentsize = word(58, 2)?;
    let shnum = word(60, 2)?;
    let shstrndx = word(62, 2)?;
    if shentsize < 64 || shnum == 0 || shstrndx >= shnum {
        return None;
    }
    let shstr_hdr = shoff.checked_add(shstrndx.checked_mul(shentsize)?)?;
    let shstr_off = word(shstr_hdr + 24, 8)?;
    let shstr_len = word(shstr_hdr + 32, 8)?;
    let shstr = bytes.get(shstr_off..shstr_off.checked_add(shstr_len)?)?;

    let mut link_sec: Option<&[u8]> = None;
    let mut note_sec: Option<&[u8]> = None;
    for i in 0..shnum {
        let hdr = shoff.checked_add(i.checked_mul(shentsize)?)?;
        let name_off = word(hdr, 4)?;
        let rest = shstr.get(name_off..)?;
        let name = &rest[..memchr::memchr(0, rest).unwrap_or(rest.len())];
        let sec_off = word(hdr + 24, 8)?;
        let sec_len = word(hdr + 32, 8)?;
        match name {
            b"" | b".strtab" | b".shstrtab" => (),
            b".symtab" => {
                // Only the null symbol entry (24 bytes) is expected.
                if sec_len > 24 {
                    return None;
                }
            }
            b".note.gnu.property" => {
                note_sec = Some(bytes.get(sec_off..sec_off.checked_add(sec_len)?)?);
            }
            b".rmeta-link" => {
                link_sec = Some(bytes.get(sec_off..sec_off.checked_add(sec_len)?)?);
            }
            _ => return None,
        }
    }
    let mut link = link_sec?.to_vec();
    for (old, new) in renames {
        let finder = Finder::new(old);
        let mut out = Vec::with_capacity(link.len());
        let mut cursor = 0;
        let mut replaced = false;
        while let Some(rel) = finder.find(&link[cursor..]) {
            let start = cursor + rel;
            let end = start + old.len();
            // If immediately preceded by the 1-byte length prefix (< 128),
            // normalize the length byte too so differing session-ID lengths
            // (e.g. 6 vs 7 chars) hash identically.
            if old.len() < 128
                && new.len() < 128
                && start > cursor
                && link[start - 1] == old.len() as u8
            {
                out.extend_from_slice(&link[cursor..start - 1]);
                out.push(new.len() as u8);
            } else {
                out.extend_from_slice(&link[cursor..start]);
            }
            out.extend_from_slice(new);
            cursor = end;
            replaced = true;
        }
        if replaced {
            out.extend_from_slice(&link[cursor..]);
            link = out;
        }
    }
    let mut normalized = Vec::with_capacity(link.len() + note_sec.map_or(0, <[u8]>::len) + 16);
    if let Some(note) = note_sec {
        normalized.extend_from_slice(&(note.len() as u64).to_le_bytes());
        normalized.extend_from_slice(note);
    } else {
        normalized.extend_from_slice(&0u64.to_le_bytes());
    }
    normalized.extend_from_slice(&(link.len() as u64).to_le_bytes());
    normalized.extend_from_slice(&link);
    Some(normalized)
}

/// Content digest of a rustc `.rlib` archive that ignores only the random
/// per-compilation session suffix in incremental `.rcgu.o` member names
/// (and their exact references inside `lib.rmeta-link`), while hashing
/// every byte of the symbol table, `lib.rmeta`, every `.rcgu.o` payload,
/// and every native member verbatim.
///
/// Returns `Ok(None)` if the file is not an incremental `.rlib` archive,
/// so the caller falls back to the raw file digest.
pub fn digest_rlib(path: &Path) -> io::Result<Option<(Digest, Stamp)>> {
    const MAX_AUX: u64 = 4 << 20;
    let mut file = File::open(path)?;
    let before = file.metadata()?;
    if !before.is_file() || before.len() < 8 {
        return Ok(None);
    }
    let file_len = before.len();
    let mut magic = [0u8; 8];
    file.read_exact(&mut magic)?;
    if &magic != b"!<arch>\n" {
        return Ok(None);
    }

    struct Member {
        name: Vec<u8>,
        offset: u64,
        size: u64,
    }
    let mut raw_members: Vec<(Vec<u8>, u64, u64)> = Vec::new();
    let mut strtab: Vec<u8> = Vec::new();
    let mut pos = 8u64;
    let mut hdr = [0u8; 60];
    while pos + 60 <= file_len {
        file.seek(SeekFrom::Start(pos))?;
        file.read_exact(&mut hdr)?;
        if &hdr[58..60] != b"`\n" {
            return Ok(None);
        }
        let size_str = std::str::from_utf8(&hdr[48..58])
            .ok()
            .map(str::trim)
            .and_then(|s| s.parse::<u64>().ok());
        let Some(size) = size_str else {
            return Ok(None);
        };
        let data_offset = pos + 60;
        let next = match data_offset
            .checked_add(size)
            .and_then(|n| n.checked_add(size & 1))
        {
            Some(n) if n <= file_len => n,
            _ => return Ok(None),
        };
        let raw_name = hdr[..16].trim_ascii_end();
        if raw_name == b"//" {
            if size > MAX_AUX {
                return Ok(None);
            }
            strtab.resize(size as usize, 0);
            file.read_exact(&mut strtab)?;
        } else {
            raw_members.push((raw_name.to_vec(), data_offset, size));
        }
        pos = next;
    }
    if pos != file_len {
        return Ok(None);
    }

    let mut members: Vec<Member> = Vec::with_capacity(raw_members.len());
    let mut renames: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for (raw_name, offset, size) in raw_members {
        let name = if raw_name == b"/" {
            b"/".to_vec()
        } else if let Some(off_bytes) = raw_name.strip_prefix(b"/")
            && !off_bytes.is_empty()
            && off_bytes.iter().all(u8::is_ascii_digit)
        {
            let Ok(off) = std::str::from_utf8(off_bytes)
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .ok_or(())
            else {
                return Ok(None);
            };
            let Some(rest) = strtab.get(off..) else {
                return Ok(None);
            };
            let end = Finder::new(b"/\n")
                .find(rest)
                .or_else(|| memchr::memchr(b'\n', rest))
                .unwrap_or(rest.len());
            rest[..end].to_vec()
        } else {
            raw_name.strip_suffix(b"/").unwrap_or(&raw_name).to_vec()
        };
        if let Some(norm) = normalized_rcgu_name(&name) {
            renames.push((name.clone(), norm));
        }
        members.push(Member { name, offset, size });
    }
    if renames.is_empty() {
        return Ok(None);
    }

    let mut hasher = blake3::Hasher::new_derive_key("jato rlib v1");
    DIGEST_BUF.with(|cell| -> io::Result<()> {
        let mut buf = cell.borrow_mut();
        if buf.len() < 256 * 1024 {
            buf.resize(256 * 1024, 0);
        }
        let slice = &mut buf[..256 * 1024];
        for member in &members {
            let label: &[u8] = renames
                .iter()
                .find(|(old, _)| old == &member.name)
                .map_or(&member.name, |(_, new)| new);
            hasher.update(&(label.len() as u64).to_le_bytes());
            hasher.update(label);
            file.seek(SeekFrom::Start(member.offset))?;
            if member.name == b"lib.rmeta-link" && member.size <= MAX_AUX {
                let mut raw = vec![0u8; member.size as usize];
                file.read_exact(&mut raw)?;
                if let Some(norm) = normalized_rmeta_link(&raw, &renames) {
                    hasher.update(&[1]);
                    hasher.update(&(norm.len() as u64).to_le_bytes());
                    hasher.update(&norm);
                    continue;
                }
                hasher.update(&[0]);
                hasher.update(&(raw.len() as u64).to_le_bytes());
                hasher.update(&raw);
                continue;
            }
            hasher.update(&[0]);
            hasher.update(&member.size.to_le_bytes());
            let mut left = member.size;
            while left > 0 {
                let want = (left as usize).min(slice.len());
                let n = file.read(&mut slice[..want])?;
                if n == 0 {
                    return Err(io::Error::other("rlib truncated while hashing"));
                }
                hasher.update(&slice[..n]);
                left -= n as u64;
            }
        }
        Ok(())
    })?;
    let stamp = Stamp::of(&file.metadata()?);
    if !Stamp::of(&before).same_contents(&stamp) || file_len != stamp.size {
        return Err(io::Error::other("file changed while it was read"));
    }
    Ok(Some((*hasher.finalize().as_bytes(), stamp)))
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

    #[test]
    fn incremental_rlib_digest_ignores_only_session_suffix() {
        let make_ar = |members: &[(&str, &[u8])]| -> Vec<u8> {
            let mut out = b"!<arch>\n".to_vec();
            let mut strtab = Vec::new();
            let mut offs = Vec::new();
            for (name, _) in members {
                if name.len() > 15 && *name != "/" {
                    offs.push(Some(strtab.len()));
                    strtab.extend_from_slice(name.as_bytes());
                    strtab.extend_from_slice(b"/\n");
                } else {
                    offs.push(None);
                }
            }
            let push_member = |out: &mut Vec<u8>, hdr_name: &str, body: &[u8]| {
                out.extend_from_slice(
                    format!(
                        "{hdr_name:<16}0           0     0     100644  {:<10}`\n",
                        body.len()
                    )
                    .as_bytes(),
                );
                out.extend_from_slice(body);
                if body.len() & 1 == 1 {
                    out.push(b'\n');
                }
            };
            if !strtab.is_empty() {
                push_member(&mut out, "//", &strtab);
            }
            for ((name, body), off) in members.iter().zip(offs) {
                let hdr_name = match off {
                    Some(off) => format!("/{off}"),
                    None if *name == "/" => "/".to_owned(),
                    None => format!("{name}/"),
                };
                push_member(&mut out, &hdr_name, body);
            }
            out
        };
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.rlib");
        let b = dir.path().join("b.rlib");
        let c = dir.path().join("c.rlib");
        std::fs::write(
            &a,
            make_ar(&[
                ("/", b"symtab"),
                ("lib.rmeta", b"meta"),
                ("crate-abc.5wad9z.03zh75l.rcgu.o", b"object-bytes"),
            ]),
        )
        .unwrap();
        std::fs::write(
            &b,
            make_ar(&[
                ("/", b"symtab"),
                ("lib.rmeta", b"meta"),
                ("crate-abc.5wad9z.9x1k2p.rcgu.o", b"object-bytes"),
            ]),
        )
        .unwrap();
        std::fs::write(
            &c,
            make_ar(&[
                ("/", b"symtab"),
                ("lib.rmeta", b"meta"),
                ("crate-abc.5wad9z.9x1k2p.rcgu.o", b"object-other"),
            ]),
        )
        .unwrap();
        let (da, _) = digest_rlib(&a).unwrap().expect("incremental rlib");
        let (db, _) = digest_rlib(&b).unwrap().expect("incremental rlib");
        let (dc, _) = digest_rlib(&c).unwrap().expect("incremental rlib");
        assert_eq!(da, db);
        assert_ne!(da, dc);
        // Non-incremental rlib returns None.
        let plain = dir.path().join("plain.rlib");
        std::fs::write(
            &plain,
            make_ar(&[("crate-abc.crate.1234-cgu.0.rcgu.o", b"object-bytes")]),
        )
        .unwrap();
        assert!(digest_rlib(&plain).unwrap().is_none());
    }
}
