//! Large artifacts on compressed Btrfs can keep their existing extents.
//! fs-verity makes the cached inode immutable and authenticates its identity.
//! A successful FICLONE snapshots those bytes into an independent writable file.
use crate::store::{Damaged, Digest};
use anyhow::{Context, Result, ensure};
use std::fs::{self, File};
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};

pub struct Object {
    file: File,
    pub verity: Digest,
    pub hash: Digest,
}

pub fn path(dir: &Path, digest: &Digest) -> PathBuf {
    let name = blake3::Hash::from_bytes(*digest).to_hex();
    dir.join("objects-v1").join(&name[..2]).join(&name[2..])
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::ffi::CString;
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};

    // Fixed Linux uapi structures from linux/fsverity.h, ABI version 1.
    #[repr(C)]
    #[derive(Default)]
    struct Enable {
        version: u32,
        algorithm: u32,
        block_size: u32,
        salt_size: u32,
        salt: u64,
        sig_size: u32,
        reserved1: u32,
        signature: u64,
        reserved: [u64; 11],
    }
    #[repr(C)]
    struct Measurement {
        algorithm: u16,
        size: u16,
        bytes: Digest,
    }

    fn clone_file(source: &File, dest: &File) -> io::Result<()> {
        let r = unsafe { libc::ioctl(dest.as_raw_fd(), libc::FICLONE as _, source.as_raw_fd()) };
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub fn measure(file: &File) -> io::Result<Digest> {
        let mut m = Measurement {
            algorithm: 0,
            size: 32,
            bytes: [0; 32],
        };
        // The ioctl size encodes the 4-byte header, before its flexible array.
        let r = unsafe {
            libc::ioctl(
                file.as_raw_fd(),
                libc::_IOWR::<[u16; 2]>(b'f' as u32, 134) as _,
                &mut m,
            )
        };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }
        if m.algorithm != 1 || m.size != 32 {
            return Err(io::Error::other("unexpected fs-verity digest"));
        }
        Ok(m.bytes)
    }

    pub fn supported(dir: &Path) -> bool {
        let Ok(handle) = File::open(dir) else {
            return false;
        };
        let Ok(info) = fs::read_to_string(format!("/proc/self/fdinfo/{}", handle.as_raw_fd()))
        else {
            return false;
        };
        let Some(id) = info
            .lines()
            .find_map(|l| l.strip_prefix("mnt_id:").map(str::trim))
        else {
            return false;
        };
        // The allocation budget charges uncompressed length. Prefer the packed
        // backend on filesystems without transparent data compression.
        fs::read_to_string("/proc/self/mountinfo").is_ok_and(|mounts| {
            mounts.lines().any(|line| {
                let Some((mount, fs)) = line.split_once(" - btrfs ") else {
                    return false;
                };
                mount.split_whitespace().next() == Some(id)
                    && fs.split_whitespace().nth(1).is_some_and(|o| {
                        o.split(',')
                            .any(|s| s.starts_with("compress=") || s.starts_with("compress-force="))
                    })
            })
        })
    }

    pub fn prepare(src: &File, dir: &Path) -> Result<Object> {
        // Anonymous until the database writer publishes it. An interrupted
        // compression/build therefore leaves no multi-megabyte staging file.
        let writer = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_TMPFILE)
            .open(dir)?;
        clone_file(src, &writer)?;
        let mut file = File::open(format!("/proc/self/fd/{}", writer.as_raw_fd()))?;
        drop(writer); // fs-verity requires no writable handles
        let enable = Enable {
            version: 1,
            algorithm: 1,
            block_size: 4096,
            ..Enable::default()
        };
        let result = unsafe {
            libc::ioctl(
                file.as_raw_fd(),
                libc::_IOW::<Enable>(b'f' as u32, 133) as _,
                &enable,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error().into());
        }
        let verity = measure(&file)?;
        let hash = *blake3::Hasher::new()
            .update_reader(&mut file)?
            .finalize()
            .as_bytes();
        Ok(Object { file, verity, hash })
    }

    pub fn publish(object: &Object, path: &Path) -> Result<()> {
        fs::create_dir_all(path.parent().context("missing object directory")?)?;
        if let Ok(f) = File::open(path)
            && measure(&f).ok() == Some(object.verity)
        {
            return Ok(());
        }
        // This runs under SQLite's exclusive writer lease. The open fds of
        // concurrent readers still refer to the previous inode.
        match fs::remove_file(path) {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        let fd = CString::new(format!("/proc/self/fd/{}", object.file.as_raw_fd()))?;
        let dest = CString::new(path.as_os_str().as_bytes())?;
        let result = unsafe {
            libc::linkat(
                libc::AT_FDCWD,
                fd.as_ptr(),
                libc::AT_FDCWD,
                dest.as_ptr(),
                libc::AT_SYMLINK_FOLLOW,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }

    pub fn copy(source: &mut File, dest: &mut File) -> Result<()> {
        if clone_file(source, dest).is_err() {
            // Cross-device restore. Reads of this authenticated source are
            // verified by the kernel page by page.
            std::io::copy(source, dest)?;
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
pub use linux::{copy, measure, prepare, publish, supported};

#[cfg(not(target_os = "linux"))]
mod fallback {
    use super::*;
    pub fn supported(_: &Path) -> bool {
        false
    }
    pub fn prepare(_: &File, _: &Path) -> Result<Object> {
        anyhow::bail!("sealed storage requires Linux")
    }
    pub fn publish(_: &Object, _: &Path) -> Result<()> {
        anyhow::bail!("sealed storage requires Linux")
    }
    pub fn measure(_: &File) -> std::io::Result<Digest> {
        Err(std::io::Error::other("fs-verity unavailable"))
    }
    pub fn copy(source: &mut File, dest: &mut File) -> Result<()> {
        std::io::copy(source, dest)?;
        Ok(())
    }
}
#[cfg(not(target_os = "linux"))]
pub use fallback::{copy, measure, prepare, publish, supported};

pub fn restore(
    source: &mut File,
    dest: &mut File,
    verity: &Digest,
    hash: &Digest,
    size: u64,
) -> Result<()> {
    ensure!(source.metadata()?.len() == size, Damaged::Object(*hash));
    if measure(source).ok() == Some(*verity) {
        copy(source, dest)?;
    } else {
        // Also supports a cache copied with an ordinary file-copy utility:
        // it can preserve every byte while losing the verity inode attribute.
        let mut buffer = [0; 64 * 1024];
        let mut h = blake3::Hasher::new();
        loop {
            let n = source.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            h.update(&buffer[..n]);
            std::io::Write::write_all(dest, &buffer[..n])?;
        }
        ensure!(h.finalize().as_bytes() == hash, Damaged::Object(*hash));
    }
    ensure!(dest.metadata()?.len() == size, "incomplete sealed restore");
    dest.rewind()?;
    Ok(())
}

// Run under the database writer lease. A kill between linkat and COMMIT can
// leave an otherwise unreachable inode. Only delete recognized object names.
pub fn sweep(dir: &Path, tx: &rusqlite::Transaction<'_>) -> Result<()> {
    let root = dir.join("objects-v1");
    if !root.is_dir() {
        return Ok(());
    }
    for shard in fs::read_dir(root)? {
        let shard = shard?;
        let prefix = shard.file_name();
        let Some(prefix) = prefix.to_str() else {
            continue;
        };
        if prefix.len() != 2 || !shard.file_type()?.is_dir() {
            continue;
        }
        for entry in fs::read_dir(shard.path())? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Ok(hash) = blake3::Hash::from_hex(format!("{prefix}{name}")) else {
                continue;
            };
            let referenced = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM objects WHERE hash=?1)",
                [hash.as_bytes()],
                |r| r.get::<_, bool>(0),
            )?;
            if !referenced && entry.file_type()?.is_file() {
                fs::remove_file(entry.path())?;
            }
        }
        // Empty shards are optional, and another reader may be finishing.
        let _ = fs::remove_dir(shard.path());
    }
    Ok(())
}
