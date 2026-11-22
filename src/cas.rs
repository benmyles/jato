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
use crate::manifest::Manifest;
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

const SCHEMA_VERSION: u32 = 1;
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
CREATE TABLE stamps(
    dev INTEGER NOT NULL, ino INTEGER NOT NULL, size INTEGER NOT NULL, mtime INTEGER NOT NULL,
    ctime INTEGER NOT NULL, digest BLOB NOT NULL, seen INTEGER NOT NULL, PRIMARY KEY(dev, ino)
) WITHOUT ROWID;
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
    pub stored_bytes: u64,
    pub artifact_bytes: u64,
    pub database_bytes: u64,
    pub entries: u64,
    pub objects: u64,
    pub remembered_files: u64,
    pub counters: BTreeMap<String, u64>,
}

pub struct Candidate {
    pub key: Digest,
    pub manifest: Vec<u8>,
    used: i64,
}

/// One output of a command, copied into the cache directory but not yet
/// part of the cache.
pub struct Staged {
    pub digest: Digest,
    pub size: u64,
    pub mode: u32,
    /// Text whose roots were replaced by marks.
    pub marked: bool,
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
}

pub struct Store {
    conn: Connection,
    pub dir: PathBuf,
    env_limit: Option<u64>,
    /// Whether restored bytes are read back and compared with their digest.
    verify: bool,
    /// Whether to try sharing extents with the files commands write.
    clone: bool,
    known: HashMap<(u64, u64), (Stamp, Digest)>,
    learned: Vec<(Stamp, Digest)>,
    counts: BTreeMap<String, i64>,
    used: Vec<Digest>,
    serial: u32,
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

/// One output to put in place.
struct Work<'a> {
    output: &'a crate::manifest::Output,
    dest: &'a PathBuf,
    source: File,
    packed: bool,
}

struct How<'a> {
    verify: bool,
    clone: bool,
    roots: &'a Roots,
}

/// Write one output to a staging file beside its destination. Returns the
/// staging file if one was created, and Some(true) if it holds the output,
/// Some(false) if the object is not what it should be, None if this
/// command lacks a root the output names.
fn place(work: Work<'_>, how: &How<'_>) -> (Option<PathBuf>, Result<Option<bool>>) {
    let Work {
        output,
        dest,
        mut source,
        packed,
    } = work;
    let Some(file_name) = dest.file_name() else {
        return (None, Err(anyhow::anyhow!("output without a file name")));
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
        if output.marked {
            ensure!(output.size <= TEXT_LIMIT * 4, "oversized text object");
            let mut text = Vec::with_capacity(output.size as usize);
            source.read_to_end(&mut text)?;
            if blake3::hash(&text).as_bytes() != &output.digest {
                return Ok(Some(false));
            }
            let Some(text) = how.roots.unmark(&text) else {
                return Ok(None);
            };
            out.write_all(&text)?;
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
            if !how.clone || clone_file(&source, &out).is_err() {
                let copied = io::copy(&mut source, &mut out)?;
                ensure!(copied == output.size, "short copy of a cached object");
            }
            if how.verify {
                use std::io::Seek;
                out.rewind()?;
                let (digest, length, _) = scan::digest_open(&mut out, &[])?;
                if length != output.size || digest != output.digest {
                    return Ok(Some(false));
                }
            }
        }
        // The umask applied at creation. Outputs keep their mode.
        out.set_permissions(fs::Permissions::from_mode(output.mode & 0o777))?;
        Ok(Some(true))
    })();
    (Some(staging), written)
}

impl Store {
    pub fn open() -> Result<Self> {
        let env_limit = env::var("JATO_MAX_SIZE")
            .ok()
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
        match env::var("JATO_STORAGE").as_deref() {
            Ok("packed") => store.clone = false,
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
            verify,
            clone: true,
            known: HashMap::new(),
            learned: Vec::new(),
            counts: BTreeMap::new(),
            used: Vec::new(),
            serial: 0,
        })
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
        if read != stamp {
            return Err(io::Error::other("file changed while it was read"));
        }
        self.remember(stamp, digest);
        self.count("hashed_bytes", stamp.size);
        Ok(digest)
    }

    // ---- lookups ----------------------------------------------------------

    /// Read from one consistent state of the index until `end_read`,
    /// without taking and dropping a lock for every question.
    pub fn begin_read(&self) {
        let _ = self.conn.execute_batch("BEGIN");
    }

    pub fn end_read(&self) {
        let _ = self.conn.execute_batch("COMMIT");
    }

    /// Earlier results for a command line, most recently used first.
    pub fn candidates(&self, recipe: &Digest) -> Result<Vec<Candidate>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT key,manifest,used FROM entries WHERE recipe=?1 ORDER BY used DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![recipe, VARIANTS as i64], |r| {
            Ok(Candidate {
                key: r.get(0)?,
                manifest: r.get(1)?,
                used: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
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
    pub fn restore(
        &mut self,
        candidate: &Candidate,
        manifest: &Manifest,
        roots: &Roots,
        tree: bool,
        damaged: &mut bool,
    ) -> Result<Option<Restored>> {
        let mut dests = Vec::with_capacity(manifest.outputs.len());
        for output in &manifest.outputs {
            match roots.join(output.root, &output.path) {
                Some(dest) => dests.push(dest),
                None => return Ok(None),
            }
        }
        let mut todo = Vec::with_capacity(dests.len());
        for (i, (output, dest)) in manifest.outputs.iter().zip(&dests).enumerate() {
            if !tree || !self.holds(output, dest, roots) {
                todo.push(i);
            }
        }
        // Open every object before writing anything, so a result evicted
        // meanwhile leaves the target untouched.
        let mut sources = Vec::with_capacity(todo.len());
        for &i in &todo {
            let digest = &manifest.outputs[i].digest;
            let plain = File::open(object_path(&self.dir, digest, false));
            let found = match plain {
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    File::open(object_path(&self.dir, digest, true)).map(|f| (f, true))
                }
                other => other.map(|f| (f, false)),
            };
            match found {
                Ok((source, packed)) => sources.push(Work {
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
        let how = How {
            verify: self.verify,
            clone: self.clone,
            roots,
        };
        let bytes: u64 = sources.iter().map(|w| w.output.size).sum();
        // Each piece of work leaves a staged file beside its destination.
        let threads = if tree {
            8.min(sources.len() / 16 + 1)
        } else {
            1
        };
        let mut lanes: Vec<Vec<Work<'_>>> = (0..threads).map(|_| Vec::new()).collect();
        for (i, work) in sources.into_iter().enumerate() {
            lanes[i % threads].push(work);
        }
        // A lane's staged files with where each belongs, and how it went.
        type Lane<'a> = (Vec<(PathBuf, &'a PathBuf)>, Result<Option<bool>>);
        fn run<'a>(lane: Vec<Work<'a>>, how: &How<'_>) -> Lane<'a> {
            let mut staged = Vec::with_capacity(lane.len());
            for work in lane {
                let dest = work.dest;
                let (staging, placed) = place(work, how);
                staged.extend(staging.map(|s| (s, dest)));
                if !matches!(placed, Ok(Some(true))) {
                    return (staged, placed);
                }
            }
            (staged, Ok(Some(true)))
        }
        let results: Vec<Lane<'_>> = if threads == 1 {
            lanes.into_iter().map(|lane| run(lane, &how)).collect()
        } else {
            std::thread::scope(|scope| {
                let handles: Vec<_> = lanes
                    .into_iter()
                    .map(|lane| scope.spawn(|| run(lane, &how)))
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
        let mut staged: Vec<(PathBuf, &PathBuf)> = Vec::new();
        let mut verdict = Ok(Some(true));
        for (files, result) in results {
            staged.extend(files);
            if !matches!(result, Ok(Some(true))) {
                verdict = result;
            }
        }
        if !matches!(verdict, Ok(Some(true))) {
            for (staging, _) in &staged {
                let _ = fs::remove_file(staging);
            }
            *damaged |= matches!(verdict, Ok(Some(false)));
            return verdict.map(|_| None);
        }
        let mut done = 0;
        let renamed: io::Result<()> = staged.iter().try_for_each(|(staging, dest)| {
            fs::rename(staging, dest)?;
            done += 1;
            Ok(())
        });
        if let Err(e) = renamed {
            for (staging, _) in &staged[done..] {
                let _ = fs::remove_file(staging);
            }
            return Err(e.into());
        }
        if tree && !todo.is_empty() {
            // Now, to the microsecond: no source file can be newer.
            let stamp = now();
            let times = [
                libc::timespec {
                    tv_sec: 0,
                    tv_nsec: libc::UTIME_OMIT,
                },
                libc::timespec {
                    tv_sec: (stamp / 1_000_000) as _,
                    tv_nsec: ((stamp % 1_000_000) * 1000) as _,
                },
            ];
            for dest in &dests {
                if let Ok(name) = std::ffi::CString::new(dest.as_os_str().as_bytes()) {
                    unsafe { libc::utimensat(libc::AT_FDCWD, name.as_ptr(), times.as_ptr(), 0) };
                }
            }
        }
        if !todo.is_empty() {
            let touched: Box<dyn Iterator<Item = usize>> = if tree {
                Box::new(0..dests.len())
            } else {
                Box::new(todo.iter().copied())
            };
            for i in touched {
                // Later commands read this file. They need not hash it again.
                let output = &manifest.outputs[i];
                if !output.marked
                    && let Ok(meta) = fs::metadata(&dests[i])
                {
                    self.remember(Stamp::of(&meta), output.digest);
                }
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
        }))
    }

    /// Whether the file at `dest` already is this output.
    fn holds(&mut self, output: &crate::manifest::Output, dest: &Path, roots: &Roots) -> bool {
        let Ok(meta) = fs::symlink_metadata(dest) else {
            return false;
        };
        if !meta.is_file() || meta.permissions().mode() & 0o777 != output.mode & 0o777 {
            return false;
        }
        if output.marked {
            // Text is stored with marks for the roots. Compare it that way.
            return meta.len() <= TEXT_LIMIT * 4
                && fs::read(dest).is_ok_and(|text| {
                    blake3::hash(&roots.mark(&text)).as_bytes() == &output.digest
                });
        }
        meta.len() == output.size
            && self
                .digest_known(dest, Stamp::of(&meta))
                .is_ok_and(|digest| digest == output.digest)
    }

    /// Drop a result whose objects turned out to be missing or damaged.
    pub fn forget(&mut self, key: &Digest) {
        self.count("damaged", 1);
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
        let spellings = roots.spellings();
        let needles: Vec<&[u8]> = spellings.iter().map(|(_, s)| *s).collect();
        let mut source = File::open(file).with_context(|| format!("opening {}", file.display()))?;
        let meta = source.metadata()?;
        ensure!(meta.is_file(), "{} is not a regular file", file.display());
        let before = Stamp::of(&meta);
        let mode = meta.permissions().mode() & 0o777;
        self.serial += 1;
        let temp =
            self.dir
                .join("objects")
                .join(format!("tmp-{}-{}", std::process::id(), self.serial));
        let mut staged = Staged {
            digest: [0; 32],
            size: before.size,
            mode,
            marked: false,
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
            let text = names.contains(&true) && memchr::memchr(0, &bytes).is_none();
            if text || before.size < CLONE_FLOOR {
                let stored = if text {
                    roots.mark(&bytes)
                } else {
                    (&bytes[..]).into()
                };
                staged.digest = *blake3::hash(&stored).as_bytes();
                if object_path(&self.dir, &staged.digest, false).is_file() {
                    staged.temp = None;
                } else {
                    let mut out = create()?;
                    out.write_all(&stored)?;
                    out.set_permissions(fs::Permissions::from_mode(0o444))?;
                }
                staged.size = stored.len() as u64;
                staged.stored = stored.len() as u64;
                staged.marked = text;
                staged.names = if text {
                    vec![false; needles.len()]
                } else {
                    names
                };
                ensure!(
                    Stamp::of(&source.metadata()?) == before,
                    "{} changed while it was stored",
                    file.display()
                );
                if !text {
                    self.remember(before, staged.digest);
                }
                return Ok(staged);
            }
            use std::io::Seek;
            source.rewind()?;
        }
        let mut out = create()?;
        if self.clone && clone_file(&source, &out).is_ok() {
            // The clone is a private snapshot. Its digest is the digest of
            // what the cache holds, whatever happens to the original.
            let (digest, length, names) = scan::digest_open(&mut out, &needles)?;
            ensure!(length == before.size, "{} changed size", file.display());
            staged.digest = digest;
            staged.names = names;
        } else {
            // No shared extents here. Keep a compressed copy.
            let mut hasher = blake3::Hasher::new();
            let mut encoder = zstd::stream::write::Encoder::new(&mut out, 3)?;
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
            staged.packed = true;
            staged.stored = out.metadata()?.len();
        }
        out.set_permissions(fs::Permissions::from_mode(0o444))?;
        if Stamp::of(&source.metadata()?) == before {
            self.remember(before, staged.digest);
        }
        Ok(staged)
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
        let total: u64 = staged.iter().map(|s| s.stored).sum();
        if total + (manifest.len() + detail.len()) as u64 > limit / 2 {
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
        for output in &record.outputs {
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
    pub fn flush(&mut self) {
        if self.learned.is_empty() && self.counts.is_empty() && self.used.is_empty() {
            return;
        }
        let learned = std::mem::take(&mut self.learned);
        let counts = std::mem::take(&mut self.counts);
        let used = std::mem::take(&mut self.used);
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
            stored_bytes: counters.remove("bytes").unwrap_or(0),
            artifact_bytes: one("SELECT sum(size) FROM objects")?,
            database_bytes,
            entries: one("SELECT count(*) FROM entries")?,
            objects: one("SELECT count(*) FROM objects")?,
            remembered_files: one("SELECT count(*) FROM stamps")?,
            counters,
        })
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

/// When more than `limit` is stored, remove the least recently used entries
/// until no more than `target` is.
fn evict(tx: &Transaction<'_>, dir: &Path, limit: u64, target: u64) -> Result<()> {
    let mut used = usage(tx)?;
    if used <= limit {
        return Ok(());
    }
    let mut evicted = 0i64;
    while used > target {
        let batch: Vec<i64> = tx
            .prepare_cached("SELECT id FROM entries ORDER BY used LIMIT 32")?
            .query_map([], |r| r.get(0))?
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
            let _ = fs::remove_file(lock.path());
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
