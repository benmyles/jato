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
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Zstd compression level for packed cache objects on non-cloning filesystems.
/// Negative levels (--fast=5) compress ~40% faster and decompress ~33% faster
/// than level 1 while still shrinking debug build artifacts by ~2.5-3x.
const PACK_LEVEL: i32 = -5;
/// Compressed objects at or below this uncompressed size are decompressed in
/// one bulk call instead of streaming frame by frame.
const BULK_DECOMPRESS_LIMIT: u64 = 4 << 20;

type InflightGate = Arc<(Mutex<bool>, Condvar)>;

/// Process-wide in-memory stamp cache shared across pooled `Store` instances
/// inside one supervisor session, with per-inode singleflight deduplication so
/// dozens of concurrent crates never redundantly hash the same newly-built
/// large `.rlib` or binary.
#[derive(Default)]
pub struct SharedStamps {
    map: RwLock<HashMap<(u64, u64), (Stamp, Digest)>>,
    inflight: Mutex<HashMap<(u64, u64), InflightGate>>,
}

impl SharedStamps {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, stamp: &Stamp) -> Option<Digest> {
        let guard = self.map.read().unwrap_or_else(|e| e.into_inner());
        guard
            .get(&(stamp.dev, stamp.ino))
            .filter(|(k, _)| k == stamp)
            .map(|(_, d)| *d)
    }

    pub fn get_inode(&self, dev: u64, ino: u64) -> Option<(Stamp, Digest)> {
        let guard = self.map.read().unwrap_or_else(|e| e.into_inner());
        guard.get(&(dev, ino)).copied()
    }

    pub fn put(&self, stamp: Stamp, digest: Digest) {
        let mut guard = self.map.write().unwrap_or_else(|e| e.into_inner());
        guard.insert((stamp.dev, stamp.ino), (stamp, digest));
    }

    /// Return a cached digest for `stamp`, or serialize concurrent hashers of
    /// the same large inode so only one thread computes it while peers wait.
    pub fn get_or_hash(
        &self,
        stamp: &Stamp,
        compute: impl FnOnce() -> io::Result<CheckDigest>,
    ) -> io::Result<CheckDigest> {
        if let Some(d) = self.get(stamp) {
            return Ok((d, None, 0));
        }
        // Only singleflight files >= 512 KiB (such as `.rlib`s and binaries)
        // where redundant hashing across concurrent workers is expensive.
        if stamp.size < 512 * 1024 {
            let res = compute()?;
            if let Some((s, d)) = res.1 {
                self.put(s, d);
            }
            return Ok(res);
        }
        let key = (stamp.dev, stamp.ino);
        loop {
            let existing = {
                let mut inflight = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(d) = self.get(stamp) {
                    return Ok((d, None, 0));
                }
                if let Some(gate) = inflight.get(&key) {
                    Some(gate.clone())
                } else {
                    let gate = Arc::new((Mutex::new(false), Condvar::new()));
                    inflight.insert(key, gate);
                    None
                }
            };
            match existing {
                None => break,
                Some(gate) => {
                    let mut done = gate.0.lock().unwrap_or_else(|e| e.into_inner());
                    while !*done {
                        done = gate.1.wait(done).unwrap_or_else(|e| e.into_inner());
                    }
                    if let Some(d) = self.get(stamp) {
                        return Ok((d, None, 0));
                    }
                }
            }
        }
        struct Guard<'a> {
            shared: &'a SharedStamps,
            key: (u64, u64),
        }
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                let gate = self
                    .shared
                    .inflight
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&self.key);
                if let Some(gate) = gate {
                    let mut done = gate.0.lock().unwrap_or_else(|e| e.into_inner());
                    *done = true;
                    gate.1.notify_all();
                }
            }
        }
        let _guard = Guard { shared: self, key };
        let res = compute()?;
        if let Some((s, d)) = res.1 {
            self.put(s, d);
        }
        Ok(res)
    }
}

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
    /// Configured remote/shared cache URL, if any.
    pub remote: Option<String>,
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
#[derive(Clone)]
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
    shared: Option<Arc<SharedStamps>>,
    learned: Vec<(Stamp, Digest)>,
    counts: BTreeMap<String, i64>,
    used: Vec<Digest>,
    noted: Vec<Digest>,
    remote: Option<crate::remote::Remote>,
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

pub(crate) fn object_path(dir: &Path, digest: &Digest, packed: bool) -> PathBuf {
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
    /// When restoring directly into a fresh sibling target directory, write
    /// straight here instead of staging beside `dest`.
    direct: Option<PathBuf>,
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
    /// Process umask, to skip redundant `fchmod` on freshly created files.
    umask: u32,
}

/// Reusable per-worker buffers and zstd decoders so restoring thousands of
/// files incurs zero per-file heap/mmap allocations or `ZSTD_DCtx` setups.
struct RestoreScratch {
    in_buf: Vec<u8>,
    out_buf: Vec<u8>,
    bulk: zstd::bulk::Decompressor<'static>,
    stream: zstd::stream::raw::Decoder<'static>,
}

impl RestoreScratch {
    fn new() -> io::Result<Self> {
        Ok(Self {
            in_buf: Vec::with_capacity(256 * 1024),
            out_buf: Vec::with_capacity(256 * 1024),
            bulk: zstd::bulk::Decompressor::new()?,
            stream: zstd::stream::raw::Decoder::new()?,
        })
    }
}

/// Reusable per-worker buffers and zstd encoder for `stage_file` / `stage_many`.
struct StageScratch {
    in_buf: Vec<u8>,
    out_buf: Vec<u8>,
    enc: zstd::stream::raw::Encoder<'static>,
}

impl StageScratch {
    fn new() -> io::Result<Self> {
        Ok(Self {
            in_buf: vec![0u8; 256 * 1024],
            out_buf: vec![0u8; 256 * 1024],
            enc: zstd::stream::raw::Encoder::new(PACK_LEVEL)?,
        })
    }
}

/// Cap parallel file-I/O worker threads so a low `RLIMIT_NOFILE` never causes
/// `EMFILE` when workers open source and destination files concurrently.
fn fd_thread_cap() -> usize {
    let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) } == 0
        && limit.rlim_cur != libc::RLIM_INFINITY
    {
        return (limit.rlim_cur.saturating_sub(20) / 2).max(1) as usize;
    }
    usize::MAX
}

/// Write one output either to a staging file beside its destination or
/// directly into a fresh staging target directory (`work.direct`). Returns
/// `(optional_staging_path, optional_direct_stamp, verdict)`.
fn place(
    work: Work<'_>,
    how: &How<'_>,
    scratch: &mut RestoreScratch,
) -> (Option<PathBuf>, Option<Stamp>, Result<Option<bool>>) {
    use zstd::stream::raw::{InBuffer, Operation, OutBuffer};
    let Work {
        output,
        dest,
        direct,
        source,
        packed,
        ..
    } = work;
    let Some(file_name) = dest.file_name() else {
        return (
            None,
            None,
            Err(anyhow::anyhow!("output without a file name")),
        );
    };
    let mut source = match File::open(&source) {
        Ok(source) => source,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return (None, None, Ok(Some(false))),
        Err(e) => return (None, None, Err(e.into())),
    };
    let is_direct = direct.is_some();
    let target_path = match direct {
        Some(path) => path,
        None => {
            let mut name = file_name.to_owned();
            name.push(format!(".jato-{}", std::process::id()));
            dest.with_file_name(name)
        }
    };
    let want_mode = output.mode & 0o777;
    // Only cloning + verify needs read access back on `out`.
    let need_read = !output.encoding.is_marked() && !packed && how.clone && how.verify;
    let open = |path: &Path| {
        let mut opts = OpenOptions::new();
        opts.write(true).mode(want_mode);
        if need_read {
            opts.read(true);
        }
        if is_direct {
            opts.create_new(true);
        } else {
            opts.create(true).truncate(true);
        }
        opts.open(path)
    };
    let mut out = match open(&target_path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let made =
                fs::create_dir_all(target_path.parent().unwrap()).and_then(|()| open(&target_path));
            match made {
                Ok(f) => f,
                Err(e) => return (None, None, Err(e.into())),
            }
        }
        Err(e) => return (None, None, Err(e.into())),
    };
    let written = (|| -> Result<Option<Option<Stamp>>> {
        if output.encoding.is_marked() {
            ensure!(output.size <= TEXT_LIMIT * 4, "oversized marked object");
            scratch.in_buf.clear();
            source.read_to_end(&mut scratch.in_buf)?;
            if blake3::hash(&scratch.in_buf).as_bytes() != &output.digest {
                return Ok(None);
            }
            let restored = match output.encoding {
                Encoding::Text => how.roots.unmark(&scratch.in_buf),
                Encoding::CargoDep => crate::roots::unmark_cargo_dep(&scratch.in_buf, how.roots)
                    .map(std::borrow::Cow::Owned),
                Encoding::Raw => unreachable!(),
            };
            let Some(restored) = restored else {
                // Use Err sentinel? Handled cleanly below via Result<Option<bool>>.
                return Err(anyhow::anyhow!("__jato_missing_root__"));
            };
            out.write_all(&restored)?;
        } else if packed && output.size <= BULK_DECOMPRESS_LIMIT {
            scratch.in_buf.clear();
            source.read_to_end(&mut scratch.in_buf)?;
            let want = output.size as usize;
            if scratch.out_buf.len() < want {
                scratch.out_buf.resize(want, 0);
            }
            let n = match scratch
                .bulk
                .decompress_to_buffer(&scratch.in_buf, &mut scratch.out_buf[..want])
            {
                Ok(n) => n,
                Err(_) => return Ok(None),
            };
            if n != want || blake3::hash(&scratch.out_buf[..n]).as_bytes() != &output.digest {
                return Ok(None);
            }
            out.write_all(&scratch.out_buf[..n])?;
        } else if packed {
            scratch.stream.reinit()?;
            if scratch.in_buf.len() < 256 * 1024 {
                scratch.in_buf.resize(256 * 1024, 0);
            }
            if scratch.out_buf.len() < 256 * 1024 {
                scratch.out_buf.resize(256 * 1024, 0);
            }
            let mut hasher = blake3::Hasher::new();
            let mut written = 0u64;
            let mut eof = false;
            let mut frame_done = false;
            let mut in_pos = 0usize;
            let mut in_len = 0usize;
            loop {
                if in_pos == in_len && !eof {
                    in_pos = 0;
                    in_len = source.read(&mut scratch.in_buf[..256 * 1024])?;
                    if in_len == 0 {
                        eof = true;
                    } else {
                        frame_done = false;
                    }
                }
                if eof && in_pos == in_len && frame_done {
                    break;
                }
                let mut ib = InBuffer::around(&scratch.in_buf[in_pos..in_len]);
                let mut ob = OutBuffer::around(&mut scratch.out_buf[..256 * 1024]);
                let status = match scratch.stream.run(&mut ib, &mut ob) {
                    Ok(status) => status,
                    Err(_) => return Ok(None),
                };
                in_pos += ib.pos();
                let produced = ob.pos();
                if produced > 0 {
                    let chunk = &scratch.out_buf[..produced];
                    hasher.update(chunk);
                    out.write_all(chunk)?;
                    written += produced as u64;
                }
                frame_done = status == 0;
                if eof && in_pos == in_len && produced == 0 {
                    if !frame_done {
                        return Ok(None);
                    }
                    break;
                }
            }
            if written != output.size || hasher.finalize().as_bytes() != &output.digest {
                return Ok(None);
            }
        } else {
            // An object's length is part of its identity.
            if source.metadata()?.len() != output.size {
                return Ok(None);
            }
            let cloned = how.clone && clone_file(&source, &out).is_ok();
            if cloned {
                if how.verify {
                    use std::io::Seek;
                    out.rewind()?;
                    let (digest, length, _) =
                        scan::digest_open_with(&mut out, &[], &mut scratch.in_buf)?;
                    if length != output.size || digest != output.digest {
                        return Ok(None);
                    }
                }
            } else if how.verify {
                // Single-pass verified copy with reusable buffer.
                if scratch.in_buf.len() < 256 * 1024 {
                    scratch.in_buf.resize(256 * 1024, 0);
                }
                let mut hasher = blake3::Hasher::new();
                let mut written = 0u64;
                loop {
                    let n = source.read(&mut scratch.in_buf[..256 * 1024])?;
                    if n == 0 {
                        break;
                    }
                    let chunk = &scratch.in_buf[..n];
                    hasher.update(chunk);
                    out.write_all(chunk)?;
                    written += n as u64;
                }
                if written != output.size || hasher.finalize().as_bytes() != &output.digest {
                    return Ok(None);
                }
            } else {
                let copied = io::copy(&mut source, &mut out)?;
                ensure!(copied == output.size, "short copy of a cached object");
            }
        }
        // The umask applied at creation. Only call `fchmod` when the umask
        // masked off a bit we need or when overwriting an existing file.
        if !is_direct || (want_mode & how.umask) != 0 {
            out.set_permissions(fs::Permissions::from_mode(want_mode))?;
        }
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
        let stamp = if is_direct {
            out.metadata().ok().map(|m| Stamp::of(&m))
        } else {
            None
        };
        Ok(Some(stamp))
    })();
    let staging = (!is_direct).then_some(target_path);
    match written {
        Ok(Some(stamp)) => (staging, stamp, Ok(Some(true))),
        Ok(None) => (staging, None, Ok(Some(false))),
        Err(e) if e.to_string() == "__jato_missing_root__" => (staging, None, Ok(None)),
        Err(e) => (staging, None, Err(e)),
    }
}

/// Whether `meta` still reflects the same file contents as `before` (taking
/// into account that Cargo hardlinking a finished artifact bumps `ctime` and
/// `nlink` without changing the file's bytes).
fn unchanged_since(before: Stamp, before_nlink: u64, meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    let after = Stamp::of(meta);
    after == before || (before.same_contents(&after) && meta.nlink() != before_nlink)
}

/// Confirm that an open file's contents did not change while it was read.
/// Fast path checks `unchanged_since`; if `ctime` moved with identical
/// `(dev, ino, size, mtime)` and equal `nlink` (e.g., Cargo's artifact uplift
/// doing `link` + `unlink` concurrently), re-hash the open descriptor from
/// offset 0 and prove the bytes match `expected`.
fn verify_unchanged(
    source: &mut File,
    file: &Path,
    before: Stamp,
    before_nlink: u64,
    expected: &Digest,
    buf: &mut Vec<u8>,
) -> Result<Stamp> {
    use std::io::Seek;
    let after_meta = source.metadata()?;
    if unchanged_since(before, before_nlink, &after_meta) {
        return Ok(Stamp::of(&after_meta));
    }
    let after = Stamp::of(&after_meta);
    if before.same_contents(&after) {
        source.rewind()?;
        let (digest2, length2, _) = scan::digest_open_with(source, &[], buf)?;
        if length2 == before.size && &digest2 == expected {
            return Ok(Stamp::of(&source.metadata()?));
        }
    }
    bail!("{} changed while it was stored", file.display())
}

/// Compress `source` to `out` in a single read pass using reusable buffers and
/// `StageScratch`, computing BLAKE3 digest and scanning for `needles`.
fn compress_single_pass(
    source: &mut File,
    out: &mut File,
    needles: &[&[u8]],
    found_names: &mut [bool],
    scratch: &mut StageScratch,
) -> io::Result<(Digest, u64)> {
    use zstd::stream::raw::{InBuffer, Operation, OutBuffer};
    scratch.enc.reinit()?;
    let finders: Vec<_> = needles.iter().map(memchr::memmem::Finder::new).collect();
    let keep = needles.iter().map(|n| n.len() + 1).max().unwrap_or(0);
    let mut tail: Vec<u8> = Vec::new();
    let mut hasher = blake3::Hasher::new();
    let mut length = 0u64;
    let in_cap = scratch.in_buf.len();
    let out_cap = scratch.out_buf.len();
    loop {
        let n = source.read(&mut scratch.in_buf[..in_cap])?;
        if n == 0 {
            break;
        }
        let chunk = &scratch.in_buf[..n];
        hasher.update(chunk);
        length += n as u64;
        if keep > 0 {
            tail.extend_from_slice(chunk);
            for ((finder, needle), found) in finders.iter().zip(needles).zip(found_names.iter_mut())
            {
                let mut from = 0;
                while !*found {
                    let Some(at) = finder.find(&tail[from..]) else {
                        break;
                    };
                    let end = from + at + needle.len();
                    if tail.get(end).is_none_or(|b| {
                        !(b.is_ascii_alphanumeric()
                            || matches!(b, b'.' | b'_' | b'-' | b'+' | b'@' | b'~'))
                    }) {
                        *found = true;
                        break;
                    }
                    from += at + 1;
                }
            }
            let drop = tail.len().saturating_sub(keep);
            tail.drain(..drop);
        }
        let mut pos = 0;
        while pos < n {
            let mut ib = InBuffer::around(&scratch.in_buf[pos..n]);
            let mut ob = OutBuffer::around(&mut scratch.out_buf[..out_cap]);
            scratch.enc.run(&mut ib, &mut ob)?;
            pos += ib.pos();
            let produced = ob.pos();
            if produced > 0 {
                out.write_all(&scratch.out_buf[..produced])?;
            }
        }
    }
    loop {
        let mut ob = OutBuffer::around(&mut scratch.out_buf[..out_cap]);
        let rem = scratch.enc.finish(&mut ob, true)?;
        let produced = ob.pos();
        if produced > 0 {
            out.write_all(&scratch.out_buf[..produced])?;
        }
        if rem == 0 {
            break;
        }
    }
    Ok((*hasher.finalize().as_bytes(), length))
}

/// Copy one output into the cache directory `dir` and learn its digest and
/// which of `roots`' spellings its bytes name; `clone` says whether to try
/// sharing its extents, `hot` says whether to store uncompressed in the hot
/// memory tier when cloning is unavailable, and `one_pass` skips the pre-check
/// read when the file is freshly generated and unlikely to be cached already.
fn stage_file(
    dir: &Path,
    clone: bool,
    hot: bool,
    one_pass: bool,
    file: &Path,
    roots: &Roots,
    scratch: &mut StageScratch,
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
            let final_stamp = if unchanged_since(before, before_nlink, &after_meta) {
                Stamp::of(&after_meta)
            } else if before.same_contents(&Stamp::of(&after_meta)) {
                use std::io::Seek;
                source.rewind()?;
                let mut bytes2 = Vec::with_capacity(bytes.len());
                source.read_to_end(&mut bytes2)?;
                ensure!(
                    bytes2 == bytes,
                    "{} changed while it was stored",
                    file.display()
                );
                Stamp::of(&source.metadata()?)
            } else {
                bail!("{} changed while it was stored", file.display());
            };
            let digest = staged.digest;
            return Ok((staged, Some((final_stamp, digest))));
        }
        use std::io::Seek;
        source.rewind()?;
    }
    let mut out = create()?;
    if clone && clone_file(&source, &out).is_ok() {
        let (digest, length, names) =
            scan::digest_open_with(&mut out, &needles, &mut scratch.in_buf)?;
        ensure!(length == before.size, "{} changed size", file.display());
        staged.digest = digest;
        staged.names = names;
    } else if hot {
        let mut hasher = blake3::Hasher::new();
        let mut tail: Vec<u8> = Vec::new();
        let keep = needles.iter().map(|n| n.len() + 1).max().unwrap_or(0);
        let mut length = 0u64;
        let in_cap = scratch.in_buf.len();
        loop {
            let n = source.read(&mut scratch.in_buf[..in_cap])?;
            if n == 0 {
                break;
            }
            let chunk = &scratch.in_buf[..n];
            hasher.update(chunk);
            out.write_all(chunk)?;
            length += n as u64;
            if keep > 0 {
                tail.extend_from_slice(chunk);
                for (needle, found) in needles.iter().zip(&mut staged.names) {
                    *found = *found || scan::names_path(&tail, needle);
                }
                let drop = tail.len().saturating_sub(keep);
                tail.drain(..drop);
            }
        }
        ensure!(length == before.size, "{} changed size", file.display());
        let digest = *hasher.finalize().as_bytes();
        let final_stamp = verify_unchanged(
            &mut source,
            file,
            before,
            before_nlink,
            &digest,
            &mut scratch.in_buf,
        )?;
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
                return Ok((staged, Some((final_stamp, digest))));
            }
        }
        staged.packed = false;
        staged.stored = before.size;
    } else if one_pass {
        // Fresh unit output or incremental state: compress, hash, and scan in
        // a single read pass; only discard the temp file if already stored.
        let (digest, length) =
            compress_single_pass(&mut source, &mut out, &needles, &mut staged.names, scratch)?;
        ensure!(length == before.size, "{} changed size", file.display());
        let final_stamp = verify_unchanged(
            &mut source,
            file,
            before,
            before_nlink,
            &digest,
            &mut scratch.in_buf,
        )?;
        staged.digest = digest;
        for packed in [true, false] {
            if let Ok(meta) = fs::metadata(object_path(dir, &digest, packed))
                && meta.is_file()
            {
                drop(out);
                let _ = fs::remove_file(&temp);
                staged.temp = None;
                staged.packed = packed;
                staged.stored = meta.len();
                return Ok((staged, Some((final_stamp, digest))));
            }
        }
        staged.packed = true;
        staged.stored = out.metadata()?.len();
    } else {
        // Whole-build adoption: check first whether the cache already holds
        // this file (e.g., stored when a compiler finished).
        use std::io::Seek;
        let (digest, length, names) =
            scan::digest_open_with(&mut source, &needles, &mut scratch.in_buf)?;
        ensure!(length == before.size, "{} changed size", file.display());
        for packed in [true, false] {
            if let Ok(meta) = fs::metadata(object_path(dir, &digest, packed))
                && meta.is_file()
            {
                let final_stamp = verify_unchanged(
                    &mut source,
                    file,
                    before,
                    before_nlink,
                    &digest,
                    &mut scratch.in_buf,
                )?;
                drop(out);
                let _ = fs::remove_file(&temp);
                staged.digest = digest;
                staged.names = names;
                staged.temp = None;
                staged.packed = packed;
                staged.stored = meta.len();
                return Ok((staged, Some((final_stamp, digest))));
            }
        }
        source.rewind()?;
        let (digest2, length2) =
            compress_single_pass(&mut source, &mut out, &needles, &mut staged.names, scratch)?;
        ensure!(length2 == before.size, "{} changed size", file.display());
        ensure!(
            digest == digest2,
            "{} changed while it was stored",
            file.display()
        );
        staged.digest = digest2;
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
        Self::open_at(cache_dir()?)
    }

    pub fn open_at(dir: PathBuf) -> Result<Self> {
        let env_limit = env::var("JATO_MAX_SIZE")
            .ok()
            .map(|s| parse_size(&s))
            .transpose()?;
        let env_memory = env::var("JATO_MEMORY")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(|s| parse_size(&s))
            .transpose()?;
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
        store.remote = store.configured_remote();
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
            shared: None,
            learned: Vec::new(),
            counts: BTreeMap::new(),
            used: Vec::new(),
            noted: Vec::new(),
            remote: None,
        })
    }

    pub fn set_shared_stamps(&mut self, shared: Arc<SharedStamps>) {
        self.shared = Some(shared);
    }

    pub fn shared_stamps(&self) -> Option<&Arc<SharedStamps>> {
        self.shared.as_ref()
    }

    pub fn has_remote(&self) -> bool {
        self.remote.is_some()
    }

    fn configured_remote(&self) -> Option<crate::remote::Remote> {
        if let Ok(val) = env::var("JATO_REMOTE") {
            let trimmed = val.trim();
            if trimmed.is_empty() || trimmed == "0" {
                return None;
            }
            return match crate::remote::Remote::parse(trimmed) {
                Ok(r) => Some(r),
                Err(e) => {
                    crate::command::log("remote", format_args!("ignoring JATO_REMOTE: {e:#}"));
                    None
                }
            };
        }
        let stored = self.remote_url().ok().flatten()?;
        crate::remote::Remote::parse(&stored).ok()
    }

    /// Configured remote cache URL (from `JATO_REMOTE` or stored setting).
    pub fn remote_url(&self) -> Result<Option<String>> {
        if let Ok(val) = env::var("JATO_REMOTE") {
            let trimmed = val.trim();
            if trimmed.is_empty() || trimmed == "0" {
                return Ok(None);
            }
            return Ok(Some(crate::remote::Remote::parse(trimmed)?.url));
        }
        let stored: Option<String> = self
            .conn
            .query_row("SELECT value FROM settings WHERE name='remote'", [], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(stored)
    }

    /// Persist or clear the remote cache URL in the local index.
    pub fn set_remote_url(&mut self, url: Option<&str>) -> Result<Option<String>> {
        match url {
            Some(raw) => {
                let parsed = crate::remote::Remote::parse(raw)?;
                self.conn.execute(
                    "INSERT INTO settings VALUES('remote',?1) ON CONFLICT(name) DO UPDATE SET value=excluded.value",
                    [&parsed.url],
                )?;
                self.remote = Some(parsed.clone());
                Ok(Some(parsed.url))
            }
            None => {
                self.conn
                    .execute("DELETE FROM settings WHERE name='remote'", [])?;
                self.remote = None;
                Ok(None)
            }
        }
    }

    /// Pull a matching entry for `recipe` from the configured remote cache,
    /// cryptographically verifying the manifest, detail, local inputs, and
    /// every downloaded object before committing it into the local store.
    /// Returns `true` if a matching verified entry was adopted locally.
    pub fn pull_remote(
        &mut self,
        recipe: &Digest,
        here: &mut crate::manifest::Here<'_>,
        roots: &Roots,
        check_placed: bool,
    ) -> bool {
        let Some(remote) = self.remote.clone() else {
            return false;
        };
        let keys = match remote.recipe_keys(recipe) {
            Ok(keys) if !keys.is_empty() => keys,
            Ok(_) => return false,
            Err(e) => {
                crate::command::log("remote", format_args!("recipe lookup failed: {e:#}"));
                return false;
            }
        };
        let local_keys: HashSet<Digest> = self
            .candidates(recipe)
            .unwrap_or_default()
            .into_iter()
            .map(|c| c.key)
            .collect();
        let dir = self.dir.clone();
        let prefer_packed = !self.clone && !self.use_hot_tier();
        for key in keys {
            if local_keys.contains(&key) {
                continue;
            }
            let entry = match remote.fetch_entry(&key) {
                Ok(Some(e)) => e,
                Ok(None) => continue,
                Err(e) => {
                    crate::command::log("remote", format_args!("entry fetch failed: {e:#}"));
                    return false;
                }
            };
            let Ok(manifest) = Manifest::decode_verified(&entry.manifest, recipe, &key) else {
                crate::command::log(
                    "remote",
                    "rejected remote entry: manifest verification failed",
                );
                continue;
            };
            if blake3::hash(&entry.detail).as_bytes() != &manifest.detail {
                crate::command::log("remote", "rejected remote entry: detail digest mismatch");
                continue;
            }
            if check_placed && manifest.placed(roots).is_err() {
                continue;
            }
            if manifest.holds(here, self).is_err() {
                continue;
            }
            // Collect unique output/saved objects that are not already present locally.
            let mut unique: Vec<(Digest, bool)> = Vec::new();
            let mut seen: HashSet<Digest> = HashSet::new();
            for out in manifest.outputs.iter().chain(&manifest.saved) {
                if !seen.insert(out.digest) {
                    continue;
                }
                let have_local = self
                    .conn
                    .prepare_cached("SELECT EXISTS(SELECT 1 FROM objects WHERE digest=?1)")
                    .and_then(|mut s| s.query_row([&out.digest], |r| r.get::<_, bool>(0)))
                    .unwrap_or(false)
                    && (object_path(&dir, &out.digest, false).is_file()
                        || object_path(&dir, &out.digest, true).is_file());
                if !have_local {
                    let keep_packed =
                        prefer_packed && out.encoding == Encoding::Raw && out.size >= CLONE_FLOOR;
                    unique.push((out.digest, keep_packed));
                }
            }
            // Fetch & verify missing objects in parallel into staging temp files.
            type FetchedObj = (Digest, PathBuf, u64, u64, bool);
            let fetched: Vec<FetchedObj> = if unique.is_empty() {
                Vec::new()
            } else {
                let threads = std::thread::available_parallelism()
                    .map_or(4, |n| n.get())
                    .clamp(1, 8)
                    .min(unique.len());
                let each = unique.len().div_ceil(threads);
                let failed = std::sync::atomic::AtomicBool::new(false);
                let serial = std::sync::atomic::AtomicU64::new(0);
                let lanes: Vec<Vec<FetchedObj>> = std::thread::scope(|scope| {
                    let handles: Vec<_> = unique
                        .chunks(each)
                        .map(|chunk| {
                            let remote = &remote;
                            let dir = &dir;
                            let failed = &failed;
                            let serial = &serial;
                            scope.spawn(move || {
                                let mut got = Vec::with_capacity(chunk.len());
                                for &(digest, keep_packed) in chunk {
                                    if failed.load(std::sync::atomic::Ordering::Relaxed) {
                                        return got;
                                    }
                                    let id =
                                        serial.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    let tmp = dir
                                        .join("objects")
                                        .join(format!("remote-{}-{id}", std::process::id()));
                                    match remote.fetch_object_to(&digest, &tmp, keep_packed) {
                                        Ok(Some((size, stored, packed))) => {
                                            got.push((digest, tmp, size, stored, packed));
                                        }
                                        Ok(None) => {
                                            let _ = fs::remove_file(&tmp);
                                            failed
                                                .store(true, std::sync::atomic::Ordering::Relaxed);
                                            return got;
                                        }
                                        Err(e) => {
                                            let _ = fs::remove_file(&tmp);
                                            crate::command::log(
                                                "remote",
                                                format_args!("object fetch failed: {e:#}"),
                                            );
                                            failed
                                                .store(true, std::sync::atomic::Ordering::Relaxed);
                                            return got;
                                        }
                                    }
                                }
                                got
                            })
                        })
                        .collect();
                    handles
                        .into_iter()
                        .map(|h| h.join().unwrap_or_default())
                        .collect()
                });
                let all: Vec<_> = lanes.into_iter().flatten().collect();
                if failed.load(std::sync::atomic::Ordering::Relaxed) || all.len() != unique.len() {
                    for (_, tmp, ..) in all {
                        let _ = fs::remove_file(tmp);
                    }
                    continue;
                }
                all
            };
            // Adopt objects + entry transactionally into local SQLite.
            let adopted = self.adopt_remote_entry(
                recipe,
                &key,
                &manifest,
                &entry.manifest,
                &entry.detail,
                entry.cost_ms,
                fetched,
            );
            if adopted.is_ok_and(|ok| ok) {
                self.count("remote_hit", 1);
                return true;
            }
        }
        false
    }

    #[allow(clippy::too_many_arguments)]
    fn adopt_remote_entry(
        &mut self,
        recipe: &Digest,
        key: &Digest,
        record: &Manifest,
        manifest_bytes: &[u8],
        detail: &[u8],
        cost_ms: u64,
        mut fetched: Vec<(Digest, PathBuf, u64, u64, bool)>,
    ) -> Result<bool> {
        let limit = self.budget()?;
        let new_bytes: u64 = fetched.iter().map(|(_, _, _, stored, _)| *stored).sum();
        if new_bytes + (manifest_bytes.len() + detail.len()) as u64 > limit / 2 {
            for (_, tmp, ..) in fetched {
                let _ = fs::remove_file(tmp);
            }
            return Ok(false);
        }
        let dir = self.dir.clone();
        let time = now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let added = tx.execute(
            "INSERT OR IGNORE INTO entries(key,recipe,used,cost_ms,manifest,detail)
             VALUES(?1,?2,?3,?4,?5,?6)",
            params![key, recipe, time, cost_ms, manifest_bytes, detail],
        )?;
        if added == 0 {
            for (_, tmp, ..) in fetched {
                let _ = fs::remove_file(tmp);
            }
            tx.commit()?;
            return Ok(true);
        }
        let entry_id = tx.last_insert_rowid();
        let mut charge = ENTRY_OVERHEAD + (manifest_bytes.len() + detail.len()) as u64;
        for (digest, tmp, size, stored, packed) in fetched.drain(..) {
            let known: bool = tx
                .prepare_cached("SELECT EXISTS(SELECT 1 FROM objects WHERE digest=?1)")?
                .query_row([&digest], |r| r.get(0))?;
            if !known {
                let path = object_path(&dir, &digest, packed);
                let moved = fs::create_dir(path.parent().unwrap())
                    .or_else(|e| match e.kind() {
                        io::ErrorKind::AlreadyExists => Ok(()),
                        _ => Err(e),
                    })
                    .and_then(|()| fs::rename(&tmp, &path));
                if let Err(e) = moved {
                    let _ = fs::remove_file(&tmp);
                    return Err(e.into());
                }
                tx.execute(
                    "INSERT INTO objects VALUES(?1,?2,?3,?4)",
                    params![digest, size, stored, packed],
                )?;
                charge += stored;
            } else {
                let _ = fs::remove_file(&tmp);
            }
        }
        let mut linked: HashSet<Digest> = HashSet::new();
        for output in record.outputs.iter().chain(&record.saved) {
            if !linked.insert(output.digest) {
                continue;
            }
            tx.prepare_cached("INSERT OR IGNORE INTO outputs VALUES(?1,?2)")?
                .execute(params![entry_id, output.digest])?;
        }
        add(&tx, "bytes", charge as i64)?;
        evict(&tx, &dir, limit, limit - limit / 10)?;
        tx.commit()?;
        Ok(true)
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
        if let Ok(count) = self
            .conn
            .query_row("SELECT COUNT(*) FROM stamps", [], |r| r.get::<_, i64>(0))
            && count > 0
        {
            self.known.reserve(count as usize);
        }
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
        if let Some(shared) = &self.shared
            && let Some(digest) = shared.get(stamp)
        {
            self.known.insert((stamp.dev, stamp.ino), (*stamp, digest));
            return Some(digest);
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
        if let Some(shared) = &self.shared {
            shared.put(found, digest);
        }
        (found == *stamp).then_some(digest)
    }

    /// Like `recall`, but also matches when a hardlink bumped `ctime` on the
    /// same inode without changing its size or modification time (e.g. Cargo
    /// hardlinking a freshly compiled binary out of `deps/`).
    pub fn recall_hardlink(&mut self, stamp: &Stamp) -> Option<Digest> {
        let matches = |known: &Stamp| known.same_contents(stamp) && stamp.ctime >= known.ctime;
        if let Some((known, digest)) = self.known.get(&(stamp.dev, stamp.ino)) {
            return matches(known).then_some(*digest);
        }
        if let Some(shared) = &self.shared
            && let Some((known, digest)) = shared.get_inode(stamp.dev, stamp.ino)
        {
            self.known.insert((stamp.dev, stamp.ino), (known, digest));
            return matches(&known).then_some(digest);
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
        if let Some(shared) = &self.shared {
            shared.put(found, digest);
        }
        matches(&found).then_some(digest)
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
        if let Some(shared) = &self.shared {
            shared.put(stamp, digest);
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
        let compute = || {
            let (digest, read, _) = scan::digest_file(path, &[])?;
            if !stamp.same_contents(&read) {
                return Err(io::Error::other("file changed while it was read"));
            }
            Ok((digest, Some((read, digest)), stamp.size))
        };
        let (digest, learned, hashed) = match self.shared.clone() {
            Some(shared) => shared.get_or_hash(&stamp, compute)?,
            None => compute()?,
        };
        if let Some((read, d)) = learned {
            self.remember(read, d);
        }
        if hashed > 0 {
            self.count("hashed_bytes", hashed);
        }
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
            let compute = || -> io::Result<Option<CheckDigest>> {
                let Some((digest, read)) = scan::digest_rlib(path)? else {
                    return Ok(None);
                };
                if !stamp.same_contents(&read) {
                    return Err(io::Error::other("file changed while it was read"));
                }
                let learned = Stamp {
                    dev: read.dev | RLIB_STAMP_BIT,
                    ..read
                };
                Ok(Some((digest, Some((learned, digest)), stamp.size)))
            };
            if let Some(shared) = self.shared.clone() {
                // Singleflight incremental rlib hashing across concurrent units.
                let mut is_rlib = true;
                let res = shared.get_or_hash(&tagged, || match compute()? {
                    Some(v) => Ok(v),
                    None => {
                        is_rlib = false;
                        Ok(([0; 32], None, 0))
                    }
                })?;
                if is_rlib {
                    if let Some((s, d)) = res.1 {
                        self.remember(s, d);
                    }
                    if res.2 > 0 {
                        self.count("hashed_bytes", res.2);
                    }
                    return Ok(res.0);
                }
            } else if let Some((digest, learned, hashed)) = compute()? {
                if let Some((s, d)) = learned {
                    self.remember(s, d);
                }
                self.count("hashed_bytes", hashed);
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
/// and optional `shared` stamps first and returns
/// `(digest, Option<(stamp_to_remember, digest)>, hashed_bytes)`.
pub fn digest_for_check(
    path: &Path,
    stamp: Stamp,
    in_target: bool,
    roots: &Roots,
    known: &HashMap<(u64, u64), (Stamp, Digest)>,
    shared: Option<&SharedStamps>,
) -> io::Result<CheckDigest> {
    const RLIB_STAMP_BIT: u64 = 1 << 62;
    let recall = |s: &Stamp| -> Option<Digest> {
        known
            .get(&(s.dev, s.ino))
            .filter(|(k, _)| k == s)
            .map(|(_, d)| *d)
            .or_else(|| shared.and_then(|sh| sh.get(s)))
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
                if let Some(sh) = shared {
                    sh.put(learned, digest);
                }
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
                    if let Some(sh) = shared {
                        sh.put(read, digest);
                    }
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
            if let Some(sh) = shared {
                sh.put(read, digest);
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
        let most = std::thread::available_parallelism().map_or(8, |n| n.get());
        let target_root = roots.path(crate::roots::TARGET);
        let target_missing = tree && target_root.is_some_and(|t| fs::symlink_metadata(t).is_err());
        let todo: Vec<usize> = if !tree {
            (0..dests.len()).filter(|&i| !left(i)).collect()
        } else if target_missing {
            let mut todo = Vec::with_capacity(dests.len());
            for (i, (output, dest)) in manifest.outputs.iter().zip(&dests).enumerate() {
                if left(i) {
                    continue;
                }
                if output.root == crate::roots::TARGET || !self.holds(output, dest, roots) {
                    todo.push(i);
                }
            }
            todo
        } else if dests.len() >= 128 {
            // Parallel `holds` check for existing targets (e.g. `fresh` and
            // `undo` on a 14k-file workspace): eliminates single-threaded
            // hashing of modified gigabyte binaries, and deduplicates
            // hardlinked twin binaries via `SharedStamps` singleflight.
            self.preload_stamps();
            let known = self.known_stamps();
            let local_shared = SharedStamps::new();
            let shared = self.shared.as_deref().unwrap_or(&local_shared);
            let threads = most.clamp(2, 16);
            let each = dests.len().div_ceil(threads);
            type HoldsLane = (Vec<usize>, Vec<(Stamp, Digest)>, u64);
            let dests_ref = &dests;
            let lanes: Vec<HoldsLane> = std::thread::scope(|scope| {
                let mut handles = Vec::with_capacity(threads);
                for chunk_start in (0..dests_ref.len()).step_by(each) {
                    let chunk_end = (chunk_start + each).min(dests_ref.len());
                    handles.push(scope.spawn(move || {
                        let mut todo_local = Vec::new();
                        let mut learned_local = Vec::new();
                        let mut hashed = 0u64;
                        for i in chunk_start..chunk_end {
                            if left(i) {
                                continue;
                            }
                            let (ok, learned, bytes) = holds_readonly(
                                &manifest.outputs[i],
                                &dests_ref[i],
                                roots,
                                known,
                                Some(shared),
                            );
                            if let Some(pair) = learned {
                                learned_local.push(pair);
                            }
                            hashed += bytes;
                            if !ok {
                                todo_local.push(i);
                            }
                        }
                        (todo_local, learned_local, hashed)
                    }));
                }
                handles
                    .into_iter()
                    .map(|h| h.join().unwrap_or_default())
                    .collect()
            });
            let mut todo = Vec::new();
            let mut learned = Vec::new();
            let mut hashed_bytes = 0u64;
            for (t, l, h) in lanes {
                todo.extend(t);
                learned.extend(l);
                hashed_bytes += h;
            }
            self.remember_many(learned, hashed_bytes);
            todo
        } else {
            let mut todo = Vec::with_capacity(dests.len());
            for (i, (output, dest)) in manifest.outputs.iter().zip(&dests).enumerate() {
                if !left(i) && !self.holds(output, dest, roots) {
                    todo.push(i);
                }
            }
            todo
        };
        // Find every object before writing anything: a result that is not
        // all there is found out before any of it is copied. Small and marked
        // objects are never stored compressed, so skip probing `.zst` for them,
        // deduplicate identical digests before stat'ing, and parallelize across
        // threads for whole builds.
        let prefer_packed = !self.clone && !self.use_hot_tier();
        let find_obj =
            |dir: &Path, digest: &Digest, may_pack: bool| -> io::Result<Option<(PathBuf, bool)>> {
                let order = if prefer_packed && may_pack {
                    [true, false]
                } else {
                    [false, true]
                };
                for packed in order {
                    let p = object_path(dir, digest, packed);
                    match fs::symlink_metadata(&p) {
                        Ok(_) => return Ok(Some((p, packed))),
                        Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                        Err(e) => return Err(e),
                    }
                }
                Ok(None)
            };
        let mut sources: Vec<Work<'_>> = Vec::with_capacity(todo.len());
        if tree && todo.len() >= 256 {
            let mut unique_digests: Vec<(Digest, bool)> = Vec::with_capacity(todo.len());
            let mut seen_digests: HashMap<Digest, usize> = HashMap::with_capacity(todo.len());
            for &i in &todo {
                let out = &manifest.outputs[i];
                let may_pack = out.encoding == Encoding::Raw && out.size >= CLONE_FLOOR;
                seen_digests.entry(out.digest).or_insert_with(|| {
                    let idx = unique_digests.len();
                    unique_digests.push((out.digest, may_pack));
                    idx
                });
            }
            type FoundObj = (Digest, Option<(PathBuf, bool)>);
            let dir = &self.dir;
            let threads = most.clamp(2, 24).min(unique_digests.len());
            let each = unique_digests.len().div_ceil(threads);
            let found_lanes: Vec<io::Result<Vec<FoundObj>>> = std::thread::scope(|scope| {
                let handles: Vec<_> = unique_digests
                    .chunks(each)
                    .map(|chunk| {
                        scope.spawn(move || {
                            let mut out = Vec::with_capacity(chunk.len());
                            for &(digest, may_pack) in chunk {
                                out.push((digest, find_obj(dir, &digest, may_pack)?));
                            }
                            Ok(out)
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| {
                        h.join()
                            .unwrap_or_else(|_| Err(io::Error::other("object lookup failed")))
                    })
                    .collect()
            });
            let mut located: HashMap<Digest, (PathBuf, bool)> =
                HashMap::with_capacity(unique_digests.len());
            for lane in found_lanes {
                for (digest, found) in lane? {
                    match found {
                        Some(pair) => {
                            located.insert(digest, pair);
                        }
                        None => {
                            *damaged = true;
                            return Ok(None);
                        }
                    }
                }
            }
            for &i in &todo {
                let out = &manifest.outputs[i];
                let (source, packed) = located[&out.digest].clone();
                sources.push(Work {
                    at: i,
                    output: out,
                    dest: &dests[i],
                    direct: None,
                    source,
                    packed,
                });
            }
        } else {
            for &i in &todo {
                let out = &manifest.outputs[i];
                let may_pack = out.encoding == Encoding::Raw && out.size >= CLONE_FLOOR;
                match find_obj(&self.dir, &out.digest, may_pack)? {
                    Some((source, packed)) => sources.push(Work {
                        at: i,
                        output: out,
                        dest: &dests[i],
                        direct: None,
                        source,
                        packed,
                    }),
                    None => {
                        *damaged = true;
                        return Ok(None);
                    }
                }
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
        // Deduplicate identical large raw outputs inside TARGET (such as
        // Cargo's hardlinked `deps/<bin>-<hash>` and `<bin>` pairs) so each
        // binary is decompressed, verified, and written once and hardlinked
        // to its duplicate destination paths — cutting restore I/O and target
        // disk footprint roughly in half.
        let bytes: u64 = sources.iter().map(|w| w.output.size).sum();
        let mut hardlinks: Vec<(usize, Work<'_>)> = Vec::new();
        if tree {
            let mut primary: HashMap<(Digest, u32), usize> = HashMap::new();
            let mut unique: Vec<Work<'_>> = Vec::with_capacity(sources.len());
            for work in sources {
                if work.output.root == crate::roots::TARGET
                    && work.output.encoding == Encoding::Raw
                    && work.output.size >= 64 * 1024
                {
                    let key = (work.output.digest, work.output.mode & 0o777);
                    if let Some(&prim_at) = primary.get(&key) {
                        hardlinks.push((prim_at, work));
                        continue;
                    }
                    primary.insert(key, work.at);
                }
                unique.push(work);
            }
            sources = unique;
        }
        // Every file of a whole build gets one time, as if the build had
        // just ended before anyone looked at its sources: no source is
        // newer, and one edited from then on will be.
        let time = if tree { time } else { None };
        // When restoring into a brand-new target directory that does not yet
        // exist, write all TARGET outputs directly into a sibling staging
        // directory and atomically rename the whole directory into place once
        // every file has been verified. This replaces thousands of per-file
        // renames and post-rename `stat` calls with one `rename(2)` and
        // in-place `fstat(2)` on each open file descriptor.
        let staging_target: Option<PathBuf> = if target_missing
            && time.is_some()
            && let Some(target) = target_root
            && sources
                .iter()
                .any(|w| w.output.root == crate::roots::TARGET)
            && let (Some(parent), Some(base)) = (target.parent(), target.file_name())
        {
            static DIR_SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let serial = DIR_SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut name = base.to_owned();
            name.push(format!(".jato-{}-{serial}", std::process::id()));
            let staging_dir = parent.join(name);
            let created = fs::create_dir_all(parent).and_then(|()| fs::create_dir(&staging_dir));
            created.ok().map(|()| staging_dir)
        } else {
            None
        };
        if let Some(staging_dir) = &staging_target {
            for work in &mut sources {
                if work.output.root == crate::roots::TARGET {
                    work.direct = Some(staging_dir.join(&work.output.path));
                }
            }
            for (_, work) in &mut hardlinks {
                work.direct = Some(staging_dir.join(&work.output.path));
            }
        }
        // Pre-create all unique parent directories in sorted order on one
        // thread: on ext4, concurrent `mkdir` calls into shared parents like
        // `debug/.fingerprint/` contend on the parent inode's `i_rwsem` and
        // run ~3x slower than sequential sorted creation.
        if sources.len() + hardlinks.len() >= 16 {
            let mut parents: Vec<&Path> = sources
                .iter()
                .chain(hardlinks.iter().map(|(_, w)| w))
                .filter_map(|work| {
                    work.direct
                        .as_deref()
                        .unwrap_or(work.dest.as_path())
                        .parent()
                })
                .collect();
            parents.sort_unstable();
            parents.dedup();
            for dir in parents {
                let _ = fs::create_dir_all(dir);
            }
        }
        let how = How {
            verify: self.verify,
            clone: self.clone,
            roots,
            time,
            umask: crate::command::umask(),
        };
        let write_bytes: u64 = sources.iter().map(|w| w.output.size).sum();
        let fd_cap = fd_thread_cap();
        let threads = if sources.len() <= 1 {
            1
        } else if tree {
            let max_thr = if write_bytes >= 64 << 20 { 32 } else { 24 };
            let want = if write_bytes >= 4 << 20 {
                sources.len()
            } else {
                sources.len() / 16 + 1
            };
            most.clamp(1, max_thr).min(want).min(fd_cap)
        } else if write_bytes >= 4 << 20 {
            most.clamp(1, 4).min(sources.len()).min(fd_cap)
        } else {
            1
        };
        let looked = begun.elapsed();
        // The largest first, each thread taking the next when it is done
        // with one: a build has a few files of hundreds of megabytes and
        // thousands of a few kilobytes.
        sources.sort_by_key(|work| std::cmp::Reverse(work.output.size));
        let queue = std::sync::Mutex::new(sources.into_iter());
        // A thread's staged files, direct stamps, and how it went.
        type Lane = (
            Vec<(PathBuf, usize)>,
            Vec<(usize, Stamp)>,
            Result<Option<bool>>,
        );
        fn run(
            queue: &std::sync::Mutex<std::vec::IntoIter<Work<'_>>>,
            failed: &std::sync::atomic::AtomicBool,
            how: &How<'_>,
        ) -> Lane {
            use std::sync::atomic::Ordering;
            let mut staged = Vec::new();
            let mut direct_stamps = Vec::new();
            let mut scratch = match RestoreScratch::new() {
                Ok(s) => s,
                Err(e) => {
                    failed.store(true, Ordering::Relaxed);
                    return (staged, direct_stamps, Err(e.into()));
                }
            };
            loop {
                if failed.load(Ordering::Relaxed) {
                    return (staged, direct_stamps, Ok(Some(true)));
                }
                let next = queue.lock().unwrap_or_else(|e| e.into_inner()).next();
                let Some(work) = next else {
                    return (staged, direct_stamps, Ok(Some(true)));
                };
                let at = work.at;
                let (staging, stamp, placed) = place(work, how, &mut scratch);
                staged.extend(staging.map(|s| (s, at)));
                if let Some(stamp) = stamp {
                    direct_stamps.push((at, stamp));
                }
                if !matches!(placed, Ok(Some(true))) {
                    failed.store(true, Ordering::Relaxed);
                    return (staged, direct_stamps, placed);
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
                            (
                                Vec::new(),
                                Vec::new(),
                                Err(anyhow::anyhow!("a restore failed")),
                            )
                        })
                    })
                    .collect()
            })
        };
        let written = begun.elapsed();
        let mut staged: Vec<(PathBuf, usize)> = Vec::new();
        let mut known: Vec<(usize, Option<Stamp>)> = Vec::new();
        let mut verdict = Ok(Some(true));
        for (files, direct_stamps, result) in results {
            staged.extend(files);
            known.extend(direct_stamps.into_iter().map(|(at, s)| (at, Some(s))));
            if !matches!(result, Ok(Some(true))) {
                verdict = result;
            }
        }
        let give_up = |staged: &[(PathBuf, usize)], staging_target: &Option<PathBuf>| {
            for (staging, _) in staged {
                let _ = fs::remove_file(staging);
            }
            if let Some(dir) = staging_target {
                let _ = fs::remove_dir_all(dir);
            }
        };
        if !matches!(verdict, Ok(Some(true))) {
            give_up(&staged, &staging_target);
            *damaged |= matches!(verdict, Ok(Some(false)));
            return verdict.map(|_| None);
        }
        if !target_missing {
            for (i, dest) in dests.iter().enumerate() {
                // What is left out is taken out of the target directory, for
                // Cargo to find missing. A file anywhere else is the person's.
                if left(i)
                    && manifest.outputs[i].root == crate::roots::TARGET
                    && let Err(e) = fs::remove_file(dest)
                    && e.kind() != io::ErrorKind::NotFound
                {
                    give_up(&staged, &staging_target);
                    return Err(e.into());
                }
            }
        }
        // Link deduplicated large TARGET outputs into `staging_target` before
        // moving the whole target directory into place.
        if let Some(staging_dir) = &staging_target
            && !hardlinks.is_empty()
        {
            let mut relinked: HashSet<usize> = HashSet::new();
            let mut fallback_scratch: Option<RestoreScratch> = None;
            for (primary_at, work) in hardlinks.drain(..) {
                let src = staging_dir.join(&manifest.outputs[primary_at].path);
                let dst = work
                    .direct
                    .clone()
                    .unwrap_or_else(|| staging_dir.join(&work.output.path));
                if let Some(p) = dst.parent() {
                    let _ = fs::create_dir_all(p);
                }
                if fs::hard_link(&src, &dst).is_ok() {
                    relinked.insert(primary_at);
                } else {
                    let dup_at = work.at;
                    let scratch = match &mut fallback_scratch {
                        Some(s) => s,
                        None => fallback_scratch.insert(RestoreScratch::new()?),
                    };
                    let (st, stamp, res) = place(work, &how, scratch);
                    if let Some(st) = st {
                        staged.push((st, dup_at));
                    }
                    if let Some(stamp) = stamp {
                        known.push((dup_at, Some(stamp)));
                    }
                    if !matches!(res, Ok(Some(true))) {
                        give_up(&staged, &staging_target);
                        *damaged |= matches!(res, Ok(Some(false)));
                        return res.map(|_| None);
                    }
                }
            }
            for primary_at in relinked {
                let src = staging_dir.join(&manifest.outputs[primary_at].path);
                if let Ok(meta) = fs::metadata(&src) {
                    known.push((primary_at, Some(Stamp::of(&meta))));
                }
            }
        }
        // Atomically move the fresh target directory into place if used. If a
        // concurrent process created `target` in the meantime, move the files
        // into place individually and remove the temporary directory.
        if let (Some(staging_dir), Some(target)) = (&staging_target, target_root)
            && let Err(e) = fs::rename(staging_dir, target)
        {
            if matches!(e.raw_os_error(), Some(libc::ENOTEMPTY | libc::EEXIST)) {
                let mut fallback_ok = true;
                for &at in &todo {
                    let out = &manifest.outputs[at];
                    if out.root != crate::roots::TARGET {
                        continue;
                    }
                    let src = staging_dir.join(&out.path);
                    let dst = &dests[at];
                    if let Some(p) = dst.parent() {
                        let _ = fs::create_dir_all(p);
                    }
                    if fs::rename(&src, dst).is_err() {
                        fallback_ok = false;
                        break;
                    }
                }
                let _ = fs::remove_dir_all(staging_dir);
                if !fallback_ok {
                    give_up(&staged, &None);
                    return Err(e.into());
                }
            } else {
                give_up(&staged, &staging_target);
                return Err(e.into());
            }
        }
        // Each remaining staged file goes to its place, and is known from then
        // on by what the filesystem says of it there.
        if !staged.is_empty() {
            type Known = Vec<(usize, Option<Stamp>)>;
            let dests_ref = &dests;
            let unlink_large = tree && !target_missing;
            let moved = |part: &[(PathBuf, usize)]| -> io::Result<Known> {
                let mut known = Vec::with_capacity(part.len());
                for (staging, at) in part {
                    // On ext4, renaming a newly-written large file over an
                    // existing non-empty file triggers `auto_da_alloc`
                    // synchronous block allocation/writeback (~40x slower on
                    // gigabytes of debug binaries). Unlinking the replaced
                    // TARGET artifact right before same-dir rename avoids it.
                    if unlink_large
                        && manifest.outputs[*at].root == crate::roots::TARGET
                        && manifest.outputs[*at].size >= 1 << 20
                    {
                        let _ = fs::remove_file(&dests_ref[*at]);
                    }
                    fs::rename(staging, &dests_ref[*at])?;
                    let stamp = fs::metadata(&dests_ref[*at])
                        .ok()
                        .map(|meta| Stamp::of(&meta));
                    known.push((*at, stamp));
                }
                Ok(known)
            };
            let move_threads = if staged.len() <= 1 {
                1
            } else if bytes >= 4 << 20 {
                most.clamp(1, 16).min(staged.len())
            } else {
                most.clamp(1, 16).min(staged.len() / 256 + 1)
            };
            let moves: Vec<io::Result<Known>> = if move_threads == 1 {
                vec![moved(&staged)]
            } else {
                let each = staged.len().div_ceil(move_threads);
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
            let mut failure = None;
            for part in moves {
                match part {
                    Ok(part) => known.extend(part),
                    Err(e) => failure = Some(e),
                }
            }
            if let Some(e) = failure {
                give_up(&staged, &None);
                return Err(e.into());
            }
        }
        // Link deduplicated large TARGET outputs into an existing target dir
        // after their primaries have been moved into place. Unlink + hard_link
        // runs in parallel so replacing dozens of 150 MiB binaries on `undo`
        // does not serialize ext4 unlink work on a single thread.
        if !hardlinks.is_empty() {
            let mut relinked: HashSet<usize> = HashSet::new();
            let mut fallbacks: Vec<Work<'_>> = Vec::new();
            if hardlinks.len() >= 4 {
                let link_threads = most.clamp(2, 16).min(hardlinks.len());
                let dests_ref = &dests;
                let items: Vec<(usize, PathBuf, PathBuf)> = hardlinks
                    .iter()
                    .map(|(primary_at, work)| {
                        (
                            *primary_at,
                            dests_ref[*primary_at].clone(),
                            dests_ref[work.at].clone(),
                        )
                    })
                    .collect();
                let each = items.len().div_ceil(link_threads);
                let ok_flags: Vec<bool> = std::thread::scope(|scope| {
                    let handles: Vec<_> = items
                        .chunks(each)
                        .map(|chunk| {
                            scope.spawn(move || {
                                let mut res = Vec::with_capacity(chunk.len());
                                for (_, src, dst) in chunk {
                                    if let Some(p) = dst.parent() {
                                        let _ = fs::create_dir_all(p);
                                    }
                                    let _ = fs::remove_file(dst);
                                    res.push(fs::hard_link(src, dst).is_ok());
                                }
                                res
                            })
                        })
                        .collect();
                    handles
                        .into_iter()
                        .flat_map(|h| h.join().unwrap_or_default())
                        .collect()
                });
                for ((primary_at, work), ok) in hardlinks.into_iter().zip(ok_flags) {
                    if ok {
                        relinked.insert(primary_at);
                    } else {
                        fallbacks.push(work);
                    }
                }
            } else {
                for (primary_at, work) in hardlinks {
                    let src = &dests[primary_at];
                    let dst = &dests[work.at];
                    if let Some(p) = dst.parent() {
                        let _ = fs::create_dir_all(p);
                    }
                    let _ = fs::remove_file(dst);
                    if fs::hard_link(src, dst).is_ok() {
                        relinked.insert(primary_at);
                    } else {
                        fallbacks.push(work);
                    }
                }
            }
            if !fallbacks.is_empty() {
                let mut fallback_scratch = RestoreScratch::new()?;
                for work in fallbacks {
                    let dup_at = work.at;
                    let (st, _, res) = place(work, &how, &mut fallback_scratch);
                    match (st, res) {
                        (Some(st), Ok(Some(true))) => {
                            fs::rename(&st, &dests[dup_at])?;
                            let stamp = fs::metadata(&dests[dup_at])
                                .ok()
                                .map(|meta| Stamp::of(&meta));
                            known.push((dup_at, stamp));
                        }
                        (st, res) => {
                            if let Some(st) = st {
                                let _ = fs::remove_file(st);
                            }
                            *damaged |= matches!(res, Ok(Some(false)));
                            return res.map(|_| None);
                        }
                    }
                }
            }
            for primary_at in relinked {
                if let Ok(meta) = fs::metadata(&dests[primary_at]) {
                    known.push((primary_at, Some(Stamp::of(&meta))));
                }
            }
        }
        if tree && !todo.is_empty() && (!target_missing || time.is_none()) {
            // Without a time taken before the sources were looked at, the
            // files get the one the filesystem has now.
            let late = match time {
                Some(_) => None,
                None => just_before_now(&dests[todo[0]]),
            };
            // Update untouched files' mtimes on a single thread (avoids ext4
            // journal lock contention across 16 threads, which is 2.5x slower
            // for pure metadata updates).
            if late.is_some() {
                known.clear();
            }
            for (i, dest) in dests.iter().enumerate() {
                let written_now = todo.binary_search(&i).is_ok();
                let Some(stamp) = late.or(time.filter(|_| !written_now)) else {
                    continue;
                };
                let source = manifest.outputs[i].root != crate::roots::TARGET && !written_now;
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
                    if let Ok(meta) = fs::metadata(dest) {
                        self.remember(Stamp::of(&meta), manifest.outputs[i].digest);
                    }
                }
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
    pub fn place_saved(&self, saved: &[crate::manifest::Output], roots: &Roots) -> bool {
        if saved.is_empty() {
            return true;
        }
        let mut dests = Vec::with_capacity(saved.len());
        for file in saved {
            match roots.join(file.root, &file.path) {
                Some(dest) => dests.push(dest),
                None => return false,
            }
        }
        // Pre-create the session directory once.
        let mut parents: HashSet<&Path> = HashSet::new();
        for dest in &dests {
            if let Some(parent) = dest.parent() {
                parents.insert(parent);
            }
        }
        for dir in parents {
            if fs::create_dir_all(dir).is_err() {
                return false;
            }
        }
        let prefer_packed = !self.clone && !self.use_hot_tier();
        let mut works: Vec<Work<'_>> = Vec::with_capacity(saved.len());
        for (i, (file, dest)) in saved.iter().zip(&dests).enumerate() {
            let order = if prefer_packed {
                [true, false]
            } else {
                [false, true]
            };
            let found = order.into_iter().find_map(|packed| {
                let p = object_path(&self.dir, &file.digest, packed);
                fs::symlink_metadata(&p).ok().map(|_| (p, packed))
            });
            let Some((source, packed)) = found else {
                return false;
            };
            works.push(Work {
                at: i,
                output: file,
                dest,
                direct: (!dest.exists()).then(|| dest.clone()),
                source,
                packed,
            });
        }
        let how = How {
            verify: self.verify,
            clone: self.clone,
            roots,
            time: None,
            umask: crate::command::umask(),
        };
        works.sort_by_key(|w| std::cmp::Reverse(w.output.size));
        let most = std::thread::available_parallelism().map_or(4, |n| n.get());
        let threads = most
            .clamp(1, 4)
            .min(works.len() / 16 + 1)
            .min(fd_thread_cap());
        let queue = Mutex::new(works.into_iter());
        let failed = std::sync::atomic::AtomicBool::new(false);
        let run = || -> (Vec<PathBuf>, bool) {
            use std::sync::atomic::Ordering;
            let mut written = Vec::new();
            let Ok(mut scratch) = RestoreScratch::new() else {
                failed.store(true, Ordering::Relaxed);
                return (written, false);
            };
            loop {
                if failed.load(Ordering::Relaxed) {
                    return (written, false);
                }
                let next = queue.lock().unwrap_or_else(|e| e.into_inner()).next();
                let Some(work) = next else {
                    return (written, true);
                };
                let dest = work.dest.clone();
                match place(work, &how, &mut scratch) {
                    (None, _, Ok(Some(true))) => {
                        written.push(dest);
                    }
                    (Some(staging), _, Ok(Some(true))) if fs::rename(&staging, &dest).is_ok() => {
                        written.push(dest);
                    }
                    (staging, _, _) => {
                        if let Some(staging) = staging {
                            let _ = fs::remove_file(staging);
                        }
                        failed.store(true, Ordering::Relaxed);
                        return (written, false);
                    }
                }
            }
        };
        let results: Vec<(Vec<PathBuf>, bool)> = if threads == 1 {
            vec![run()]
        } else {
            std::thread::scope(|scope| {
                let handles: Vec<_> = (0..threads).map(|_| scope.spawn(run)).collect();
                handles
                    .into_iter()
                    .map(|h| h.join().unwrap_or((Vec::new(), false)))
                    .collect()
            })
        };
        let ok = results.iter().all(|(_, ok)| *ok);
        if !ok {
            for (files, _) in results {
                for path in files {
                    let _ = fs::remove_file(path);
                }
            }
        }
        ok
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

    /// Whether the cache index already holds an object with this digest.
    pub fn has_object(&self, digest: &Digest) -> bool {
        self.conn
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM objects WHERE digest=?1)")
            .and_then(|mut s| s.query_row([digest], |r| r.get::<_, bool>(0)))
            .unwrap_or(false)
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

    /// Verify in parallel that every non-skipped output already holds its
    /// recorded bytes and permissions on disk. Short-circuits on the first
    /// mismatch and records any newly hashed stamps.
    pub fn holds_all_parallel(
        &mut self,
        outputs: &[crate::manifest::Output],
        skip: &[bool],
        roots: &Roots,
    ) -> bool {
        let mut checks: Vec<(usize, PathBuf)> = Vec::with_capacity(outputs.len());
        for (i, output) in outputs.iter().enumerate() {
            if skip.get(i).copied().unwrap_or(false) {
                continue;
            }
            let Some(dest) = roots.join(output.root, &output.path) else {
                return false;
            };
            checks.push((i, dest));
        }
        if checks.is_empty() {
            return true;
        }
        if checks.len() < 128 {
            for (i, dest) in checks {
                if !self.holds(&outputs[i], &dest, roots) {
                    return false;
                }
            }
            return true;
        }
        self.preload_stamps();
        let known = &self.known;
        let shared = self.shared.as_deref();
        let most = std::thread::available_parallelism().map_or(8, |n| n.get());
        let threads = most.clamp(2, 16).min(checks.len() / 64 + 1);
        let each = checks.len().div_ceil(threads);
        let mismatch = std::sync::atomic::AtomicBool::new(false);
        type HoldsLane = (bool, Vec<(Stamp, Digest)>, u64);
        let lanes: Vec<HoldsLane> = std::thread::scope(|scope| {
            let handles: Vec<_> = checks
                .chunks(each)
                .map(|chunk| {
                    let mismatch = &mismatch;
                    scope.spawn(move || {
                        let mut learned = Vec::new();
                        let mut hashed = 0u64;
                        for (i, dest) in chunk {
                            if mismatch.load(std::sync::atomic::Ordering::Relaxed) {
                                return (false, learned, hashed);
                            }
                            let (ok, l, h) =
                                holds_readonly(&outputs[*i], dest, roots, known, shared);
                            if let Some(pair) = l {
                                learned.push(pair);
                            }
                            hashed += h;
                            if !ok {
                                mismatch.store(true, std::sync::atomic::Ordering::Relaxed);
                                return (false, learned, hashed);
                            }
                        }
                        (true, learned, hashed)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or((false, Vec::new(), 0)))
                .collect()
        });
        let mut all_ok = true;
        let mut all_learned = Vec::new();
        let mut total_hashed = 0u64;
        for (ok, l, h) in lanes {
            all_ok &= ok;
            all_learned.extend(l);
            total_hashed += h;
        }
        self.remember_many(all_learned, total_hashed);
        all_ok
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
    #[cfg(test)]
    pub fn stage(&mut self, file: &Path, roots: &Roots) -> Result<Staged> {
        let hot = self.use_hot_tier();
        let mut scratch = StageScratch::new()?;
        let (staged, learned) =
            stage_file(&self.dir, self.clone, hot, true, file, roots, &mut scratch)?;
        if let Some((stamp, digest)) = learned {
            self.remember(stamp, digest);
        }
        Ok(staged)
    }

    /// The same for many files at once, on as many threads as there are
    /// processors: each file is read through, and a build's programs are
    /// gigabytes. Set `one_pass` when files are freshly generated (such as
    /// unit outputs or incremental state) to avoid a pre-check read pass.
    pub fn stage_many_with(
        &mut self,
        files: &[&Path],
        roots: &Roots,
        one_pass: bool,
    ) -> Vec<Result<Staged>> {
        self.stage_many_capped(files, roots, one_pass, 24)
    }

    pub fn stage_many_capped(
        &mut self,
        files: &[&Path],
        roots: &Roots,
        one_pass: bool,
        max_threads: usize,
    ) -> Vec<Result<Staged>> {
        type Done = Result<(Staged, Option<(Stamp, Digest)>)>;
        let most = std::thread::available_parallelism().map_or(4, |n| n.get());
        let threads = most
            .clamp(1, max_threads.max(1))
            .min(files.len() / 4 + 1)
            .min(fd_thread_cap());
        let (dir, clone, hot) = (self.dir.clone(), self.clone, self.use_hot_tier());
        let next = std::sync::atomic::AtomicUsize::new(0);
        let work = || -> Vec<(usize, Done)> {
            let mut done = Vec::new();
            let mut scratch = match StageScratch::new() {
                Ok(s) => s,
                Err(e) => {
                    let at = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if at < files.len() {
                        done.push((at, Err(e.into())));
                    }
                    return done;
                }
            };
            loop {
                let at = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(file) = files.get(at) else {
                    return done;
                };
                done.push((
                    at,
                    stage_file(&dir, clone, hot, one_pass, file, roots, &mut scratch),
                ));
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

    /// Make a finished command part of the cache. Returns the committed key,
    /// or `None` if the entry does not fit in half the cache budget.
    pub fn commit(
        &mut self,
        recipe: &Digest,
        record: &Manifest,
        detail: &[u8],
        cost_ms: u64,
        staged: &mut [Staged],
    ) -> Result<Option<Digest>> {
        let (key, manifest) = record.encode_with_key(recipe)?;
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
            return Ok(None);
        }
        let dir = self.dir.clone();
        let time = now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let added = tx.execute(
            "INSERT OR IGNORE INTO entries(key,recipe,used,cost_ms,manifest,detail)
             VALUES(?1,?2,?3,?4,?5,?6)",
            params![key, recipe, time, cost_ms, manifest, detail],
        )?;
        if added == 0 {
            tx.execute(
                "UPDATE entries SET used=?1 WHERE key=?2",
                params![time, key],
            )?;
            write_bookkeeping(
                &tx,
                &self.learned,
                &self.counts,
                &self.used,
                &self.noted,
                time,
            )?;
            tx.commit()?;
            self.learned.clear();
            self.counts.clear();
            self.used.clear();
            self.noted.clear();
            return Ok(Some(key));
        }
        let entry = tx.last_insert_rowid();
        let mut charge = ENTRY_OVERHEAD + (manifest.len() + detail.len()) as u64;
        let mut verified_objs: HashSet<Digest> = HashSet::with_capacity(staged.len());
        for output in staged.iter_mut() {
            let known: bool = verified_objs.contains(&output.digest)
                || tx
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
            verified_objs.insert(output.digest);
        }
        // Outputs that were already stored by another result are shared
        // with it, and must still be there. Deduplicate digests so identical
        // files (e.g. Cargo hardlinks) only hit SQLite once.
        let mut linked: HashSet<Digest> =
            HashSet::with_capacity(record.outputs.len() + record.saved.len());
        for output in record.outputs.iter().chain(&record.saved) {
            if !linked.insert(output.digest) {
                continue;
            }
            let known: bool = verified_objs.contains(&output.digest)
                || tx
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
        write_bookkeeping(
            &tx,
            &self.learned,
            &self.counts,
            &self.used,
            &self.noted,
            time,
        )?;
        tx.commit()?;
        self.learned.clear();
        self.counts.clear();
        self.used.clear();
        self.noted.clear();
        if let Some(remote) = self.remote.clone()
            && !remote.readonly
        {
            let digests: Vec<Digest> = linked.into_iter().collect();
            match remote.push_entry(&dir, recipe, &key, &manifest, detail, cost_ms, &digests) {
                Ok(()) => self.count("remote_push", 1),
                Err(e) => crate::command::log("remote", format_args!("push failed: {e:#}")),
            }
        }
        Ok(Some(key))
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
            write_bookkeeping(&tx, &learned, &counts, &used, &noted, now())?;
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
            remote: self.remote.as_ref().map(|r| r.url.clone()),
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

/// Lock-free mirror of `Store::holds` for parallel existing-target restore
/// checks (`fresh` and `undo`). Returns `(holds, optional_learned_stamp,
/// newly_hashed_bytes)`.
fn holds_readonly(
    output: &crate::manifest::Output,
    dest: &Path,
    roots: &Roots,
    known: &HashMap<(u64, u64), (Stamp, Digest)>,
    shared: Option<&SharedStamps>,
) -> (bool, Option<(Stamp, Digest)>, u64) {
    let Ok(meta) = fs::symlink_metadata(dest) else {
        return (false, None, 0);
    };
    if !meta.is_file() || meta.permissions().mode() & 0o777 != output.mode & 0o777 {
        return (false, None, 0);
    }
    match output.encoding {
        Encoding::Text => {
            let ok = meta.len() <= TEXT_LIMIT * 4
                && fs::read(dest).is_ok_and(|text| {
                    blake3::hash(&roots.mark(&text)).as_bytes() == &output.digest
                });
            return (ok, None, 0);
        }
        Encoding::CargoDep => {
            let ok = meta.len() <= TEXT_LIMIT * 4
                && fs::read(dest).is_ok_and(|bytes| {
                    crate::roots::mark_cargo_dep(&bytes, roots)
                        .is_some_and(|marked| blake3::hash(&marked).as_bytes() == &output.digest)
                });
            return (ok, None, 0);
        }
        Encoding::Raw => (),
    }
    if meta.len() != output.size {
        return (false, None, 0);
    }
    let stamp = Stamp::of(&meta);
    if let Some((_, d)) = known
        .get(&(stamp.dev, stamp.ino))
        .filter(|(k, _)| k == &stamp)
    {
        return (*d == output.digest, None, 0);
    }
    if let Some(d) = shared.and_then(|sh| sh.get(&stamp)) {
        return (d == output.digest, None, 0);
    }
    let compute = || -> io::Result<CheckDigest> {
        let (digest, read, _) = scan::digest_file(dest, &[])?;
        if !stamp.same_contents(&read) {
            return Err(io::Error::other("file changed while it was read"));
        }
        Ok((digest, Some((read, digest)), stamp.size))
    };
    let res = match shared {
        Some(sh) => sh.get_or_hash(&stamp, compute),
        None => compute(),
    };
    match res {
        Ok((digest, learned, hashed)) => (digest == output.digest, learned, hashed),
        Err(_) => (false, None, 0),
    }
}

fn write_bookkeeping(
    tx: &Transaction<'_>,
    learned: &[(Stamp, Digest)],
    counts: &BTreeMap<String, i64>,
    used: &[Digest],
    noted: &[Digest],
    time: i64,
) -> Result<()> {
    if !learned.is_empty() {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO stamps VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(dev,ino) DO UPDATE SET
             size=excluded.size,mtime=excluded.mtime,ctime=excluded.ctime,
             digest=excluded.digest,seen=excluded.seen",
        )?;
        for (stamp, digest) in learned {
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
    if !used.is_empty() {
        let mut stmt = tx.prepare_cached("UPDATE entries SET used=?1 WHERE key=?2")?;
        for key in used {
            stmt.execute(params![time, key])?;
        }
    }
    if !noted.is_empty() {
        let mut stmt = tx.prepare_cached("INSERT OR REPLACE INTO ran VALUES(?1,?2)")?;
        for key in noted {
            stmt.execute(params![key, time])?;
        }
    }
    for (name, value) in counts {
        add(tx, name, *value)?;
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renaming_a_populated_directory_keeps_file_stamps_intact() {
        let dir = tempfile::tempdir().unwrap();
        let staging = dir.path().join("target.jato-test");
        let sub = staging.join("debug/deps");
        fs::create_dir_all(&sub).unwrap();
        let file_path = sub.join("libfoo.rlib");
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&file_path)
            .unwrap();
        f.write_all(b"hello world").unwrap();
        let times = [
            libc::timespec {
                tv_sec: 0,
                tv_nsec: libc::UTIME_OMIT,
            },
            libc::timespec {
                tv_sec: 1_700_000_000,
                tv_nsec: 123_456_789,
            },
        ];
        assert_eq!(unsafe { libc::futimens(f.as_raw_fd(), times.as_ptr()) }, 0);
        let before = Stamp::of(&f.metadata().unwrap());
        drop(f);

        let target = dir.path().join("target");
        fs::rename(&staging, &target).unwrap();
        let after = Stamp::of(&fs::metadata(target.join("debug/deps/libfoo.rlib")).unwrap());
        assert_eq!(before, after);
    }

    #[test]
    fn bulk_decompress_handles_empty_and_small_packed_objects() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::at(dir.path().join("cache")).unwrap();
        store.clone = false;
        store.force_packed = true;
        let roots = Roots::default();
        let src_dir = dir.path().join("src");
        fs::create_dir_all(&src_dir).unwrap();
        let empty = src_dir.join("empty.bin");
        let small = src_dir.join("small.bin");
        fs::write(&empty, b"").unwrap();
        let payload: Vec<u8> = (0..64 * 1024).map(|i| (i % 251) as u8).collect();
        fs::write(&small, &payload).unwrap();
        let mut staged = vec![
            store.stage(&empty, &roots).unwrap(),
            store.stage(&small, &roots).unwrap(),
        ];
        let mut manifest = Manifest::default();
        let out_dir = dir.path().join("out");
        let outputs = [
            (out_dir.join("empty.bin"), &staged[0]),
            (out_dir.join("small.bin"), &staged[1]),
        ];
        for (p, s) in outputs {
            manifest.outputs.push(crate::manifest::Output {
                root: crate::roots::ABSOLUTE,
                path: p,
                digest: s.digest,
                size: s.size,
                mode: s.mode,
                encoding: s.encoding,
            });
        }
        let detail = crate::command::encode_detail(0, b"", b"");
        manifest.detail = *blake3::hash(&detail).as_bytes();
        let recipe = [7u8; 32];
        assert!(
            store
                .commit(&recipe, &manifest, &detail, 1, &mut staged)
                .unwrap()
                .is_some()
        );
        let cand = store.candidates(&recipe).unwrap().pop().unwrap();
        let mut damaged = false;
        let restored = store
            .restore(&cand, &manifest, &roots, Placing::default(), &mut damaged)
            .unwrap();
        assert!(!damaged);
        assert!(restored.is_some());
        assert_eq!(fs::read(out_dir.join("empty.bin")).unwrap(), b"");
        assert_eq!(fs::read(out_dir.join("small.bin")).unwrap(), payload);
    }

    #[test]
    fn streaming_decompress_restores_large_packed_objects_and_detects_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::at(dir.path().join("cache")).unwrap();
        store.clone = false;
        store.force_packed = true;
        let roots = Roots::default();
        let src = dir.path().join("big.bin");
        // > BULK_DECOMPRESS_LIMIT (4 MiB) to exercise streaming raw::Decoder.
        let payload: Vec<u8> = (0..(5 * 1024 * 1024 + 123))
            .map(|i| ((i * 31 + (i >> 9)) % 251) as u8)
            .collect();
        fs::write(&src, &payload).unwrap();
        let mut staged = vec![store.stage(&src, &roots).unwrap()];
        let out_path = dir.path().join("out/big.bin");
        let mut manifest = Manifest::default();
        manifest.outputs.push(crate::manifest::Output {
            root: crate::roots::ABSOLUTE,
            path: out_path.clone(),
            digest: staged[0].digest,
            size: staged[0].size,
            mode: staged[0].mode,
            encoding: staged[0].encoding,
        });
        let detail = crate::command::encode_detail(0, b"", b"");
        manifest.detail = *blake3::hash(&detail).as_bytes();
        let recipe = [9u8; 32];
        assert!(
            store
                .commit(&recipe, &manifest, &detail, 1, &mut staged)
                .unwrap()
                .is_some()
        );
        let cand = store.candidates(&recipe).unwrap().pop().unwrap();
        let mut damaged = false;
        let restored = store
            .restore(&cand, &manifest, &roots, Placing::default(), &mut damaged)
            .unwrap();
        assert!(!damaged);
        assert!(restored.is_some());
        assert_eq!(fs::read(&out_path).unwrap(), payload);

        // Truncate the stored packed object and confirm restore flags it damaged.
        let obj = object_path(&store.dir, &manifest.outputs[0].digest, true);
        let bytes = fs::read(&obj).unwrap();
        let mut perms = fs::metadata(&obj).unwrap().permissions();
        perms.set_mode(0o644);
        fs::set_permissions(&obj, perms).unwrap();
        fs::write(&obj, &bytes[..bytes.len() / 2]).unwrap();
        let mut damaged = false;
        let restored = store
            .restore(&cand, &manifest, &roots, Placing::default(), &mut damaged)
            .unwrap();
        assert!(damaged);
        assert!(restored.is_none());
    }

    #[test]
    fn restore_deduplicates_identical_large_target_outputs_with_hardlinks() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::at(dir.path().join("cache")).unwrap();
        store.clone = false;
        store.force_packed = true;
        let target = dir.path().join("target");
        let work = dir.path().join("work");
        fs::create_dir_all(&work).unwrap();
        let roots = Roots {
            work: crate::roots::Root::new(&work),
            target: crate::roots::Root::new(&target),
        };
        let src = dir.path().join("app.bin");
        // >= 64 KiB raw binary so tree-mode restore deduplicates it with a hardlink.
        let payload: Vec<u8> = (0..(128 * 1024 + 7))
            .map(|i| ((i * 17 + (i >> 7)) % 251) as u8)
            .collect();
        fs::write(&src, &payload).unwrap();
        fs::set_permissions(&src, fs::Permissions::from_mode(0o755)).unwrap();
        let mut staged = vec![store.stage(&src, &roots).unwrap()];
        let mut manifest = Manifest::default();
        for rel in ["debug/deps/app-1234", "debug/app"] {
            manifest.outputs.push(crate::manifest::Output {
                root: crate::roots::TARGET,
                path: PathBuf::from(rel),
                digest: staged[0].digest,
                size: staged[0].size,
                mode: 0o755,
                encoding: Encoding::Raw,
            });
        }
        let detail = crate::command::encode_detail(0, b"", b"");
        manifest.detail = *blake3::hash(&detail).as_bytes();
        let recipe = [11u8; 32];
        assert!(
            store
                .commit(&recipe, &manifest, &detail, 1, &mut staged)
                .unwrap()
                .is_some()
        );
        let cand = store.candidates(&recipe).unwrap().pop().unwrap();
        let time = Some(libc::timespec {
            tv_sec: 1_700_000_000,
            tv_nsec: 0,
        });
        // 1. Fresh missing target (`staging_target` atomic rename path).
        let mut damaged = false;
        let restored = store
            .restore(
                &cand,
                &manifest,
                &roots,
                Placing {
                    tree: true,
                    leave: None,
                    time,
                    remap_out: None,
                },
                &mut damaged,
            )
            .unwrap()
            .unwrap();
        assert!(!damaged);
        assert_eq!(restored.files, 2);
        let a = target.join("debug/deps/app-1234");
        let b = target.join("debug/app");
        assert_eq!(fs::read(&a).unwrap(), payload);
        assert_eq!(fs::read(&b).unwrap(), payload);
        let ma = fs::metadata(&a).unwrap();
        let mb = fs::metadata(&b).unwrap();
        assert_eq!((ma.dev(), ma.ino()), (mb.dev(), mb.ino()));
        assert_eq!(ma.nlink(), 2);
        assert_eq!(ma.permissions().mode() & 0o777, 0o755);

        // 2. Subsequent `fresh` check sees 0 files to restore.
        let mut damaged = false;
        let fresh = store
            .restore(
                &cand,
                &manifest,
                &roots,
                Placing {
                    tree: true,
                    leave: None,
                    time,
                    remap_out: None,
                },
                &mut damaged,
            )
            .unwrap()
            .unwrap();
        assert!(!damaged);
        assert_eq!(fresh.files, 0);

        // 3. Overwriting both files in an existing target (`undo` path).
        fs::write(&a, vec![0u8; payload.len()]).unwrap();
        fs::write(&b, vec![1u8; payload.len()]).unwrap();
        let mut damaged = false;
        let undone = store
            .restore(
                &cand,
                &manifest,
                &roots,
                Placing {
                    tree: true,
                    leave: None,
                    time,
                    remap_out: None,
                },
                &mut damaged,
            )
            .unwrap()
            .unwrap();
        assert!(!damaged);
        assert_eq!(undone.files, 2);
        assert_eq!(fs::read(&a).unwrap(), payload);
        assert_eq!(fs::read(&b).unwrap(), payload);
        let ma2 = fs::metadata(&a).unwrap();
        let mb2 = fs::metadata(&b).unwrap();
        assert_eq!((ma2.dev(), ma2.ino()), (mb2.dev(), mb2.ino()));
        assert_eq!(ma2.nlink(), 2);
    }
}
