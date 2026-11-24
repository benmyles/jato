//! The cache: a SQLite index beside content-addressed object files.
//!
//! An object holds the bytes of one output. On a filesystem that shares
//! extents (Btrfs, XFS) it is a clone of the file the command wrote, and a
//! restore is another clone: no data is copied in either direction, and the
//! cache and every target directory share one copy on disk. Elsewhere an
//! object is a zstd stream and a restore decompresses it.
//!
//! Object files are renamed into place and unlinked only while this process
//! holds SQLite's writer lock, so the index and the directory agree.
use crate::manifest::{Encoding, Manifest};
use crate::observe::Stamp;
use crate::roots::Roots;
use crate::scan::{self, Digest};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SCHEMA_VERSION: u32 = 2;
const SCHEMA_BUILDS: &str = "
CREATE TABLE IF NOT EXISTS builds(
    recipe BLOB PRIMARY KEY, key BLOB NOT NULL, used INTEGER NOT NULL,
    outputs INTEGER NOT NULL, env_tag BLOB NOT NULL, kind INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS builds_by_tag ON builds(env_tag, used DESC);
";
const SCHEMA: &str = "
CREATE TABLE settings(name TEXT PRIMARY KEY, value INTEGER NOT NULL) WITHOUT ROWID;
CREATE TABLE counters(name TEXT PRIMARY KEY, value INTEGER NOT NULL) WITHOUT ROWID;
CREATE TABLE entries(
    id INTEGER PRIMARY KEY, key BLOB UNIQUE NOT NULL, recipe BLOB NOT NULL,
    used INTEGER NOT NULL, cost_ms INTEGER NOT NULL, manifest BLOB NOT NULL, detail BLOB NOT NULL
);
CREATE INDEX entries_by_recipe ON entries(recipe, used DESC);
CREATE INDEX entries_by_use ON entries(used);
CREATE TABLE outputs(
    entry INTEGER NOT NULL REFERENCES entries ON DELETE CASCADE, object BLOB NOT NULL,
    PRIMARY KEY(entry, object)
) WITHOUT ROWID;
CREATE INDEX outputs_by_object ON outputs(object);
CREATE TABLE objects(
    digest BLOB PRIMARY KEY, size INTEGER NOT NULL, stored INTEGER NOT NULL, packed INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE ran(key BLOB PRIMARY KEY, at INTEGER NOT NULL) WITHOUT ROWID;
CREATE TABLE stamps(
    dev INTEGER NOT NULL, ino INTEGER NOT NULL, size INTEGER NOT NULL, mtime INTEGER NOT NULL,
    ctime INTEGER NOT NULL, digest BLOB NOT NULL, seen INTEGER NOT NULL, PRIMARY KEY(dev, ino)
) WITHOUT ROWID;
CREATE TABLE builds(
    recipe BLOB PRIMARY KEY, key BLOB NOT NULL, used INTEGER NOT NULL,
    outputs INTEGER NOT NULL, env_tag BLOB NOT NULL, kind INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX builds_by_tag ON builds(env_tag, used DESC);
";

/// Results kept for one command line. Older source states of a crate under
/// edit fall out of the cache first.
const VARIANTS: usize = 24;
const ENTRY_OVERHEAD: u64 = 512;
/// Outputs up to this size are read whole to see whether they are text that
/// names a root.
const TEXT_LIMIT: u64 = 1 << 20;
/// Below this, sharing extents saves nothing.
const CLONE_FLOOR: u64 = 16 << 10;

pub fn cache_dir() -> Result<PathBuf> {
    let dir = env::var_os("JATO_CACHE_DIR")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("XDG_CACHE_HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .map(|p| p.join("jato"))
        })
        .or_else(|| env::var_os("HOME").map(|p| PathBuf::from(p).join(".cache/jato")))
        .context("set JATO_CACHE_DIR to a private local directory")?;
    Ok(env::current_dir()?.join(dir))
}

pub fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let multiplier: u64 = match s[split..].trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kib" => 1 << 10,
        "m" | "mib" => 1 << 20,
        "g" | "gib" => 1 << 30,
        "t" | "tib" => 1 << 40,
        _ => bail!("invalid cache size {s:?}, use bytes, KiB, MiB, GiB or TiB"),
    };
    s[..split]
        .parse::<u64>()?
        .checked_mul(multiplier)
        .filter(|n| *n <= i64::MAX as u64)
        .context("cache size too large")
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as i64
}

/// The latest time that anything written to this filesystem from now on is
/// certain to be newer than, as the filesystem itself tells time.
///
/// A file gets the time of the kernel's last clock tick when it is
/// written, which can be a millisecond behind the clock a program reads,
/// or the time of a server. Files given the time by this program's clock
/// could be newer than a source edited right afterwards, and Cargo would
/// take them for up to date.
pub fn just_before_now(beside: &Path) -> Option<libc::timespec> {
    use std::os::unix::fs::MetadataExt;
    static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let serial = SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let probe = beside
        .parent()?
        .join(format!(".jato-time-{}-{serial}", std::process::id()));
    let written = File::create(&probe).and_then(|file| file.metadata());
    let _ = fs::remove_file(&probe);
    let written = written.ok()?;
    // One edited within the same tick gets this very time, so step back.
    let (mut sec, mut nsec) = (written.mtime(), written.mtime_nsec() - 1);
    if nsec < 0 {
        sec -= 1;
        nsec = 999_999_999;
    }
    Some(libc::timespec {
        tv_sec: sec as _,
        tv_nsec: nsec as _,
    })
}

fn wait_for_writer(attempt: i32) -> bool {
    if attempt >= 400 {
        return false;
    }
    let micros = match attempt {
        0..=5 => 50u64 << attempt,
        6..=20 => 2_000,
        _ => 25_000,
    };
    std::thread::sleep(Duration::from_micros(micros));
    true
}

fn filesystem(dir: &Path) -> Option<libc::statfs> {
    let name = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut info: libc::statfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statfs(name.as_ptr(), &raw mut info) } == 0).then_some(info)
}

/// A tenth of the filesystem, between 10 and 100 GiB. Clones share their
/// data with the target directories they came from, so most of this is not
/// extra disk space.
fn default_budget(dir: &Path) -> u64 {
    const GIB: u64 = 1 << 30;
    // The field types differ between platforms.
    #[allow(clippy::unnecessary_cast)]
    let total = filesystem(dir).map_or(0, |info| {
        (info.f_blocks as u64).saturating_mul(info.f_bsize as u64)
    });
    (total / 10).clamp(10 * GIB, 100 * GIB)
}

/// Whether a write to a file is certain to change its stamp. Linux 6.13
/// gives a file a fresh fine-grained change time whenever the previous one
/// has been looked at; before that, two writes in one clock tick can share
/// a time.
fn fine_grained_times() -> bool {
    use std::sync::OnceLock;
    static FINE: OnceLock<bool> = OnceLock::new();
    *FINE.get_or_init(|| {
        let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
        if !cfg!(target_os = "linux") || unsafe { libc::uname(&raw mut uts) } != 0 {
            return false;
        }
        let release = unsafe { std::ffi::CStr::from_ptr(uts.release.as_ptr()) }.to_string_lossy();
        let mut parts = release
            .split(['.', '-'])
            .map(|p| p.parse::<u32>().unwrap_or(0));
        (parts.next().unwrap_or(0), parts.next().unwrap_or(0)) >= (6, 13)
    })
}

#[derive(Serialize)]
pub struct Stats {
    pub directory: PathBuf,
    pub limit_bytes: u64,
    pub memory_bytes: u64,
    pub hot_bytes: u64,
    pub stored_bytes: u64,
    pub artifact_bytes: u64,
    pub database_bytes: u64,
    pub entries: u64,
    pub objects: u64,
    pub remembered_files: u64,
    pub counters: BTreeMap<String, u64>,
    /// Why the latest build that ran under watch was not recorded as a
    /// whole, and how many seconds ago that was.
    pub unrecorded: Option<(u64, String)>,
    /// Why the latest build that was recorded is only put back in the
    /// target directory it was made in, if that is so.
    pub only_there: Option<(u64, String)>,
    /// Whether the filesystem the cache is on can clone files, so that
    /// results are stored and put back without being copied.
    pub clones: bool,
}

/// Whether one file in `dir` can be made a clone of another.
fn clones(dir: &Path) -> bool {
    let name = |which: &str| dir.join(format!("tmp-clone-{}-{which}", std::process::id()));
    let (from, to) = (name("a"), name("b"));
    let made = (|| -> io::Result<()> {
        fs::write(&from, [0u8; 4096])?;
        clone_file(&File::open(&from)?, &File::create(&to)?)
    })();
    let _ = fs::remove_file(&from);
    let _ = fs::remove_file(&to);
    made.is_ok()
}

/// A record that may stand for a command, before it has been read.
pub struct Candidate {
    pub key: Digest,
    pub used: i64,
}

/// One output of a command, copied into the cache directory but not yet
/// part of the cache.
pub struct Staged {
    pub digest: Digest,
    pub size: u64,
    pub mode: u32,
    /// Whether and how the roots were replaced by marks in the stored bytes.
    pub encoding: Encoding,
    /// For each root spelling asked about, whether these bytes name it.
    pub names: Vec<bool>,
    temp: Option<PathBuf>,
    packed: bool,
    stored: u64,
}

impl Drop for Staged {
    fn drop(&mut self) {
        if let Some(temp) = &self.temp {
            let _ = fs::remove_file(temp);
        }
    }
}

pub struct Restored {
    pub detail: Vec<u8>,
    pub cost_ms: u64,
    pub bytes: u64,
    /// How many files had to be written, and how long it took to see
    /// which, to write them beside where they belong, and to move them
    /// there and give them their time.
    pub files: usize,
    pub took: [std::time::Duration; 3],
}

pub struct Store {
    conn: Connection,
    pub dir: PathBuf,
    env_limit: Option<u64>,
    env_memory: Option<u64>,
    /// Whether restored bytes are read back and compared with their digest.
    verify: bool,
    /// Whether to try sharing extents with the files commands write.
    clone: bool,
    /// Whether `JATO_STORAGE=packed` explicitly disables uncompressed objects.
    force_packed: bool,
    known: HashMap<(u64, u64), (Stamp, Digest)>,
    stamps_loaded: bool,
    learned: Vec<(Stamp, Digest)>,
    counts: BTreeMap<String, i64>,
    used: Vec<Digest>,
    noted: Vec<Digest>,
}

/// Whether an error says the index file itself cannot be trusted.
pub fn damaged_index(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(e, _))
                if matches!(
                    e.code,
                    rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
                )
        )
    })
}

/// Move a damaged index out of the way. Its objects stay, to be collected
/// by `jato gc` or adopted again as the same outputs are stored.
pub fn set_aside(dir: &Path) {
    let stamp = now();
    for suffix in ["", "-wal", "-shm"] {
        let from = dir.join(format!("index-v2.sqlite3{suffix}"));
        let _ = fs::rename(&from, dir.join(format!("damaged-{stamp}{suffix}")));
    }
}

fn clone_file(source: &File, dest: &File) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        if unsafe { libc::ioctl(dest.as_raw_fd(), libc::FICLONE as _, source.as_raw_fd()) } == 0 {
            return Ok(());
        }
        Err(io::Error::last_os_error())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (source, dest);
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

fn hex(digest: &Digest) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(64);
    for b in digest {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn object_path(dir: &Path, digest: &Digest, packed: bool) -> PathBuf {
    let name = hex(digest);
    let mut path = dir.join("objects");
    path.push(&name[..2]);
    path.push(if packed {
        format!("{}.zst", &name[2..])
    } else {
        name[2..].to_owned()
    });
    path
}

/// How a result's files are to be put in place.
#[derive(Clone, Copy, Default)]
pub struct Placing<'a> {
    /// A whole build's: files that already hold the right bytes are left,
    /// and the others are written on several threads.
    pub tree: bool,
    /// Outputs to leave out, and to remove where they are.
    pub leave: Option<&'a [bool]>,
    /// The time to give every file of a whole build.
    pub time: Option<libc::timespec>,
    /// Override the destination path of a single-output result (for C
    /// compilations whose `-o` filename carries a per-checkout hash).
    pub remap_out: Option<&'a Path>,
}

/// One output to put in place.
struct Work<'a> {
    /// Its place among the outputs.
    at: usize,
    output: &'a crate::manifest::Output,
    dest: &'a PathBuf,
    /// Where the object is kept, and whether compressed.
    source: PathBuf,
    packed: bool,
}

struct How<'a> {
    verify: bool,
    clone: bool,
    roots: &'a Roots,
    /// The time to give each file, if all of a build's get one.
    time: Option<libc::timespec>,
}

/// Write one output to a staging file beside its destination. Returns the
/// staging file if one was created, and Some(true) if it holds the output,
/// Some(false) if the object is not what it should be, None if this
/// command lacks a root the output names.
fn place(work: Work<'_>, how: &How<'_>) -> (Option<PathBuf>, Result<Option<bool>>) {
    let Work {
        output,
        dest,
        source,
        packed,
        ..
    } = work;
    let Some(file_name) = dest.file_name() else {
        return (None, Err(anyhow::anyhow!("output without a file name")));
    };
    // Opened only now: a build has more files than a process may hold
    // open. One evicted since it was looked for is a result that is not
    // all there, like one that was never whole.
    let mut source = match File::open(&source) {
        Ok(source) => source,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return (None, Ok(Some(false))),
        Err(e) => return (None, Err(e.into())),
    };
    let mut name = file_name.to_owned();
    name.push(format!(".jato-{}", std::process::id()));
    let staging = dest.with_file_name(name);
    let open = |path: &Path| {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .mode(output.mode & 0o777)
            .open(path)
    };
    let mut out = match open(&staging) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let made = fs::create_dir_all(staging.parent().unwrap()).and_then(|()| open(&staging));
            match made {
                Ok(f) => f,
                Err(e) => return (None, Err(e.into())),
            }
        }
        Err(e) => return (None, Err(e.into())),
    };
    let written = (|| -> Result<Option<bool>> {
        if output.encoding.is_marked() {
            ensure!(output.size <= TEXT_LIMIT * 4, "oversized marked object");
            let mut raw = Vec::with_capacity(output.size as usize);
            source.read_to_end(&mut raw)?;
            if blake3::hash(&raw).as_bytes() != &output.digest {
                return Ok(Some(false));
            }
            let restored = match output.encoding {
                Encoding::Text => how.roots.unmark(&raw),
                Encoding::CargoDep => {
                    crate::roots::unmark_cargo_dep(&raw, how.roots).map(std::borrow::Cow::Owned)
                }
                Encoding::Raw => unreachable!(),
            };
            let Some(restored) = restored else {
                return Ok(None);
            };
            out.write_all(&restored)?;
        } else if packed {
            let mut hasher = blake3::Hasher::new();
            let mut decoder = zstd::stream::read::Decoder::new(&mut source)?;
            let mut buffer = vec![0u8; 256 * 1024];
            let mut written = 0u64;
            loop {
                let n = decoder.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buffer[..n]);
                out.write_all(&buffer[..n])?;
                written += n as u64;
            }
            if written != output.size || hasher.finalize().as_bytes() != &output.digest {
                return Ok(Some(false));
            }
        } else {
            // An object's length is part of its identity.
            if source.metadata()?.len() != output.size {
                return Ok(Some(false));
            }
            let cloned = how.clone && clone_file(&source, &out).is_ok();
            if cloned {
                if how.verify {
                    use std::io::Seek;
                    out.rewind()?;
                    let (digest, length, _) = scan::digest_open(&mut out, &[])?;
                    if length != output.size || digest != output.digest {
                        return Ok(Some(false));
                    }
                }
            } else if how.verify {
                // Single-pass verified copy: hash with SIMD BLAKE3 while
                // writing, avoiding a second read pass.
                let mut hasher = blake3::Hasher::new();
                let mut buffer = vec![0u8; 256 * 1024];
                let mut written = 0u64;
                loop {
                    let n = source.read(&mut buffer)?;
                    if n == 0 {
                        break;
                    }
                    hasher.update(&buffer[..n]);
                    out.write_all(&buffer[..n])?;
                    written += n as u64;
                }
                if written != output.size || hasher.finalize().as_bytes() != &output.digest {
                    return Ok(Some(false));
                }
            } else {
                let copied = io::copy(&mut source, &mut out)?;
                ensure!(copied == output.size, "short copy of a cached object");
            }
        }
        // The umask applied at creation. Outputs keep their mode.
        out.set_permissions(fs::Permissions::from_mode(output.mode & 0o777))?;
        if let Some(time) = how.time {
            let times = [
                libc::timespec {
                    tv_sec: 0,
                    tv_nsec: libc::UTIME_OMIT,
                },
                time,
            ];
            if unsafe { libc::futimens(out.as_raw_fd(), times.as_ptr()) } != 0 {
                return Err(io::Error::last_os_error().into());
            }
        }
        Ok(Some(true))
    })();
    (Some(staging), written)
}

/// Whether `meta` still reflects the same file contents as `before` (taking
/// into account that Cargo hardlinking a finished artifact bumps `ctime` and
/// `nlink` without changing the file's bytes).
fn unchanged_since(before: Stamp, before_nlink: u64, meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    let after = Stamp::of(meta);
    after == before || (before.same_contents(&after) && meta.nlink() != before_nlink)
}

/// Copy one output into the cache directory `dir` and learn its digest and
/// which of `roots`' spellings its bytes name; `clone` says whether to try
/// sharing its extents, and `hot` says whether to store uncompressed in the
/// hot memory tier when cloning is unavailable. With it comes what the file
/// looked like while it held those bytes, if it was at rest, to know it by
/// later.
fn stage_file(
    dir: &Path,
    clone: bool,
    hot: bool,
    file: &Path,
    roots: &Roots,
) -> Result<(Staged, Option<(Stamp, Digest)>)> {
    use std::os::unix::fs::MetadataExt;
    let spellings = roots.spellings();
    let needles: Vec<&[u8]> = spellings.iter().map(|(_, s)| *s).collect();
    let mut source = File::open(file).with_context(|| format!("opening {}", file.display()))?;
    let meta = source.metadata()?;
    ensure!(meta.is_file(), "{} is not a regular file", file.display());
    let before = Stamp::of(&meta);
    let before_nlink = meta.nlink();
    let mode = meta.permissions().mode() & 0o777;
    // One process can hold several stores, each staging at once.
    static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let serial = SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp = dir
        .join("objects")
        .join(format!("tmp-{}-{serial}", std::process::id()));
    let mut staged = Staged {
        digest: [0; 32],
        size: before.size,
        mode,
        encoding: Encoding::Raw,
        names: vec![false; needles.len()],
        temp: Some(temp.clone()),
        packed: false,
        stored: before.size,
    };
    let create = || {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
    };
    if before.size <= TEXT_LIMIT {
        let mut bytes = Vec::with_capacity(before.size as usize);
        source.read_to_end(&mut bytes)?;
        let names: Vec<bool> = needles
            .iter()
            .map(|n| scan::names_path(&bytes, n))
            .collect();
        let has_root = names.contains(&true);
        let text = has_root && memchr::memchr(0, &bytes).is_none();
        let cargo_dep = if !text
            && has_root
            && file
                .file_name()
                .is_some_and(|n| n.as_bytes().starts_with(b"dep-"))
        {
            crate::roots::mark_cargo_dep(&bytes, roots)
        } else {
            None
        };
        if text || cargo_dep.is_some() || before.size < CLONE_FLOOR {
            let (encoding, stored): (Encoding, std::borrow::Cow<'_, [u8]>) = if text {
                (Encoding::Text, roots.mark(&bytes))
            } else if let Some(marked) = cargo_dep {
                (Encoding::CargoDep, std::borrow::Cow::Owned(marked))
            } else {
                (Encoding::Raw, (&bytes[..]).into())
            };
            staged.digest = *blake3::hash(&stored).as_bytes();
            if object_path(dir, &staged.digest, false).is_file() {
                staged.temp = None;
            } else {
                let mut out = create()?;
                out.write_all(&stored)?;
                out.set_permissions(fs::Permissions::from_mode(0o444))?;
            }
            staged.size = stored.len() as u64;
            staged.stored = stored.len() as u64;
            staged.encoding = encoding;
            staged.names = if encoding.is_marked() {
                vec![false; needles.len()]
            } else {
                names
            };
            let after_meta = source.metadata()?;
            ensure!(
                unchanged_since(before, before_nlink, &after_meta),
                "{} changed while it was stored",
                file.display()
            );
            // For text or Cargo dep files that name a root this is what
            // they say with the roots' names taken out, which is all a
            // command that reads them there learns that it would not learn
            // anywhere.
            let digest = staged.digest;
            return Ok((staged, Some((Stamp::of(&after_meta), digest))));
        }
        use std::io::Seek;
        source.rewind()?;
    }
    let mut out = create()?;
    if clone && clone_file(&source, &out).is_ok() {
        // The clone is a private snapshot. Its digest is the digest of
        // what the cache holds, whatever happens to the original.
        let (digest, length, names) = scan::digest_open(&mut out, &needles)?;
        ensure!(length == before.size, "{} changed size", file.display());
        staged.digest = digest;
        staged.names = names;
    } else if hot {
        // High-RAM hot tier on a non-cloning filesystem: store uncompressed in
        // a single read+hash+scan+write pass so objects stay page-cached and
        // restore at full SIMD BLAKE3 memory bandwidth with zero zstd cost.
        let mut hasher = blake3::Hasher::new();
        let mut buffer = vec![0u8; 256 * 1024];
        let mut tail: Vec<u8> = Vec::new();
        let keep = needles.iter().map(|n| n.len() + 1).max().unwrap_or(0);
        let mut length = 0u64;
        loop {
            let n = source.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
            out.write_all(&buffer[..n])?;
            length += n as u64;
            if keep > 0 {
                tail.extend_from_slice(&buffer[..n]);
                for (needle, found) in needles.iter().zip(&mut staged.names) {
                    *found = *found || scan::names_path(&tail, needle);
                }
                let drop = tail.len().saturating_sub(keep);
                tail.drain(..drop);
            }
        }
        ensure!(length == before.size, "{} changed size", file.display());
        let after_meta = source.metadata()?;
        ensure!(
            unchanged_since(before, before_nlink, &after_meta),
            "{} changed while it was stored",
            file.display()
        );
        let digest = *hasher.finalize().as_bytes();
        staged.digest = digest;
        for packed in [false, true] {
            if let Ok(meta) = fs::metadata(object_path(dir, &digest, packed))
                && meta.is_file()
            {
                drop(out);
                let _ = fs::remove_file(&temp);
                staged.temp = None;
                staged.packed = packed;
                staged.stored = meta.len();
                return Ok((staged, Some((Stamp::of(&after_meta), digest))));
            }
        }
        staged.packed = false;
        staged.stored = before.size;
    } else {
        // No shared extents here: a copy is kept, compressed. Look
        // first at whether the cache has the file already. Most of
        // what a build made was stored as each compiler finished, and
        // a file is read many times faster than it is compressed.
        use std::io::Seek;
        let (digest, length, names) = scan::digest_open(&mut source, &needles)?;
        ensure!(length == before.size, "{} changed size", file.display());
        for packed in [false, true] {
            if let Ok(meta) = fs::metadata(object_path(dir, &digest, packed))
                && meta.is_file()
            {
                let after_meta = source.metadata()?;
                ensure!(
                    unchanged_since(before, before_nlink, &after_meta),
                    "{} changed while it was stored",
                    file.display()
                );
                drop(out);
                let _ = fs::remove_file(&temp);
                staged.digest = digest;
                staged.names = names;
                staged.temp = None;
                staged.packed = packed;
                staged.stored = meta.len();
                return Ok((staged, Some((Stamp::of(&after_meta), digest))));
            }
        }
        source.rewind()?;
        let mut hasher = blake3::Hasher::new();
        let mut encoder = zstd::stream::write::Encoder::new(&mut out, 1)?;
        let mut buffer = vec![0u8; 256 * 1024];
        let mut tail: Vec<u8> = Vec::new();
        let keep = needles.iter().map(|n| n.len() + 1).max().unwrap_or(0);
        let mut length = 0u64;
        loop {
            let n = source.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
            encoder.write_all(&buffer[..n])?;
            length += n as u64;
            // Look for roots in this block and across its start.
            tail.extend_from_slice(&buffer[..n]);
            for (needle, found) in needles.iter().zip(&mut staged.names) {
                *found = *found || scan::names_path(&tail, needle);
            }
            let drop = tail.len().saturating_sub(keep);
            tail.drain(..drop);
        }
        encoder.finish()?;
        ensure!(length == before.size, "{} changed size", file.display());
        staged.digest = *hasher.finalize().as_bytes();
        ensure!(
            digest == staged.digest,
            "{} changed while it was stored",
            file.display()
        );
        staged.packed = true;
        staged.stored = out.metadata()?.len();
    }
    out.set_permissions(fs::Permissions::from_mode(0o444))?;
    let after_meta = source.metadata()?;
    let learned = unchanged_since(before, before_nlink, &after_meta)
        .then_some((Stamp::of(&after_meta), staged.digest));
    Ok((staged, learned))
}

impl Store {
    pub fn open() -> Result<Self> {
        let env_limit = env::var("JATO_MAX_SIZE")
            .ok()
            .map(|s| parse_size(&s))
            .transpose()?;
        let env_memory = env::var("JATO_MEMORY")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(|s| parse_size(&s))
            .transpose()?;
        let dir = cache_dir()?;
        let mut store = match Self::at(dir.clone()) {
            Err(e) if damaged_index(&e) => {
                set_aside(&dir);
                Self::at(dir)?
            }
            other => other?,
        };
        store.env_limit = env_limit;
        store.env_memory = env_memory;
        store.tune_memory();
        match env::var("JATO_STORAGE").as_deref() {
            Ok("packed") => {
                store.clone = false;
                store.force_packed = true;
            }
            Ok("auto") | Err(_) => (),
            Ok(other) => bail!("JATO_STORAGE must be auto or packed, not {other:?}"),
        }
        match env::var("JATO_VERIFY").as_deref() {
            Ok("1") => store.verify = true,
            Ok("0") => store.verify = false,
            _ => (),
        }
        Ok(store)
    }

    pub fn at(dir: PathBuf) -> Result<Self> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        let conn = Connection::open(dir.join("index-v2.sqlite3"))?;
        conn.set_db_config(
            rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
            true,
        )?;
        conn.set_prepared_statement_cache_capacity(48);
        conn.busy_handler(Some(wait_for_writer))?;
        // Nothing is forced to disk: after a power failure the index may be
        // damaged, and a damaged index is set aside. A short log keeps the
        // cost of opening the index low for each compiler command.
        conn.execute_batch(
            "PRAGMA foreign_keys=ON; PRAGMA cache_size=-2048; PRAGMA mmap_size=268435456;
             PRAGMA synchronous=OFF; PRAGMA wal_autocheckpoint=128;
             PRAGMA journal_size_limit=1048576;",
        )?;
        let version: u32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        ensure!(
            version <= SCHEMA_VERSION,
            "this cache was written by a newer Jato (format {version})"
        );
        if version < SCHEMA_VERSION {
            let lock = OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(dir.join("init.lock"))?;
            lock.lock()?;
            let version: u32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if version == 0 {
                conn.execute_batch("PRAGMA journal_mode=WAL;")?;
                conn.execute_batch(&format!(
                    "BEGIN IMMEDIATE; {SCHEMA} PRAGMA user_version={SCHEMA_VERSION}; COMMIT;"
                ))?;
                for sub in ["objects", "locks"] {
                    fs::DirBuilder::new()
                        .recursive(true)
                        .mode(0o700)
                        .create(dir.join(sub))?;
                }
            } else if version < 2 {
                conn.execute_batch(&format!(
                    "BEGIN IMMEDIATE; {SCHEMA_BUILDS} PRAGMA user_version={SCHEMA_VERSION}; COMMIT;"
                ))?;
            }
        }
        // Btrfs checksums every block it returns. Elsewhere a damaged
        // object would go unnoticed unless its bytes are read back.
        const BTRFS: i64 = 0x9123_683e;
        #[allow(clippy::unnecessary_cast)]
        let verify = filesystem(&dir).is_none_or(|info| info.f_type as i64 != BTRFS);
        Ok(Self {
            conn,
            dir,
            env_limit: None,
            env_memory: None,
            verify,
            clone: true,
            force_packed: false,
            known: HashMap::new(),
            stamps_loaded: false,
            learned: Vec::new(),
            counts: BTreeMap::new(),
            used: Vec::new(),
            noted: Vec::new(),
        })
    }

    /// Configured RAM budget for accelerating builds (SQLite page cache / mmap
    /// and uncompressed hot object storage on non-cloning filesystems).
    pub fn memory_budget(&self) -> Result<u64> {
        let stored: Option<u64> = self
            .conn
            .query_row("SELECT value FROM settings WHERE name='memory'", [], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(self.env_memory.or(stored).unwrap_or(0))
    }

    pub fn set_memory_budget(&mut self, size: Option<u64>) -> Result<u64> {
        match size {
            Some(size) if size > 0 => self.conn.execute(
                "INSERT INTO settings VALUES('memory',?1) ON CONFLICT(name) DO UPDATE SET value=excluded.value",
                [size],
            )?,
            _ => self
                .conn
                .execute("DELETE FROM settings WHERE name='memory'", [])?,
        };
        self.tune_memory();
        self.memory_budget()
    }

    /// Scale SQLite's page cache and memory-mapped I/O when a memory budget
    /// is configured so the entire index stays resident in RAM.
    fn tune_memory(&self) {
        let Ok(mem) = self.memory_budget() else {
            return;
        };
        if mem == 0 {
            return;
        }
        let cache_kib = (mem / 64 / 1024).clamp(4096, 512 * 1024);
        let mmap = (mem / 8).clamp(256 << 20, 2 << 30);
        let _ = self.conn.execute_batch(&format!(
            "PRAGMA cache_size=-{cache_kib}; PRAGMA mmap_size={mmap};"
        ));
    }

    /// Whether newly staged objects on a non-cloning filesystem should be kept
    /// uncompressed in the hot memory tier (staying page-cached in RAM for
    /// single-pass SIMD BLAKE3 restores without zstd compression overhead).
    fn use_hot_tier(&self) -> bool {
        if self.force_packed {
            return false;
        }
        let Ok(mem) = self.memory_budget() else {
            return false;
        };
        if mem == 0 {
            return false;
        }
        let current: u64 = self
            .conn
            .prepare_cached("SELECT COALESCE(sum(stored), 0) FROM objects WHERE packed=0")
            .and_then(|mut s| s.query_row([], |r| r.get::<_, i64>(0)))
            .unwrap_or(0)
            .max(0) as u64;
        current < mem
    }

    /// Bulk-load remembered file stamps into memory in one sequential scan so
    /// parallel manifest checks need zero SQLite queries per input.
    pub fn preload_stamps(&mut self) {
        if self.stamps_loaded {
            return;
        }
        self.stamps_loaded = true;
        let Ok(mut stmt) = self
            .conn
            .prepare("SELECT dev,ino,size,mtime,ctime,digest FROM stamps")
        else {
            return;
        };
        let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                Stamp {
                    dev: r.get::<_, i64>(0)? as u64,
                    ino: r.get::<_, i64>(1)? as u64,
                    size: r.get::<_, i64>(2)? as u64,
                    mtime: r.get(3)?,
                    ctime: r.get(4)?,
                },
                r.get::<_, Digest>(5)?,
            ))
        }) else {
            return;
        };
        for (stamp, digest) in rows.flatten() {
            self.known
                .entry((stamp.dev, stamp.ino))
                .or_insert((stamp, digest));
        }
    }

    pub fn known_stamps(&self) -> &HashMap<(u64, u64), (Stamp, Digest)> {
        &self.known
    }

    pub fn remember_many(&mut self, learned: Vec<(Stamp, Digest)>, hashed_bytes: u64) {
        for (stamp, digest) in learned {
            self.remember(stamp, digest);
        }
        if hashed_bytes > 0 {
            self.count("hashed_bytes", hashed_bytes);
        }
    }

    pub fn budget(&self) -> Result<u64> {
        let stored: Option<u64> = self
            .conn
            .query_row("SELECT value FROM settings WHERE name='limit'", [], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(stored
            .or(self.env_limit)
            .unwrap_or_else(|| default_budget(&self.dir)))
    }

    pub fn set_budget(&mut self, size: Option<u64>) -> Result<u64> {
        match size {
            Some(size) => self.conn.execute(
                "INSERT INTO settings VALUES('limit',?1) ON CONFLICT(name) DO UPDATE SET value=excluded.value",
                [size],
            )?,
            None => self
                .conn
                .execute("DELETE FROM settings WHERE name='limit'", [])?,
        };
        self.budget()
    }

    /// Count an event. Written with everything else by `flush`.
    pub fn count(&mut self, name: &str, value: u64) {
        *self.counts.entry(name.to_owned()).or_default() += value as i64;
    }

    // ---- file contents ----------------------------------------------------

    /// The digest recorded for exactly this state of a file, if any.
    pub fn recall(&mut self, stamp: &Stamp) -> Option<Digest> {
        if let Some((known, digest)) = self.known.get(&(stamp.dev, stamp.ino)) {
            return (known == stamp).then_some(*digest);
        }
        let (size, mtime, ctime, digest) = self
            .conn
            .prepare_cached("SELECT size,mtime,ctime,digest FROM stamps WHERE dev=?1 AND ino=?2")
            .and_then(|mut s| {
                s.query_row(params![stamp.dev as i64, stamp.ino as i64], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, Digest>(3)?,
                    ))
                })
                .optional()
            })
            .ok()
            .flatten()?;
        let found = Stamp {
            size: size as u64,
            mtime,
            ctime,
            ..*stamp
        };
        self.known.insert((stamp.dev, stamp.ino), (found, digest));
        (found == *stamp).then_some(digest)
    }

    /// Record that a file in this state has these contents.
    pub fn remember(&mut self, stamp: Stamp, digest: Digest) {
        if !fine_grained_times() {
            // A write in the same clock tick as the last one would leave
            // this stamp unchanged. Only a file at rest can be trusted.
            let age = now() as i128 * 1000 - stamp.mtime.max(stamp.ctime) as i128;
            if age < 50_000_000 {
                return;
            }
        }
        self.known.insert((stamp.dev, stamp.ino), (stamp, digest));
        self.learned.push((stamp, digest));
    }

    /// The contents of a regular file that was in state `stamp` when the
    /// caller looked. Unchanged files are not read again.
    pub fn digest_known(&mut self, path: &Path, stamp: Stamp) -> io::Result<Digest> {
        if let Some(digest) = self.recall(&stamp) {
            return Ok(digest);
        }
        let (digest, read, _) = scan::digest_file(path, &[])?;
        if !stamp.same_contents(&read) {
            return Err(io::Error::other("file changed while it was read"));
        }
        // As the file is now, which is what the next look will find.
        self.remember(read, digest);
        self.count("hashed_bytes", stamp.size);
        Ok(digest)
    }

    /// What a command that examined this file can have learned about what
    /// is in it: its contents, or for a file that may not be read, which
    /// the command could not read either, what can be seen from outside.
    pub fn digest_seen(&mut self, path: &Path, stamp: Stamp) -> io::Result<Digest> {
        match self.digest_known(path, stamp) {
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                let mut hash = blake3::Hasher::new_derive_key("jato file that may not be read v1");
                hash.update(&stamp.size.to_le_bytes());
                hash.update(&stamp.mtime.to_le_bytes());
                Ok(*hash.finalize().as_bytes())
            }
            digest => digest,
        }
    }

    /// What a command learned from a file it read inside one of its
    /// roots that the build itself may have written: a generated source
    /// file, something Cargo keeps about a build script, or a Rust library
    /// archive. Text and Cargo `dep-*` files that name a root are known by
    /// what they say with the roots' names taken out. Incremental `.rlib`
    /// archives are known by their symbol table, metadata, and object
    /// payloads without the per-compilation random CGU session suffix.
    pub fn digest_placed(
        &mut self,
        path: &Path,
        stamp: Stamp,
        roots: &Roots,
    ) -> io::Result<Digest> {
        // Incremental `.rlib` archives get a session-independent digest
        // cached under a distinct virtual device-id bit so it never
        // collides with the raw file digest used for object storage.
        const RLIB_STAMP_BIT: u64 = 1 << 62;
        if path.extension() == Some(std::ffi::OsStr::new("rlib")) && stamp.dev & RLIB_STAMP_BIT == 0
        {
            let tagged = Stamp {
                dev: stamp.dev | RLIB_STAMP_BIT,
                ..stamp
            };
            if let Some(digest) = self.recall(&tagged) {
                return Ok(digest);
            }
            if let Some((digest, read)) = scan::digest_rlib(path)? {
                if !stamp.same_contents(&read) {
                    return Err(io::Error::other("file changed while it was read"));
                }
                self.remember(
                    Stamp {
                        dev: read.dev | RLIB_STAMP_BIT,
                        ..read
                    },
                    digest,
                );
                self.count("hashed_bytes", stamp.size);
                return Ok(digest);
            }
        }
        if let Some(digest) = self.recall(&stamp) {
            return Ok(digest);
        }
        if stamp.size <= TEXT_LIMIT
            && let Ok(bytes) = fs::read(path)
        {
            let spellings = roots.spellings();
            let names = spellings
                .iter()
                .any(|(_, spelling)| scan::names_path(&bytes, spelling));
            if names {
                let marked = if memchr::memchr(0, &bytes).is_none() {
                    Some(roots.mark(&bytes))
                } else if path
                    .file_name()
                    .is_some_and(|n| n.as_bytes().starts_with(b"dep-"))
                {
                    crate::roots::mark_cargo_dep(&bytes, roots).map(std::borrow::Cow::Owned)
                } else {
                    None
                };
                if let Some(marked) = marked {
                    let read = fs::metadata(path).map(|meta| Stamp::of(&meta))?;
                    if !stamp.same_contents(&read) {
                        return Err(io::Error::other("file changed while it was read"));
                    }
                    let digest = *blake3::hash(&marked).as_bytes();
                    self.remember(read, digest);
                    return Ok(digest);
                }
            }
        }
        self.digest_seen(path, stamp)
    }
}

/// Result of a lock-free file digest check: the digest, an optional stamp to
/// remember, and the number of file bytes newly hashed.
pub type CheckDigest = (Digest, Option<(Stamp, Digest)>, u64);

/// Lock-free file digest lookup for parallel manifest checking. Uses `known`
/// stamps first and returns `(digest, Option<(stamp_to_remember, digest)>, hashed_bytes)`.
pub fn digest_for_check(
    path: &Path,
    stamp: Stamp,
    in_target: bool,
    roots: &Roots,
    known: &HashMap<(u64, u64), (Stamp, Digest)>,
) -> io::Result<CheckDigest> {
    const RLIB_STAMP_BIT: u64 = 1 << 62;
    let recall = |s: &Stamp| -> Option<Digest> {
        known
            .get(&(s.dev, s.ino))
            .filter(|(k, _)| k == s)
            .map(|(_, d)| *d)
    };
    if in_target {
        if path.extension() == Some(std::ffi::OsStr::new("rlib")) && stamp.dev & RLIB_STAMP_BIT == 0
        {
            let tagged = Stamp {
                dev: stamp.dev | RLIB_STAMP_BIT,
                ..stamp
            };
            if let Some(digest) = recall(&tagged) {
                return Ok((digest, None, 0));
            }
            if let Some((digest, read)) = scan::digest_rlib(path)? {
                if !stamp.same_contents(&read) {
                    return Err(io::Error::other("file changed while it was read"));
                }
                let learned = Stamp {
                    dev: read.dev | RLIB_STAMP_BIT,
                    ..read
                };
                return Ok((digest, Some((learned, digest)), stamp.size));
            }
        }
        if let Some(digest) = recall(&stamp) {
            return Ok((digest, None, 0));
        }
        if stamp.size <= TEXT_LIMIT
            && let Ok(bytes) = fs::read(path)
        {
            let spellings = roots.spellings();
            let names = spellings
                .iter()
                .any(|(_, spelling)| scan::names_path(&bytes, spelling));
            if names {
                let marked = if memchr::memchr(0, &bytes).is_none() {
                    Some(roots.mark(&bytes))
                } else if path
                    .file_name()
                    .is_some_and(|n| n.as_bytes().starts_with(b"dep-"))
                {
                    crate::roots::mark_cargo_dep(&bytes, roots).map(std::borrow::Cow::Owned)
                } else {
                    None
                };
                if let Some(marked) = marked {
                    let read = fs::metadata(path).map(|meta| Stamp::of(&meta))?;
                    if !stamp.same_contents(&read) {
                        return Err(io::Error::other("file changed while it was read"));
                    }
                    let digest = *blake3::hash(&marked).as_bytes();
                    return Ok((digest, Some((read, digest)), 0));
                }
            }
        }
    }
    if let Some(digest) = recall(&stamp) {
        return Ok((digest, None, 0));
    }
    match scan::digest_file(path, &[]) {
        Ok((digest, read, _)) => {
            if !stamp.same_contents(&read) {
                return Err(io::Error::other("file changed while it was read"));
            }
            Ok((digest, Some((read, digest)), stamp.size))
        }
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
            let mut hash = blake3::Hasher::new_derive_key("jato file that may not be read v1");
            hash.update(&stamp.size.to_le_bytes());
            hash.update(&stamp.mtime.to_le_bytes());
            Ok((*hash.finalize().as_bytes(), None, 0))
        }
        Err(e) => Err(e),
    }
}

impl Store {
    // ---- lookups ----------------------------------------------------------

    /// Read from one consistent state of the index until `end_read`,
    /// without taking and dropping a lock for every question.
    pub fn begin_read(&self) {
        let _ = self.conn.execute_batch("BEGIN");
    }

    pub fn end_read(&self) {
        let _ = self.conn.execute_batch("COMMIT");
    }

    /// Earlier results for a command line, most recently used first. The
    /// records themselves are read one at a time as they are looked at: a
    /// whole build's is megabytes, and the first is usually the one.
    pub fn candidates(&self, recipe: &Digest) -> Result<Vec<Candidate>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT key,used FROM entries WHERE recipe=?1 ORDER BY used DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![recipe, VARIANTS as i64], |r| {
            Ok(Candidate {
                key: r.get(0)?,
                used: r.get(1)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// A candidate's record as it is stored, if it is still kept.
    pub fn record(&self, candidate: &Candidate) -> Option<Vec<u8>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT manifest FROM entries WHERE key=?1")
            .ok()?;
        stmt.query_row([&candidate.key], |r| r.get(0)).ok()
    }

    /// How long ago `key` was last noted, in seconds, and note it now.
    pub fn since_noted(&mut self, key: &Digest) -> Option<u64> {
        let at: Option<i64> = self
            .conn
            .prepare_cached("SELECT at FROM ran WHERE key=?1")
            .and_then(|mut s| s.query_row([key], |r| r.get(0)).optional())
            .unwrap_or(None);
        self.noted.push(*key);
        at.map(|at| (now() - at).max(0) as u64 / 1_000_000)
    }

    /// Changes whenever another process commits to the index.
    pub fn data_version(&self) -> u32 {
        self.conn
            .query_row("PRAGMA data_version", [], |r| r.get(0))
            .unwrap_or(0)
    }

    /// Write a result's files where they belong here and return what the
    /// command printed. None if an object is gone or damaged, or the result
    /// names a root this command lacks; nothing is left behind in that case
    /// and `damaged` says whether the result should be forgotten.
    ///
    /// With `tree`, files that already hold the right bytes are left alone,
    /// the others are written on several threads, and if any were written
    /// all are given one modification time, so that Cargo finds none of
    /// them newer than another.
    ///
    /// The outputs `leave` marks are not put in place, and whatever is
    /// where they belong is taken away: Cargo is to find them missing.
    pub fn restore(
        &mut self,
        candidate: &Candidate,
        manifest: &Manifest,
        roots: &Roots,
        how: Placing<'_>,
        damaged: &mut bool,
    ) -> Result<Option<Restored>> {
        let Placing {
            tree,
            leave,
            time,
            remap_out,
        } = how;
        let left = |i: usize| leave.is_some_and(|leave| leave[i]);
        let begun = std::time::Instant::now();
        let mut dests = Vec::with_capacity(manifest.outputs.len());
        if let Some(dest) = remap_out
            && manifest.outputs.len() == 1
        {
            dests.push(dest.to_owned());
        } else {
            for output in &manifest.outputs {
                match roots.join(output.root, &output.path) {
                    Some(dest) => dests.push(dest),
                    None => return Ok(None),
                }
            }
        }
        let mut todo = Vec::with_capacity(dests.len());
        for (i, (output, dest)) in manifest.outputs.iter().zip(&dests).enumerate() {
            if !left(i) && (!tree || !self.holds(output, dest, roots)) {
                todo.push(i);
            }
        }
        // Find every object before writing anything: a result that is not
        // all there is found out before any of it is copied. Nothing is in
        // the target until all of it is staged, so one evicted meanwhile
        // leaves the target untouched too.
        let mut sources = Vec::with_capacity(todo.len());
        for &i in &todo {
            let digest = &manifest.outputs[i].digest;
            let plain = object_path(&self.dir, digest, false);
            let found = match fs::symlink_metadata(&plain) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    let packed = object_path(&self.dir, digest, true);
                    fs::symlink_metadata(&packed).map(|_| (packed, true))
                }
                other => other.map(|_| (plain, false)),
            };
            match found {
                Ok((source, packed)) => sources.push(Work {
                    at: i,
                    output: &manifest.outputs[i],
                    dest: &dests[i],
                    source,
                    packed,
                }),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    *damaged = true;
                    return Ok(None);
                }
                Err(e) => return Err(e.into()),
            }
        }
        let detail: Option<(Vec<u8>, u64)> = self
            .conn
            .prepare_cached("SELECT detail,cost_ms FROM entries WHERE key=?1")?
            .query_row([&candidate.key], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        let Some((detail, cost_ms)) = detail else {
            return Ok(None);
        };
        if blake3::hash(&detail).as_bytes() != &manifest.detail {
            *damaged = true;
            return Ok(None);
        }
        // For the test of a source edited while a build is put back:
        // say that the sources have been looked at, and wait to be told
        // to go on.
        if tree && let Some(hold) = env::var_os("JATO_TEST_HOLD") {
            let hold = PathBuf::from(hold);
            let _ = fs::write(hold.with_extension("waiting"), "");
            for _ in 0..4000 {
                if hold.exists() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        // Every file of a whole build gets one time, as if the build had
        // just ended before anyone looked at its sources: no source is
        // newer, and one edited from then on will be.
        let time = if tree { time } else { None };
        let how = How {
            verify: self.verify,
            clone: self.clone,
            roots,
            time,
        };
        let bytes: u64 = sources.iter().map(|w| w.output.size).sum();
        // Each piece of work leaves a staged file beside its destination,
        // made by as many threads as there are processors, up to sixteen.
        // Each mostly unpacks or copies, and all of them ask the kernel
        // for memory to write into: with the page cache full, 32 threads
        // took four times the kernel's time of eight to put 21 GB in
        // place, and were done a tenth sooner than sixteen.
        let most = std::thread::available_parallelism().map_or(8, |n| n.get());
        let threads = if tree {
            most.clamp(1, 16).min(sources.len() / 16 + 1)
        } else {
            1
        };
        let looked = begun.elapsed();
        // The largest first, each thread taking the next when it is done
        // with one: a build has a few files of hundreds of megabytes and
        // thousands of a few kilobytes.
        sources.sort_by_key(|work| std::cmp::Reverse(work.output.size));
        let queue = std::sync::Mutex::new(sources.into_iter());
        // A thread's staged files with which outputs they are, and how it
        // went.
        type Lane = (Vec<(PathBuf, usize)>, Result<Option<bool>>);
        fn run(
            queue: &std::sync::Mutex<std::vec::IntoIter<Work<'_>>>,
            failed: &std::sync::atomic::AtomicBool,
            how: &How<'_>,
        ) -> Lane {
            use std::sync::atomic::Ordering;
            let mut staged = Vec::new();
            loop {
                if failed.load(Ordering::Relaxed) {
                    return (staged, Ok(Some(true)));
                }
                let next = queue.lock().unwrap_or_else(|e| e.into_inner()).next();
                let Some(work) = next else {
                    return (staged, Ok(Some(true)));
                };
                let at = work.at;
                let (staging, placed) = place(work, how);
                staged.extend(staging.map(|s| (s, at)));
                if !matches!(placed, Ok(Some(true))) {
                    failed.store(true, Ordering::Relaxed);
                    return (staged, placed);
                }
            }
        }
        let failed = std::sync::atomic::AtomicBool::new(false);
        let results: Vec<Lane> = if threads == 1 {
            vec![run(&queue, &failed, &how)]
        } else {
            std::thread::scope(|scope| {
                let handles: Vec<_> = (0..threads)
                    .map(|_| scope.spawn(|| run(&queue, &failed, &how)))
                    .collect();
                handles
                    .into_iter()
                    .map(|h| {
                        h.join().unwrap_or_else(|_| {
                            (Vec::new(), Err(anyhow::anyhow!("a restore failed")))
                        })
                    })
                    .collect()
            })
        };
        let written = begun.elapsed();
        let mut staged: Vec<(PathBuf, usize)> = Vec::new();
        let mut verdict = Ok(Some(true));
        for (files, result) in results {
            staged.extend(files);
            if !matches!(result, Ok(Some(true))) {
                verdict = result;
            }
        }
        let give_up = |staged: &[(PathBuf, usize)]| {
            for (staging, _) in staged {
                let _ = fs::remove_file(staging);
            }
        };
        if !matches!(verdict, Ok(Some(true))) {
            give_up(&staged);
            *damaged |= matches!(verdict, Ok(Some(false)));
            return verdict.map(|_| None);
        }
        for (i, dest) in dests.iter().enumerate() {
            // What is left out is taken out of the target directory, for
            // Cargo to find missing. A file anywhere else is the person's.
            if left(i)
                && manifest.outputs[i].root == crate::roots::TARGET
                && let Err(e) = fs::remove_file(dest)
                && e.kind() != io::ErrorKind::NotFound
            {
                give_up(&staged);
                return Err(e.into());
            }
        }
        // Each staged file goes to its place, and is known from then on
        // by what the filesystem says of it there.
        type Known = Vec<(usize, Option<Stamp>)>;
        let moved = |part: &[(PathBuf, usize)]| -> io::Result<Known> {
            let mut known = Vec::with_capacity(part.len());
            for (staging, at) in part {
                fs::rename(staging, &dests[*at])?;
                let stamp = fs::metadata(&dests[*at]).ok().map(|meta| Stamp::of(&meta));
                known.push((*at, stamp));
            }
            Ok(known)
        };
        let threads = most.clamp(1, 16).min(staged.len() / 256 + 1);
        let moves: Vec<io::Result<Known>> = if threads == 1 {
            vec![moved(&staged)]
        } else {
            let each = staged.len().div_ceil(threads);
            std::thread::scope(|scope| {
                let handles: Vec<_> = staged
                    .chunks(each)
                    .map(|part| scope.spawn(|| moved(part)))
                    .collect();
                handles
                    .into_iter()
                    .map(|h| {
                        h.join()
                            .unwrap_or_else(|_| Err(io::Error::other("a restore failed")))
                    })
                    .collect()
            })
        };
        let mut known: Known = Vec::with_capacity(staged.len());
        let mut failure = None;
        for part in moves {
            match part {
                Ok(part) => known.extend(part),
                Err(e) => failure = Some(e),
            }
        }
        if let Some(e) = failure {
            // Whatever was not moved is still beside its place.
            give_up(&staged);
            return Err(e.into());
        }
        if tree && !todo.is_empty() {
            // Without a time taken before the sources were looked at, the
            // files get the one the filesystem has now.
            let late = match time {
                Some(_) => None,
                None => just_before_now(&dests[todo[0]]),
            };
            for (i, dest) in dests.iter().enumerate() {
                // What was written just now has the time already.
                let written = todo.binary_search(&i).is_ok();
                let Some(stamp) = late.or(time.filter(|_| !written)) else {
                    continue;
                };
                // A file outside the target directory that already held
                // what it should is one of the sources as far as Cargo
                // can tell, a lock file the recorded build wrote for one.
                // When it last changed is part of what Cargo remembers of
                // the package, and it did not change now.
                let source = manifest.outputs[i].root != crate::roots::TARGET && !written;
                if !source
                    && !left(i)
                    && let Ok(name) = std::ffi::CString::new(dest.as_os_str().as_bytes())
                {
                    let times = [
                        libc::timespec {
                            tv_sec: 0,
                            tv_nsec: libc::UTIME_OMIT,
                        },
                        stamp,
                    ];
                    unsafe { libc::utimensat(libc::AT_FDCWD, name.as_ptr(), times.as_ptr(), 0) };
                    // Later commands read this file. They need not hash
                    // it again.
                    if let Ok(meta) = fs::metadata(dest) {
                        self.remember(Stamp::of(&meta), manifest.outputs[i].digest);
                    }
                }
            }
            if late.is_some() {
                known.clear();
            }
        }
        for (at, stamp) in known {
            if let Some(stamp) = stamp {
                self.remember(stamp, manifest.outputs[at].digest);
            }
        }
        // Which results were used lately decides what is evicted. An hour
        // is fine enough, and saves a write for every reuse in a build.
        if now() - candidate.used > 3_600_000_000 {
            self.used.push(candidate.key);
        }
        Ok(Some(Restored {
            detail,
            cost_ms,
            bytes,
            files: todo.len(),
            took: [looked, written - looked, begun.elapsed() - written],
        }))
    }

    /// Put files a compiler saved for its next run where it looks for
    /// them. All of them or none: false if any could not be had.
    pub fn place_saved(&mut self, saved: &[crate::manifest::Output], roots: &Roots) -> bool {
        let how = How {
            verify: self.verify,
            clone: self.clone,
            roots,
            time: None,
        };
        let mut written: Vec<PathBuf> = Vec::with_capacity(saved.len());
        let placed = saved.iter().all(|file| {
            let Some(dest) = roots.join(file.root, &file.path) else {
                return false;
            };
            let plain = object_path(&self.dir, &file.digest, false);
            let (source, packed) = match fs::symlink_metadata(&plain) {
                Ok(_) => (plain, false),
                Err(_) => (object_path(&self.dir, &file.digest, true), true),
            };
            let work = Work {
                at: 0,
                output: file,
                dest: &dest,
                source,
                packed,
            };
            match place(work, &how) {
                (Some(staging), Ok(Some(true))) if fs::rename(&staging, &dest).is_ok() => {
                    written.push(dest);
                    true
                }
                (staging, _) => {
                    if let Some(staging) = staging {
                        let _ = fs::remove_file(staging);
                    }
                    false
                }
            }
        });
        if !placed {
            for path in written {
                let _ = fs::remove_file(path);
            }
        }
        placed
    }

    /// Record a whole build in the cross-command index so other commands
    /// sharing this toolchain/profile can seed an empty target directory.
    pub fn note_whole_build(
        &mut self,
        recipe: &Digest,
        key: &Digest,
        outputs: usize,
        env_tag: &Digest,
        kind: u8,
    ) {
        let _ = self.conn.execute(
            "INSERT OR REPLACE INTO builds(recipe,key,used,outputs,env_tag,kind)
             VALUES(?1,?2,?3,?4,?5,?6)",
            params![recipe, key, now(), outputs as i64, env_tag, i64::from(kind)],
        );
    }

    /// Recent whole builds recorded under the same toolchain/profile bucket
    /// as `env_tag`, excluding `exclude_recipe`.
    pub fn whole_builds(
        &self,
        env_tag: &Digest,
        exclude_recipe: &Digest,
    ) -> Result<Vec<(Digest, Candidate, u8, usize)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT b.recipe, b.key, e.used, b.kind, b.outputs
             FROM builds b
             INNER JOIN entries e ON e.key = b.key
             WHERE b.env_tag=?1 AND b.recipe!=?2
             ORDER BY b.used DESC LIMIT 16",
        )?;
        let rows = stmt.query_map(params![env_tag, exclude_recipe], |r| {
            Ok((
                r.get::<_, Digest>(0)?,
                Candidate {
                    key: r.get(1)?,
                    used: r.get(2)?,
                },
                r.get::<_, i64>(3)? as u8,
                r.get::<_, i64>(4)?.max(0) as usize,
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The record with this key, if it is kept.
    pub fn entry(&self, key: &Digest) -> Result<Option<Candidate>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT key,used FROM entries WHERE key=?1")?;
        let found = stmt.query_row([key], |r| {
            Ok(Candidate {
                key: r.get(0)?,
                used: r.get(1)?,
            })
        });
        Ok(found.optional()?)
    }

    /// Whether the file at `dest` already is this output.
    pub fn holds(&mut self, output: &crate::manifest::Output, dest: &Path, roots: &Roots) -> bool {
        let Ok(meta) = fs::symlink_metadata(dest) else {
            return false;
        };
        if !meta.is_file() || meta.permissions().mode() & 0o777 != output.mode & 0o777 {
            return false;
        }
        match output.encoding {
            Encoding::Text => {
                // Text is stored with marks for the roots. Compare it that way.
                return meta.len() <= TEXT_LIMIT * 4
                    && fs::read(dest).is_ok_and(|text| {
                        blake3::hash(&roots.mark(&text)).as_bytes() == &output.digest
                    });
            }
            Encoding::CargoDep => {
                return meta.len() <= TEXT_LIMIT * 4
                    && fs::read(dest).is_ok_and(|bytes| {
                        crate::roots::mark_cargo_dep(&bytes, roots).is_some_and(|marked| {
                            blake3::hash(&marked).as_bytes() == &output.digest
                        })
                    });
            }
            Encoding::Raw => (),
        }
        meta.len() == output.size
            && self
                .digest_known(dest, Stamp::of(&meta))
                .is_ok_and(|digest| digest == output.digest)
    }

    /// Count a record as used now, when the index is next written.
    pub fn touch(&mut self, key: &Digest) {
        self.used.push(*key);
    }

    /// Drop a result whose objects turned out to be missing or damaged.
    pub fn forget(&mut self, key: &Digest) {
        self.count("damaged", 1);
        self.retire(key);
    }

    /// Take a record out of the cache, with the files nothing else uses.
    pub fn retire(&mut self, key: &Digest) {
        let dir = self.dir.clone();
        let _ = (|| -> Result<()> {
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let id: Option<i64> = tx
                .query_row("SELECT id FROM entries WHERE key=?1", [key], |r| r.get(0))
                .optional()?;
            if let Some(id) = id {
                remove_entry(&tx, &dir, id)?;
            }
            tx.commit()?;
            Ok(())
        })();
    }

    // ---- publication ------------------------------------------------------

    /// Copy one output into the cache directory and learn its digest and
    /// which of `roots`' spellings its bytes name.
    pub fn stage(&mut self, file: &Path, roots: &Roots) -> Result<Staged> {
        let hot = self.use_hot_tier();
        let (staged, learned) = stage_file(&self.dir, self.clone, hot, file, roots)?;
        if let Some((stamp, digest)) = learned {
            self.remember(stamp, digest);
        }
        Ok(staged)
    }

    /// The same for many files at once, on as many threads as there are
    /// processors: each file is read through, and a build's programs are
    /// gigabytes.
    pub fn stage_many(&mut self, files: &[&Path], roots: &Roots) -> Vec<Result<Staged>> {
        type Done = Result<(Staged, Option<(Stamp, Digest)>)>;
        let most = std::thread::available_parallelism().map_or(4, |n| n.get());
        let threads = most.clamp(1, 16).min(files.len() / 4 + 1);
        let (dir, clone, hot) = (self.dir.clone(), self.clone, self.use_hot_tier());
        let next = std::sync::atomic::AtomicUsize::new(0);
        let work = || -> Vec<(usize, Done)> {
            let mut done = Vec::new();
            loop {
                let at = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(file) = files.get(at) else {
                    return done;
                };
                done.push((at, stage_file(&dir, clone, hot, file, roots)));
            }
        };
        let mut all: Vec<(usize, Done)> = if threads == 1 {
            work()
        } else {
            std::thread::scope(|scope| {
                let handles: Vec<_> = (0..threads).map(|_| scope.spawn(work)).collect();
                handles
                    .into_iter()
                    .flat_map(|h| h.join().unwrap_or_default())
                    .collect()
            })
        };
        all.sort_by_key(|(at, _)| *at);
        let mut staged: Vec<Result<Staged>> = Vec::with_capacity(files.len());
        for (at, done) in all {
            // A thread that failed outright left its files unread.
            while staged.len() < at {
                staged.push(Err(anyhow::anyhow!("a file could not be stored")));
            }
            staged.push(done.map(|(staged, learned)| {
                if let Some((stamp, digest)) = learned {
                    self.remember(stamp, digest);
                }
                staged
            }));
        }
        while staged.len() < files.len() {
            staged.push(Err(anyhow::anyhow!("a file could not be stored")));
        }
        staged
    }

    /// Make a finished command part of the cache. False if it does not fit.
    pub fn commit(
        &mut self,
        recipe: &Digest,
        record: &Manifest,
        detail: &[u8],
        cost_ms: u64,
        staged: &mut [Staged],
    ) -> Result<bool> {
        let key = record.key(recipe);
        let manifest = record.encode()?;
        let limit = self.budget()?;
        // Count only bytes not already kept in `objects/`: a whole build
        // whose compilers already stored their outputs adds only its
        // manifest and Cargo's small bookkeeping files.
        let new_bytes: u64 = staged
            .iter()
            .filter(|s| s.temp.is_some())
            .map(|s| s.stored)
            .sum();
        if new_bytes + (manifest.len() + detail.len()) as u64 > limit / 2 {
            return Ok(false);
        }
        let dir = self.dir.clone();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let added = tx.execute(
            "INSERT OR IGNORE INTO entries(key,recipe,used,cost_ms,manifest,detail)
             VALUES(?1,?2,?3,?4,?5,?6)",
            params![key, recipe, now(), cost_ms, manifest, detail],
        )?;
        if added == 0 {
            tx.execute(
                "UPDATE entries SET used=?1 WHERE key=?2",
                params![now(), key],
            )?;
            tx.commit()?;
            return Ok(true);
        }
        let entry = tx.last_insert_rowid();
        let mut charge = ENTRY_OVERHEAD + (manifest.len() + detail.len()) as u64;
        for output in staged.iter_mut() {
            let known: bool = tx
                .prepare_cached("SELECT EXISTS(SELECT 1 FROM objects WHERE digest=?1)")?
                .query_row([&output.digest], |r| r.get(0))?;
            if !known {
                let path = object_path(&dir, &output.digest, output.packed);
                match output.temp.take() {
                    Some(temp) => {
                        let moved = fs::create_dir(path.parent().unwrap())
                            .or_else(|e| match e.kind() {
                                io::ErrorKind::AlreadyExists => Ok(()),
                                _ => Err(e),
                            })
                            .and_then(|()| fs::rename(&temp, &path));
                        if let Err(e) = moved {
                            let _ = fs::remove_file(&temp);
                            return Err(e.into());
                        }
                    }
                    // The file was found in place when the output was read.
                    None => ensure!(path.is_file(), "a stored output went away"),
                }
                tx.execute(
                    "INSERT INTO objects VALUES(?1,?2,?3,?4)",
                    params![output.digest, output.size, output.stored, output.packed],
                )?;
                charge += output.stored;
            }
        }
        // Outputs that were already stored by another result are shared
        // with it, and must still be there.
        for output in record.outputs.iter().chain(&record.saved) {
            let known: bool = tx
                .prepare_cached("SELECT EXISTS(SELECT 1 FROM objects WHERE digest=?1)")?
                .query_row([&output.digest], |r| r.get(0))?;
            ensure!(known, "an output was evicted while it was being recorded");
            tx.prepare_cached("INSERT OR IGNORE INTO outputs VALUES(?1,?2)")?
                .execute(params![entry, output.digest])?;
        }
        add(&tx, "bytes", charge as i64)?;
        // Keep a bounded history for each command line.
        let surplus: Vec<i64> = tx
            .prepare_cached(
                "SELECT id FROM entries WHERE recipe=?1 ORDER BY used DESC LIMIT -1 OFFSET ?2",
            )?
            .query_map(params![recipe, VARIANTS as i64], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for id in surplus {
            remove_entry(&tx, &dir, id)?;
        }
        evict(&tx, &dir, limit, limit - limit / 10)?;
        tx.commit()?;
        Ok(true)
    }

    // ---- bookkeeping ------------------------------------------------------

    /// Write what this process learned in one transaction.
    /// Move what the write-ahead log holds into the index, so that the
    /// next process to open it has nothing to read back. Worth doing after
    /// a whole build's worth of writes; skipped if anyone else is writing.
    pub fn settle(&mut self) {
        self.flush();
        let _ = self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
    }

    pub fn flush(&mut self) {
        if self.learned.is_empty()
            && self.counts.is_empty()
            && self.used.is_empty()
            && self.noted.is_empty()
        {
            return;
        }
        let learned = std::mem::take(&mut self.learned);
        let counts = std::mem::take(&mut self.counts);
        let used = std::mem::take(&mut self.used);
        let noted = std::mem::take(&mut self.noted);
        let _ = (|| -> Result<()> {
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let time = now();
            {
                let mut stmt = tx.prepare_cached(
                    "INSERT INTO stamps VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(dev,ino) DO UPDATE SET
                     size=excluded.size,mtime=excluded.mtime,ctime=excluded.ctime,
                     digest=excluded.digest,seen=excluded.seen",
                )?;
                for (stamp, digest) in &learned {
                    stmt.execute(params![
                        stamp.dev as i64,
                        stamp.ino as i64,
                        stamp.size as i64,
                        stamp.mtime,
                        stamp.ctime,
                        digest,
                        time
                    ])?;
                }
            }
            for key in &used {
                tx.execute(
                    "UPDATE entries SET used=?1 WHERE key=?2",
                    params![time, key],
                )?;
            }
            for key in &noted {
                tx.execute(
                    "INSERT OR REPLACE INTO ran VALUES(?1,?2)",
                    params![key, time],
                )?;
            }
            for (name, value) in &counts {
                add(&tx, name, *value)?;
            }
            tx.commit()?;
            Ok(())
        })();
    }

    /// Evict down to `requested` bytes, or to the budget, and clear out
    /// whatever interrupted commands and earlier versions left behind.
    pub fn prune(&mut self, requested: Option<u64>) -> Result<(u64, u64)> {
        let limit = match requested {
            Some(limit) => limit,
            None => self.budget()?,
        };
        let dir = self.dir.clone();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let before: u64 = tx.query_row("SELECT count(*) FROM entries", [], |r| r.get(0))?;
        evict(&tx, &dir, limit, limit)?;
        tx.execute(
            "DELETE FROM stamps WHERE seen < ?1",
            [now() - 45 * 86_400_000_000i64],
        )?;
        tx.execute(
            "DELETE FROM ran WHERE at < ?1",
            [now() - 45 * 86_400_000_000i64],
        )?;
        tx.execute(
            "DELETE FROM builds WHERE key NOT IN (SELECT key FROM entries)",
            [],
        )?;
        let after: u64 = tx.query_row("SELECT count(*) FROM entries", [], |r| r.get(0))?;
        sweep_strays(&tx, &dir)?;
        tx.commit()?;
        let _ = self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        Ok((before - after, remove_earlier_formats(&dir)))
    }

    pub fn stats(&self) -> Result<Stats> {
        let mut counters = BTreeMap::new();
        let mut stmt = self.conn.prepare("SELECT name,value FROM counters")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
            let (name, value) = row?;
            counters.insert(name, value.max(0) as u64);
        }
        let one = |sql: &str| -> Result<u64> {
            Ok(self
                .conn
                .query_row(sql, [], |r| r.get::<_, Option<i64>>(0))?
                .unwrap_or(0)
                .max(0) as u64)
        };
        let database_bytes = ["index-v2.sqlite3", "index-v2.sqlite3-wal"]
            .iter()
            .filter_map(|n| fs::metadata(self.dir.join(n)).ok())
            .map(|m| m.len())
            .sum();
        Ok(Stats {
            directory: self.dir.clone(),
            limit_bytes: self.budget()?,
            memory_bytes: self.memory_budget()?,
            hot_bytes: one("SELECT sum(stored) FROM objects WHERE packed=0")?,
            stored_bytes: counters.remove("bytes").unwrap_or(0),
            artifact_bytes: one("SELECT sum(size) FROM objects")?,
            database_bytes,
            entries: one("SELECT count(*) FROM entries")?,
            objects: one("SELECT count(*) FROM objects")?,
            remembered_files: one("SELECT count(*) FROM stamps")?,
            counters,
            unrecorded: self.noted("unrecorded"),
            only_there: self.noted("only_there"),
            clones: self.clone && clones(&self.dir.join("objects")),
        })
    }

    /// What was last noted under `name`, and how many seconds ago.
    fn noted(&self, name: &str) -> Option<(u64, String)> {
        let noted: String = self
            .conn
            .query_row("SELECT value FROM settings WHERE name=?1", [name], |r| {
                r.get(0)
            })
            .optional()
            .unwrap_or(None)?;
        let (at, why) = noted.split_once('\t')?;
        let ago = (now() / 1_000_000 - at.parse::<i64>().ok()?).max(0);
        Some((ago as u64, why.to_owned()))
    }

    /// Note `why` under `name` with the time, or forget what was noted.
    fn note(&mut self, name: &str, why: Option<&str>) {
        let _ = match why {
            Some(why) => self.conn.execute(
                "INSERT INTO settings VALUES(?1,?2) ON CONFLICT(name) DO UPDATE SET value=excluded.value",
                params![name, format!("{}\t{why}", now() / 1_000_000)],
            ),
            None => self
                .conn
                .execute("DELETE FROM settings WHERE name=?1", [name]),
        };
    }

    /// Keep why a whole build that was recorded is only put back where it
    /// was made, or forget the last such reason once one is not.
    pub fn note_only_there(&mut self, why: Option<&str>) {
        self.note("only_there", why);
    }

    /// Keep why a build could not be recorded as a whole, or forget the
    /// last such reason once one has been.
    pub fn note_unrecorded(&mut self, why: Option<&str>) {
        self.note("unrecorded", why);
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        self.flush();
    }
}

fn add(tx: &Transaction<'_>, name: &str, value: i64) -> Result<()> {
    tx.prepare_cached(
        "INSERT INTO counters VALUES(?1,?2) ON CONFLICT(name) DO UPDATE SET value=value+excluded.value",
    )?
    .execute(params![name, value])?;
    Ok(())
}

/// Remove an entry, and every object only it referred to.
fn remove_entry(tx: &Transaction<'_>, dir: &Path, id: i64) -> Result<()> {
    let objects: Vec<Digest> = tx
        .prepare_cached("SELECT DISTINCT object FROM outputs WHERE entry=?1")?
        .query_map([id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let size: Option<i64> = tx
        .prepare_cached(
            "DELETE FROM entries WHERE id=?1 RETURNING length(manifest)+length(detail)",
        )?
        .query_row([id], |r| r.get(0))
        .optional()?;
    let Some(size) = size else {
        return Ok(());
    };
    let mut freed = ENTRY_OVERHEAD as i64 + size;
    for digest in objects {
        let gone: Option<(i64, bool)> = tx
            .prepare_cached(
                "DELETE FROM objects WHERE digest=?1
                 AND NOT EXISTS(SELECT 1 FROM outputs WHERE object=?1) RETURNING stored,packed",
            )?
            .query_row([&digest], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?;
        if let Some((stored, packed)) = gone {
            let _ = fs::remove_file(object_path(dir, &digest, packed));
            freed += stored;
        }
    }
    add(tx, "bytes", -freed)
}

fn usage(tx: &Transaction<'_>) -> Result<u64> {
    Ok(tx
        .query_row("SELECT value FROM counters WHERE name='bytes'", [], |r| {
            r.get::<_, i64>(0)
        })
        .optional()?
        .unwrap_or(0)
        .max(0) as u64)
}

/// When more than `limit` is stored, remove the least valuable older entries
/// until no more than `target` is. Entries used within the last minute follow
/// strict recency; older entries receive up to one hour of protection scaled
/// by the build time they saved (`cost_ms`).
fn evict(tx: &Transaction<'_>, dir: &Path, limit: u64, target: u64) -> Result<()> {
    let mut used = usage(tx)?;
    if used <= limit {
        return Ok(());
    }
    let mut evicted = 0i64;
    let cutoff = now() - 60_000_000;
    while used > target {
        let batch: Vec<i64> = tx
            .prepare_cached(
                "SELECT id FROM entries
                 ORDER BY CASE WHEN used >= ?1 THEN used
                               ELSE used + MIN(cost_ms * 10000, 3600000000) END
                 LIMIT 32",
            )?
            .query_map([cutoff], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        if batch.is_empty() {
            break;
        }
        for id in batch {
            remove_entry(tx, dir, id)?;
            evicted += 1;
            used = usage(tx)?;
            if used <= target {
                break;
            }
        }
    }
    if evicted > 0 {
        add(tx, "evicted", evicted)?;
    }
    Ok(())
}

/// Remove what interrupted commands left: object files the index does not
/// know, and old lock files.
fn sweep_strays(tx: &Transaction<'_>, dir: &Path) -> Result<()> {
    let old = |entry: &fs::DirEntry, age: u64| {
        entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| t < SystemTime::now() - Duration::from_secs(age))
    };
    for lock in fs::read_dir(dir.join("locks"))
        .into_iter()
        .flatten()
        .flatten()
    {
        if old(&lock, 86_400) {
            // What an interrupted look at the compiler left behind is a
            // directory; a lock is a file, and so are the places in `jobs`,
            // which stay.
            if lock.file_name().to_string_lossy().starts_with("probe-") {
                let _ = fs::remove_dir_all(lock.path());
            } else {
                let _ = fs::remove_file(lock.path());
            }
        }
    }
    for shard in fs::read_dir(dir.join("objects"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = shard.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("tmp-") {
            if old(&shard, 3600) {
                let _ = fs::remove_file(shard.path());
            }
            continue;
        }
        for file in fs::read_dir(shard.path()).into_iter().flatten().flatten() {
            let rest = file.file_name();
            let rest = rest.to_string_lossy();
            let text = format!("{name}{}", rest.trim_end_matches(".zst"));
            let mut digest = [0u8; 32];
            let known = text.len() == 64
                && text.is_ascii()
                && (0..32).all(|i| {
                    u8::from_str_radix(&text[2 * i..2 * i + 2], 16)
                        .map(|b| digest[i] = b)
                        .is_ok()
                })
                && tx
                    .prepare_cached("SELECT EXISTS(SELECT 1 FROM objects WHERE digest=?1)")?
                    .query_row([&digest], |r| r.get::<_, bool>(0))?;
            if !known {
                let _ = fs::remove_file(file.path());
            }
        }
    }
    Ok(())
}

/// Remove the files of cache formats before this one, which nothing can
/// read any more. Returns the bytes they held.
fn remove_earlier_formats(dir: &Path) -> u64 {
    fn size(path: &Path) -> u64 {
        match fs::symlink_metadata(path) {
            Ok(m) if m.is_dir() => fs::read_dir(path)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| size(&e.path()))
                .sum(),
            Ok(m) => m.len(),
            Err(_) => 0,
        }
    }
    let mut freed = 0;
    for name in [
        "artifacts-v1.sqlite3",
        "artifacts-v1.sqlite3-wal",
        "artifacts-v1.sqlite3-shm",
        "schema.lock",
    ] {
        freed += size(&dir.join(name));
        let _ = fs::remove_file(dir.join(name));
    }
    for name in ["objects-v1", "frames-v1", ".jato-staging-v1"] {
        freed += size(&dir.join(name));
        let _ = fs::remove_dir_all(dir.join(name));
    }
    freed
}
