pub(crate) mod objects;
mod packed;

use crate::staging::Output;
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CHUNK: usize = 256 * 1024;
const DEFAULT_LIMIT: u64 = 1024 * 1024 * 1024;
const SCHEMA: &str = include_str!("schema.sql");
const SCHEMA_VERSION: u32 = 17;
const MAX_PLAN: usize = 256 * 1024;
pub const DIAGNOSTIC_LIMIT: usize = 512 * 1024;
pub type Digest = [u8; 32];

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
    let multiplier = match s[split..].trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kib" => 1 << 10,
        "m" | "mib" => 1 << 20,
        "g" | "gib" => 1 << 30,
        _ => bail!("invalid cache size {s:?}, use bytes, KiB, MiB, or GiB"),
    };
    s[..split]
        .parse::<u64>()?
        .checked_mul(multiplier)
        .filter(|n| *n <= i64::MAX as u64)
        .context("cache size too large")
}

fn cache_limit() -> Result<u64> {
    env::var("JATO_MAX_SIZE").map_or_else(|_| Ok(DEFAULT_LIMIT), |s| parse_size(&s))
}

#[derive(Debug)]
pub(crate) enum Damaged {
    Entry,
    Chunk(Digest),
    Object(Digest),
}

impl std::fmt::Display for Damaged {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cached compiler output failed an integrity check")
    }
}
impl std::error::Error for Damaged {}

pub struct Evidence<'a> {
    pub inputs: &'a [u8],
    pub sources: &'a Digest,
    pub sample: Option<Digest>,
    pub needs: &'a [Digest],
    pub topology: &'a Digest,
    pub codegen: bool,
}

pub struct Store {
    conn: Connection,
    pub dir: PathBuf,
    limit: u64,
    allow_sealed: Option<bool>,
}

pub struct Hit {
    pub diagnostics: Vec<u8>,
    key: Digest,
    compiler_ms: u64,
}

// Most WAL writers update a handful of rows. A short first retry keeps a
// burst of wrapper completions from incurring millisecond-sized delays.
fn wait_for_writer(attempt: i32) -> bool {
    if attempt >= 130 {
        return false;
    }
    let micros = match attempt {
        0..=5 => 50u64 << attempt,
        6..=10 => 2_000,
        _ => 25_000,
    };
    std::thread::sleep(Duration::from_micros(micros));
    true
}

#[derive(Serialize)]
pub struct Stats {
    pub directory: PathBuf,
    pub limit_bytes: u64,
    pub charged_bytes: u64,
    pub database_bytes: u64,
    pub entries: u64,
    pub chunks: u64,
    pub compressed_bytes: u64,
    pub frame_file_bytes: u64,
    pub patched_files: u64,
    pub patch_bytes: u64,
    pub sealed_objects: u64,
    pub sealed_bytes: u64,
    pub pending_entries: u64,
    pub probe_entries: u64,
    pub artifact_bytes: u64,
    pub counters: BTreeMap<String, u64>,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as i64
}

struct Prepared {
    hash: Digest,
    size: u64,
    mode: u32,
    frames: usize,
    sealed: Option<crate::sealed::Object>,
}

#[derive(Default)]
struct Decoder {
    last: Option<Digest>,
    zstd: Option<zstd::bulk::Decompressor<'static>>,
    buffer: Vec<u8>,
}

impl Decoder {
    fn decode<'a>(&'a mut self, packed: &'a [u8], raw: usize, hash: Digest) -> Result<&'a [u8]> {
        ensure!(raw <= CHUNK && packed.len() <= raw, Damaged::Chunk(hash));
        if raw == packed.len() {
            return Ok(packed);
        }
        if self.last.as_ref() == Some(&hash) && self.buffer.len() == raw {
            return Ok(&self.buffer);
        }
        let zstd = match &mut self.zstd {
            Some(zstd) => zstd,
            None => self.zstd.insert(zstd::bulk::Decompressor::new()?),
        };
        self.buffer.clear();
        self.buffer.reserve(raw);
        zstd.decompress_to_buffer(packed, &mut self.buffer)
            .map_err(|_| Damaged::Chunk(hash))?;
        ensure!(self.buffer.len() == raw, Damaged::Chunk(hash));
        self.last = Some(hash);
        Ok(&self.buffer)
    }
}

fn encode_plan(raw: &[u8]) -> Result<(Digest, Vec<u8>)> {
    ensure!(raw.len() <= MAX_PLAN, "input manifest exceeds size limit");
    let hash = *blake3::hash(raw).as_bytes();
    // JSON always starts with a printable byte. A zero tags a compressed
    // manifest with its bounded, exact decompressed length.
    if raw.len() >= 1024 {
        let packed = zstd::bulk::compress(raw, 1)?;
        if packed.len() + 5 < raw.len() {
            let mut data = Vec::with_capacity(packed.len() + 5);
            data.push(0);
            data.extend_from_slice(&(raw.len() as u32).to_le_bytes());
            data.extend_from_slice(&packed);
            return Ok((hash, data));
        }
    }
    Ok((hash, raw.to_vec()))
}

fn decode_plan(stored: &[u8]) -> Result<Vec<u8>> {
    if stored.first() != Some(&0) {
        return Ok(stored.to_vec());
    }
    let header = stored.get(1..5).context("short input manifest")?;
    let len = u32::from_le_bytes(header.try_into()?) as usize;
    ensure!(len <= MAX_PLAN, "oversized input manifest");
    let raw = zstd::bulk::decompress(&stored[5..], len)?;
    ensure!(raw.len() == len, "short decoded input manifest");
    Ok(raw)
}

impl Store {
    pub fn open() -> Result<Self> {
        let fallback = cache_limit();
        let mut store = Self::at(
            cache_dir()?,
            fallback.as_ref().copied().unwrap_or(DEFAULT_LIMIT),
        )?;
        if let Err(error) = fallback
            && stored_limit(&store.conn)?.is_none()
        {
            return Err(error);
        }
        store.allow_sealed = match env::var("JATO_STORAGE").as_deref() {
            Ok("packed") => Some(false),
            Err(_) | Ok("auto") => None,
            _ => bail!("JATO_STORAGE must be auto or packed"),
        };
        Ok(store)
    }

    fn at(dir: PathBuf, limit: u64) -> Result<Self> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        let mut conn = Connection::open(dir.join("artifacts-v1.sqlite3"))?;
        // Each wrapper is short-lived. Keep the WAL between invocations;
        // checkpoint by size and during GC, rather than fsyncing on every exit.
        conn.set_db_config(
            rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
            true,
        )?;
        conn.busy_handler(Some(wait_for_writer))?;
        conn.execute_batch(
            "PRAGMA foreign_keys=ON; PRAGMA cache_size=-1024; PRAGMA mmap_size=33554432; PRAGMA synchronous=NORMAL; PRAGMA wal_autocheckpoint=256; PRAGMA journal_size_limit=1048576;",
        )?;
        let version: u32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        ensure!(
            version <= SCHEMA_VERSION,
            "unrecognized Jato cache format {version}"
        );
        if version < SCHEMA_VERSION {
            let init = OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(dir.join("schema.lock"))?;
            init.lock()?;
            let version: u32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            ensure!(
                version <= SCHEMA_VERSION,
                "unrecognized Jato cache format {version}"
            );
            if version == 0 {
                conn.execute_batch("PRAGMA auto_vacuum=FULL; PRAGMA journal_mode=WAL;")?;
            }
            if version < SCHEMA_VERSION {
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                if version == 2 {
                    tx.execute_batch(
                        "ALTER TABLE plans ADD COLUMN codegen INTEGER NOT NULL DEFAULT 0",
                    )?;
                }
                if (1..9).contains(&version) {
                    tx.execute_batch(
                        "ALTER TABLE files ADD COLUMN single INTEGER NOT NULL DEFAULT 0",
                    )?;
                }
                if (2..9).contains(&version) {
                    tx.execute_batch(
                        "ALTER TABLE plans ADD COLUMN manifest INTEGER REFERENCES manifests",
                    )?;
                }
                if (1..11).contains(&version) {
                    tx.execute_batch(
                        "DROP TRIGGER IF EXISTS drop_single; DROP TRIGGER IF EXISTS drop_part;",
                    )?;
                }
                if version != 0 && !tx.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('chunks') WHERE name='external')",[],|r| r.get::<_,bool>(0))? {
                    tx.execute_batch("ALTER TABLE chunks ADD COLUMN external INTEGER NOT NULL DEFAULT 0")?;
                }
                tx.execute_batch(SCHEMA)?;
                tx.commit()?;
            }
        }
        Ok(Self {
            conn,
            dir,
            limit,
            allow_sealed: Some(false),
        })
    }

    pub fn budget(&self) -> Result<u64> {
        Ok(stored_limit(&self.conn)?.unwrap_or(self.limit))
    }

    pub fn set_budget(&mut self, size: Option<u64>) -> Result<u64> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(size) = size {
            ensure!(
                size <= i64::MAX as u64,
                "cache limit exceeds SQLite's integer range"
            );
            tx.execute(
                "INSERT INTO cache_settings VALUES(1,?1) ON CONFLICT(singleton)
                DO UPDATE SET max_bytes=excluded.max_bytes",
                [size],
            )?;
        } else {
            tx.execute("DELETE FROM cache_settings", [])?;
        }
        tx.commit()?;
        self.budget()
    }

    pub fn compiler_channel(
        &self,
        compiler: &std::ffi::OsStr,
        identity: &Digest,
    ) -> Result<crate::admission::Channel> {
        use crate::admission::Channel;
        let saved = self
            .conn
            .query_row(
                "SELECT stable FROM compilers WHERE identity=?1",
                [identity],
                |r| r.get(0),
            )
            .optional()?;
        match saved {
            Some(1i64) => return Ok(Channel::Stable),
            Some(2) => return Ok(Channel::Nightly),
            Some(3) => return Ok(Channel::Unsupported),
            _ => (), // Older stores recorded every non-stable compiler as zero.
        }
        let channel = crate::admission::compiler_channel(compiler)?;
        self.record_compiler(identity, channel)?;
        Ok(channel)
    }

    pub fn record_compiler(
        &self,
        identity: &Digest,
        channel: crate::admission::Channel,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO compilers VALUES(?1,?2,?3) ON CONFLICT(identity) DO UPDATE
             SET stable=excluded.stable,seen=excluded.seen WHERE compilers.stable=0",
            params![identity, channel as i64, now()],
        )?;
        Ok(())
    }

    pub fn probe(&self, key: &Digest) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let result = self
            .conn
            .query_row(
                "SELECT stdout,stderr,checksum FROM probes
             WHERE key=?1 AND length(stdout)+length(stderr)<=65536",
                [key],
                |r| {
                    Ok((
                        r.get::<_, Vec<u8>>(0)?,
                        r.get::<_, Vec<u8>>(1)?,
                        r.get::<_, Digest>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((stdout, stderr, checksum)) = result else {
            return Ok(None);
        };
        if probe_checksum(&stdout, &stderr) != checksum {
            self.conn
                .execute("DELETE FROM probes WHERE key=?1", [key])?;
            return Ok(None);
        }
        Ok(Some((stdout, stderr)))
    }

    pub fn save_probe(&mut self, key: &Digest, stdout: &[u8], stderr: &[u8]) -> Result<()> {
        const MAX_PROBES: u64 = 256 * 1024;
        if stdout.len() + stderr.len() > 65536 {
            return Ok(());
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let limit = stored_limit(&tx)?.unwrap_or(self.limit);
        let budget = (limit / 32).min(MAX_PROBES);
        if (stdout.len() + stderr.len() + 160) as u64 > budget {
            return Ok(());
        }
        tx.execute(
            "INSERT OR IGNORE INTO probes VALUES(?1,?2,?3,?4,?5)",
            params![key, stdout, stderr, probe_checksum(stdout, stderr), now()],
        )?;
        while tx.query_row(
            "SELECT COUNT(*)>64 OR COALESCE(SUM(length(stdout)+length(stderr)+160),0)>?1 FROM probes",
            [budget], |r| r.get::<_, bool>(0),
        )? {
            tx.execute("DELETE FROM probes WHERE key=(SELECT key FROM probes ORDER BY seen LIMIT 1)", [])?;
        }
        let evicted = evict(&tx, limit)?;
        tx.commit()?;
        if evicted != 0 {
            self.collect_objects(false)?;
        }
        Ok(())
    }

    pub fn data_version(&self) -> Result<u32> {
        Ok(self
            .conn
            .prepare_cached("PRAGMA data_version")?
            .query_row([], |r| r.get(0))?)
    }

    pub fn plans(&mut self, recipe: &Digest) -> Result<(Vec<Vec<u8>>, u32)> {
        let tx = self.conn.unchecked_transaction()?;
        let mut q = tx.prepare_cached(
            "SELECT p.manifest,m.hash,
                    CASE WHEN p.manifest IS NULL AND length(p.inputs)<=?2 THEN p.inputs
                         WHEN length(m.data)<=?2 THEN m.data END
             FROM entries e JOIN plans p ON p.entry=e.id LEFT JOIN manifests m ON p.manifest=m.id
             WHERE e.recipe=?1 AND NOT EXISTS(SELECT 1 FROM needs WHERE needs.entry=e.id)
             ORDER BY e.access DESC LIMIT 48",
        )?;
        let mut plans = Vec::new();
        let mut ids = BTreeSet::new();
        let mut damaged = Vec::new();
        let mut rows = q.query(params![recipe, MAX_PLAN])?;
        while let Some(row) = rows.next()? {
            let bytes = if let Some(id) = row.get::<_, Option<i64>>(0)? {
                if !ids.insert(id) {
                    continue;
                }
                let raw = row
                    .get_ref(2)?
                    .as_blob()
                    .ok()
                    .and_then(|stored| decode_plan(stored).ok());
                let checksum = row.get_ref(1)?.as_blob().ok();
                match raw {
                    Some(bytes) if Some(blake3::hash(&bytes).as_bytes().as_slice()) == checksum => {
                        bytes
                    }
                    _ => {
                        damaged.push(id);
                        continue;
                    }
                }
            } else {
                let Some(raw) = row.get::<_, Option<Vec<u8>>>(2)? else {
                    continue;
                };
                raw
            };
            if !plans.contains(&bytes) {
                plans.push(bytes);
                if plans.len() == 4 {
                    break;
                }
            }
        }
        drop(rows);
        drop(q);
        // Capture the version in the same read snapshot. Only a miss needs
        // to compare it after electing a compiler.
        let version = self.data_version()?;
        // Keeping the read snapshot through invalidation prevents removal of
        // a newer manifest installed concurrently under a recycled row id.
        let reap = !damaged.is_empty();
        for id in damaged {
            tx.execute(
                "DELETE FROM entries WHERE id IN (SELECT entry FROM plans WHERE manifest=?1)",
                [id],
            )?;
            tx.execute("INSERT INTO counters VALUES('corrupt',1) ON CONFLICT(name) DO UPDATE SET value=value+1",[])?;
        }
        tx.commit()?;
        if reap {
            self.collect_objects(false)?;
        }
        Ok((plans, version))
    }

    pub fn may_match_sample(&self, recipe: &Digest, sample: &Digest) -> Result<bool> {
        // A legacy or unsampled entry must remain discoverable. The sample
        // includes the recipe, so a positive global index lookup is enough.
        Ok(self
            .conn
            .prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM input_samples WHERE digest=?2)
            OR EXISTS(SELECT 1 FROM entries e JOIN plans p ON p.entry=e.id WHERE e.recipe=?1
                AND NOT EXISTS(SELECT 1 FROM input_samples s WHERE s.entry=e.id))",
            )?
            .query_row(params![recipe, sample], |r| r.get(0))?)
    }

    pub fn has_sources(&self, digest: &Digest) -> Result<bool> {
        Ok(self
            .conn
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM source_variants WHERE digest=?1)")?
            .query_row([digest], |r| r.get(0))?)
    }

    pub fn has_source_topology(&self, sources: &Digest, topology: &Digest) -> Result<bool> {
        Ok(self
            .conn
            .prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM source_variants s JOIN plans p ON p.entry=s.entry
             WHERE s.digest=?1 AND (p.topology=?2 OR EXISTS(
                 SELECT 1 FROM worktree_topologies w WHERE w.entry=p.entry AND ?2 IN(w.latest,w.previous,w.oldest))))",
            )?
            .query_row(params![sources, topology], |r| r.get(0))?)
    }

    pub fn matches_topology(&self, key: &Digest, topology: &Digest) -> Result<Option<bool>> {
        Ok(self.conn.prepare_cached(
            "SELECT CASE WHEN p.topology=?2 THEN 1 ELSE EXISTS(
                SELECT 1 FROM worktree_topologies w WHERE w.entry=p.entry AND ?2 IN(w.latest,w.previous,w.oldest)
             ) END FROM entries e JOIN plans p ON p.entry=e.id WHERE e.key=?1"
        )?.query_row(params![key, topology], |r| r.get(0)).optional()?)
    }

    pub fn refresh_topology(&self, key: &Digest, topology: &Digest, projected: bool) -> Result<()> {
        if projected {
            // Keep the original witness plus the last three independently
            // resolved alternatives. An optional hint cannot evict artifacts.
            self.conn.execute(
                "INSERT INTO worktree_topologies(entry,latest)
                 SELECT e.id,?2 FROM entries e JOIN plans p ON p.entry=e.id WHERE e.key=?1 AND p.topology<>?2 AND
                 ((SELECT bytes FROM usage)+192<=COALESCE((SELECT max_bytes FROM cache_settings WHERE singleton=1),?3)
                  OR EXISTS(SELECT 1 FROM worktree_topologies w WHERE w.entry=e.id))
                 ON CONFLICT(entry) DO UPDATE SET oldest=previous,previous=latest,latest=excluded.latest
                 WHERE latest<>excluded.latest AND previous IS NOT excluded.latest AND oldest IS NOT excluded.latest",
                params![key,topology,self.limit.min(i64::MAX as u64)]
            )?;
        } else {
            self.conn.execute(
                "UPDATE plans SET topology=?1 WHERE entry=(SELECT id FROM entries WHERE key=?2)",
                params![topology, key],
            )?;
        }
        Ok(())
    }

    pub fn has_artifact_size(&self, size: u64, codegen: bool) -> Result<bool> {
        Ok(self.conn.prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM files f JOIN plans p ON p.entry=f.entry
             WHERE f.size=?1 AND p.codegen>=?2 AND NOT EXISTS(SELECT 1 FROM needs n WHERE n.entry=f.entry))"
        )?.query_row(params![size,codegen], |r|r.get(0))?)
    }

    pub fn knows_artifact(&self, digest: &Digest, codegen: bool) -> Result<bool> {
        Ok(self.conn.prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM files JOIN plans ON plans.entry=files.entry WHERE digest=?1 AND (?2=0 OR codegen=1) AND NOT EXISTS(SELECT 1 FROM needs WHERE needs.entry=files.entry))",
        )?.query_row(params![digest,codegen], |r| r.get(0))?)
    }

    pub fn count(&self, event: &str, value: u64) -> Result<()> {
        self.conn.execute("INSERT INTO counters VALUES(?1,?2) ON CONFLICT(name) DO UPDATE SET value=value+excluded.value", params![event, value])?;
        Ok(())
    }

    fn sealed_output(
        &mut self,
        unit: (&Digest, usize),
        path: &std::path::Path,
        source: &File,
        length: u64,
        charge: u64,
        budget: (u64, u64),
    ) -> Option<crate::sealed::Object> {
        let (used, limit) = budget;
        let standalone = charge.saturating_add(length).saturating_add(256);
        if length < CHUNK as u64
            || standalone > limit
            || !*self
                .allow_sealed
                .get_or_insert_with(|| crate::sealed::supported(&self.dir))
        {
            return None;
        }
        if used != 0
            && let Ok(Some(object)) =
                crate::sealed::reuse(source, path, &self.dir, length, &self.conn)
        {
            return Some(object);
        }
        if used != 0
            && let Ok(Some(object)) =
                crate::sealed::revision(source, &self.dir, length, unit, &self.conn)
            && charge.saturating_add(object.charge()) <= limit
        {
            return Some(object);
        }
        if standalone > limit.saturating_sub(used) {
            return None;
        }
        let object = crate::sealed::prepare(source, &self.dir).ok();
        if object.is_none() {
            self.allow_sealed = Some(false);
        }
        object
    }

    pub fn save(
        &mut self,
        recipe: &Digest,
        key: &Digest,
        outputs: &[PathBuf],
        stderr: &[u8],
        elapsed_ms: u64,
        plan: Option<Evidence<'_>>,
    ) -> Result<bool> {
        // Compress in parallel with other compilers, outside SQLite's single writer.
        // Anonymous staging files are reclaimed by the OS even on process death.
        let mut buffer = Vec::new();
        let manifest = plan.as_ref().map(|p| encode_plan(p.inputs)).transpose()?;
        let plan_cost = plan
            .as_ref()
            .zip(manifest.as_ref())
            .map_or(0, |(p, (_, bytes))| {
                272 + bytes.len() + 96 * p.needs.len() + if p.sample.is_some() { 128 } else { 0 }
            });
        let mut charge = (320 + stderr.len() + 128 * outputs.len() + plan_cost) as u64;
        let (used, limit) = self.conn.query_row(
            "SELECT bytes,(SELECT max_bytes FROM cache_settings WHERE singleton=1) FROM usage",
            [],
            |r| {
                Ok((
                    r.get::<_, u64>(0)?,
                    r.get::<_, Option<u64>>(1)?.unwrap_or(self.limit),
                ))
            },
        )?;
        if charge > limit || stderr.len() > DIAGNOSTIC_LIMIT {
            return Ok(false);
        }
        let mut files = Vec::new();
        let mut packed = packed::Recorder::new(&self.dir, used != 0);
        let mut sidecar = None;
        for (slot, path) in outputs.iter().enumerate() {
            let mut source =
                File::open(path).with_context(|| format!("opening {}", path.display()))?;
            let metadata = source.metadata()?;
            ensure!(metadata.is_file(), "expected regular compiler output");
            let mut file = Prepared {
                hash: [0; 32],
                size: 0,
                mode: metadata.permissions().mode() & 0o777,
                frames: 0,
                sealed: None,
            };
            let length = metadata.len();
            if let Some(object) =
                self.sealed_output((recipe, slot), path, &source, length, charge, (used, limit))
            {
                ensure!(
                    crate::fingerprint::stamp(&metadata)
                        == crate::fingerprint::stamp(&fs::metadata(path)?),
                    "compiler output changed during capture"
                );
                file.hash = object.patch.as_ref().map_or(object.hash, |p| p.result);
                file.size = length;
                charge += object.charge();
                file.sealed = Some(object);
                files.push(file);
                continue;
            }
            let mut hasher = blake3::Hasher::new();
            let mut boundaries = Vec::new();
            match path.extension().and_then(|v| v.to_str()) {
                Some("rmeta") => sidecar = crate::archive::Sidecar::read(&source, length),
                Some("rlib") if length > CHUNK as u64 => {
                    boundaries = crate::archive::boundaries(&source, length, sidecar.as_ref())
                        .unwrap_or_default();
                }
                _ => (),
            }
            let mut boundaries = boundaries.into_iter().peekable();
            let window = length.min(CHUNK as u64) as usize;
            if buffer.len() < window {
                buffer.resize(window, 0);
            }
            let mut buffered = 0;
            while file.size < length {
                while boundaries.next_if_eq(&file.size).is_some() {}
                let end = boundaries.peek().copied().unwrap_or(length);
                let remaining = end - file.size;
                let window = remaining.min(CHUNK as u64) as usize;
                source.read_exact(&mut buffer[buffered..window])?;
                let n = packed::cut(&buffer[..window], remaining);
                let raw = &buffer[..n];
                let digest = *blake3::hash(raw).as_bytes();
                if length > CHUNK as u64 {
                    hasher.update(raw);
                    charge += 96; // one ordered part reference
                }
                file.size += n as u64;
                file.frames += 1;
                file.hash = digest;
                charge += packed.frame(&self.conn, digest, raw)?;
                if charge > limit {
                    return Ok(false);
                }
                buffered = window - n;
                if buffered != 0 {
                    buffer.copy_within(n..window, 0);
                }
            }
            ensure!(
                file.size == metadata.len()
                    && crate::fingerprint::stamp(&metadata)
                        == crate::fingerprint::stamp(&fs::metadata(path)?),
                "compiler output changed while recording"
            );
            if file.frames != 1 {
                file.hash = *hasher.finalize().as_bytes();
            }
            files.push(file);
        }
        packed.rewind()?;
        let mut publishing_sealed = false;
        let publication = (|| -> Result<(bool, bool, bool)> {
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            // Another process can set the cache-wide policy while rustc runs.
            let limit = stored_limit(&tx)?.unwrap_or(self.limit);
            if charge > limit {
                return Ok((false, false, false));
            }
            let added = tx.execute("INSERT OR IGNORE INTO entries(key,recipe,access,stderr,stderr_hash,compile_ms) VALUES(?1,?2,?3,?4,?5,?6)",
            params![key, recipe, now(), stderr, blake3::hash(stderr).as_bytes(), elapsed_ms])?;
            if added == 0 {
                return Ok((true, false, false));
            }
            let entry = tx.last_insert_rowid();
            for (slot, file) in files.iter().enumerate() {
                let single = file.frames == 1;
                tx.execute(
                "INSERT INTO files(entry,slot,digest,size,mode,single) VALUES(?1,?2,?3,?4,?5,?6)",
                params![entry, slot, &file.hash, file.size, file.mode, single],
            )?;
                if let Some(object) = &file.sealed {
                    publishing_sealed = true;
                    crate::sealed::publish(object, &crate::sealed::path(&self.dir, &object.hash))?;
                    tx.execute("INSERT INTO objects VALUES(?1,?2,?3,1) ON CONFLICT(hash) DO UPDATE SET refs=refs+1",
                    params![object.hash,object.verity,object.bytes])?;
                    tx.execute(
                        "INSERT INTO clones VALUES(?1,?2,?3)",
                        params![entry, slot, object.hash],
                    )?;
                    if let Some(patch) = &object.patch {
                        tx.execute(
                            "INSERT INTO patches VALUES(?1,?2,?3)",
                            params![entry, slot, &patch.data],
                        )?;
                    }
                }
                let mut source = None;
                let mut offset = 0;
                for ordinal in 0..file.frames {
                    let (digest, raw) =
                        packed.publish(&tx, &outputs[slot], &mut source, offset, &mut buffer)?;
                    offset += raw;
                    if !single {
                        tx.prepare_cached("INSERT INTO parts VALUES(?1,?2,?3,?4)")?
                            .execute(params![entry, slot, ordinal, &digest])?;
                    }
                }
            }
            if packed.reused > 0 {
                tx.execute("INSERT INTO counters VALUES('packed_reused_bytes',?1) ON CONFLICT(name) DO UPDATE SET value=value+excluded.value", [packed.reused])?;
            }
            let reused: u64 = files
                .iter()
                .filter(|f| {
                    f.sealed
                        .as_ref()
                        .is_some_and(|o| o.reused && o.patch.is_none())
                })
                .map(|f| f.size)
                .sum();
            if reused > 0 {
                tx.execute("INSERT INTO counters VALUES('sealed_reused_bytes',?1) ON CONFLICT(name) DO UPDATE SET value=value+excluded.value", [reused])?;
            }
            if let (Some(plan), Some((hash, bytes))) = (plan, manifest) {
                let manifest: i64 = tx.query_row(
                    "INSERT INTO manifests(hash,data,refs) VALUES(?1,?2,1) ON CONFLICT(hash)
                 DO UPDATE SET refs=refs+1 RETURNING id",
                    params![hash, bytes],
                    |r| r.get(0),
                )?;
                tx.execute(
                "INSERT INTO plans(entry,inputs,topology,codegen,manifest) VALUES(?1,x'',?2,?3,?4)",
                params![entry, plan.topology, plan.codegen, manifest],
            )?;
                tx.execute(
                    "INSERT INTO source_variants VALUES(?1,?2)",
                    params![entry, plan.sources],
                )?;
                if let Some(sample) = plan.sample {
                    tx.execute(
                        "INSERT INTO input_samples VALUES(?1,?2)",
                        params![entry, sample],
                    )?;
                }
                certify_dependents(&tx, entry, plan.codegen, plan.needs)?;
            }
            let time = now();
            let sweep = (packed.has_files || files.iter().any(|f| f.sealed.is_some()))
                && tx.execute(
                    "INSERT INTO maintenance VALUES('objects',?1) ON CONFLICT(name)
             DO UPDATE SET last_run=excluded.last_run WHERE last_run < ?2",
                    params![time, time - 3_600_000_000],
                )? == 1;
            let evicted = evict(&tx, limit)?;
            let kept = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM entries WHERE id=?1)",
                [entry],
                |r| r.get(0),
            )?;
            tx.commit()?;
            Ok((kept, evicted > 0, sweep))
        })();
        let (kept, evicted, sweep) = match publication {
            Ok(outcome) => outcome,
            Err(error) => {
                // A rolled-back writer may already have renamed files. Under
                // a fresh writer lock, remove only names still unreferenced.
                if packed.has_files || publishing_sealed {
                    let _ = self.collect_objects(true);
                }
                return Err(error);
            }
        };
        if evicted || sweep {
            self.collect_objects(sweep)?;
        }
        Ok(kept)
    }

    pub fn materialize(&mut self, key: &Digest, outputs: &[PathBuf]) -> Result<Option<Hit>> {
        let tx = self.conn.transaction()?;
        let entry = tx.query_row("SELECT id,stderr,stderr_hash,compile_ms FROM entries WHERE key=?1 AND length(stderr)<=?2 AND NOT EXISTS(SELECT 1 FROM needs WHERE needs.entry=id)",
            params![key, DIAGNOSTIC_LIMIT], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?, r.get::<_, Digest>(2)?, r.get::<_, u64>(3)?))).optional()?;
        let Some((id, stderr, hash, ms)) = entry else {
            return Ok(None);
        };
        let staged = (|| -> Result<Vec<Output>> {
            ensure!(blake3::hash(&stderr).as_bytes() == &hash, Damaged::Entry);
            let mut files = tx.prepare_cached(
                "SELECT f.slot,f.digest,f.size,f.mode,f.single,o.verity,c.raw,
                        CASE WHEN length(c.data)<=?2 THEN c.data END,c.external,o.hash,o.bytes,
                        CASE WHEN length(d.data)<=?3 THEN d.data END
                 FROM files f LEFT JOIN clones x ON x.entry=f.entry AND x.slot=f.slot
                 LEFT JOIN patches d ON d.entry=x.entry AND d.slot=x.slot
                 LEFT JOIN objects o ON o.hash=x.hash
                 LEFT JOIN chunks c ON f.single=1 AND c.hash=f.digest
                 WHERE f.entry=?1 ORDER BY f.slot",
            )?;
            let mut records = files.query(params![id, CHUNK, crate::sealed::delta::LIMIT])?;
            let mut staged = crate::staging::for_outputs(outputs)?;
            let mut decoder = Decoder::default();
            let mut frame = Vec::new();
            for (slot, staged_file) in staged.iter_mut().enumerate() {
                let tmp = staged_file.writer();
                let row = records.next()?.ok_or(Damaged::Entry)?;
                ensure!(row.get::<_, usize>(0)? == slot, Damaged::Entry);
                let digest: Digest = row.get(1)?;
                let size: u64 = row.get(2)?;
                let mode: u32 = row.get(3)?;
                tmp.set_permissions(fs::Permissions::from_mode(mode & 0o777))?;
                if row.get::<_, bool>(4)? {
                    let raw = row.get::<_, usize>(6).map_err(|_| Damaged::Chunk(digest))?;
                    let packed = row
                        .get_ref(7)?
                        .as_blob()
                        .map_err(|_| Damaged::Chunk(digest))?;
                    let external = row.get::<_, usize>(8).map_err(|_| Damaged::Chunk(digest))?;
                    let packed =
                        packed::payload(&self.dir, &digest, raw, packed, external, &mut frame)?;
                    let bytes = decoder.decode(packed, raw, digest)?;
                    ensure!(
                        blake3::hash(bytes).as_bytes() == &digest,
                        Damaged::Chunk(digest)
                    );
                    ensure!(size == raw as u64, Damaged::Entry);
                    if packed::write_output(tmp, bytes)? {
                        tmp.set_len(size)?;
                    }
                    continue;
                }
                let verity = row.get::<_, Option<Digest>>(5)?;
                if let Some(verity) = verity {
                    let base: Digest = row.get(9)?;
                    let original_size: u64 = row.get(10)?;
                    let path = crate::sealed::path(&self.dir, &base);
                    let mut source =
                        crate::content::open_with_flags(&path, libc::O_NONBLOCK | libc::O_NOFOLLOW)
                            .map_err(|e| {
                                if e.kind() == std::io::ErrorKind::NotFound
                                    || e.raw_os_error() == Some(libc::ELOOP)
                                {
                                    anyhow::Error::new(Damaged::Object(base))
                                } else {
                                    e.into()
                                }
                            })?;
                    if base == digest {
                        ensure!(
                            original_size == size
                                && matches!(row.get_ref(11)?, rusqlite::types::ValueRef::Null),
                            Damaged::Entry
                        );
                        crate::sealed::restore(&mut source, tmp, &verity, &base, size)?;
                    } else {
                        let patch = row.get_ref(11)?.as_blob().map_err(|_| Damaged::Entry)?;
                        if crate::sealed::delta::streams(patch)
                            && crate::sealed::measure(&source).ok() == Some(verity)
                        {
                            ensure!(
                                source.metadata()?.len() == original_size,
                                Damaged::Object(base)
                            );
                        } else {
                            crate::sealed::restore(
                                &mut source,
                                tmp,
                                &verity,
                                &base,
                                original_size,
                            )?;
                        }
                        crate::sealed::delta::apply(
                            &source,
                            tmp,
                            size,
                            original_size,
                            patch,
                            &digest,
                            &base,
                        )?;
                    }
                    continue;
                }
                let mut hasher = blake3::Hasher::new();
                let mut written = 0u64;
                let mut trailing_hole = false;
                let mut stmt = tx.prepare_cached("SELECT c.hash,c.raw,CASE WHEN length(c.data)<=262144 THEN c.data END,c.external FROM parts p JOIN chunks c ON c.hash=p.chunk WHERE entry=?1 AND slot=?2 ORDER BY ordinal")?;
                let mut rows = stmt.query(params![id, slot])?;
                while let Some(row) = rows.next()? {
                    let hash: Digest = row.get(0)?;
                    let raw: usize = row.get(1)?;
                    let packed = row
                        .get_ref(2)?
                        .as_blob()
                        .map_err(|_| Damaged::Chunk(hash))?;
                    let external = row.get::<_, usize>(3).map_err(|_| Damaged::Chunk(hash))?;
                    let packed =
                        packed::payload(&self.dir, &hash, raw, packed, external, &mut frame)?;
                    let bytes = decoder.decode(packed, raw, hash)?;
                    trailing_hole = packed::write_output(tmp, bytes)?;
                    hasher.update(bytes);
                    written += raw as u64;
                }
                if trailing_hole {
                    tmp.set_len(written)?;
                }
                if written != size || hasher.finalize().as_bytes() != &digest {
                    // Normal restores hash each decoded byte once. Only a failed
                    // file digest needs a second pass to identify a bad shared chunk.
                    tmp.rewind()?;
                    let mut verify = tx.prepare_cached("SELECT p.chunk,c.raw FROM parts p JOIN chunks c ON c.hash=p.chunk WHERE entry=?1 AND slot=?2 ORDER BY ordinal")?;
                    let mut rows = verify.query(params![id, slot])?;
                    while let Some(row) = rows.next()? {
                        let (chunk, raw) = (row.get::<_, Digest>(0)?, row.get::<_, usize>(1)?);
                        ensure!(raw <= CHUNK, Damaged::Chunk(chunk));
                        decoder.buffer.resize(raw, 0);
                        tmp.read_exact(&mut decoder.buffer)?;
                        ensure!(
                            blake3::hash(&decoder.buffer).as_bytes() == &chunk,
                            Damaged::Chunk(chunk)
                        );
                    }
                    return Err(Damaged::Entry.into());
                }
            }
            ensure!(records.next()?.is_none(), Damaged::Entry);
            Ok(staged)
        })();
        if let Err(error) = staged {
            if let Some(damage) = error.downcast_ref::<Damaged>() {
                let version = tx.query_row("PRAGMA data_version", [], |r| r.get(0))?;
                tx.commit()?;
                self.invalidate(key, damage, version)?;
            }
            return Err(error);
        }
        let staged = staged?;
        tx.commit()?;
        for (tmp, path) in staged.into_iter().zip(outputs) {
            tmp.publish(path)?;
        }
        Ok(Some(Hit {
            diagnostics: stderr,
            key: *key,
            compiler_ms: ms,
        }))
    }

    fn invalidate(&mut self, key: &Digest, damage: &Damaged, version: u32) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // A WAL reader can still hold the old entry while GC unlinks its
        // files. Never remove a healthy replacement published in the meantime.
        if tx.query_row("PRAGMA data_version", [], |r| r.get::<_, u32>(0))? != version {
            return Ok(());
        }
        match damage {
            Damaged::Chunk(hash) => tx.execute("DELETE FROM entries WHERE id IN (SELECT entry FROM parts WHERE chunk=?1 UNION SELECT entry FROM files WHERE single=1 AND digest=?1)",[hash])?,
            Damaged::Object(hash) => tx.execute("DELETE FROM entries WHERE id IN (SELECT entry FROM clones WHERE hash=?1)",[hash])?,
            Damaged::Entry => tx.execute("DELETE FROM entries WHERE key=?1", [key])?,
        };
        tx.execute("INSERT INTO counters VALUES('corrupt',1) ON CONFLICT(name) DO UPDATE SET value=value+1",[])?;
        tx.commit()?;
        self.collect_objects(false)
    }

    pub fn record_hit(&mut self, hit: &Hit, extra: Option<&str>) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE entries SET access=?1 WHERE key=?2",
            params![now(), hit.key],
        )?;
        tx.execute("INSERT INTO counters VALUES ('hit',1),('reused_compiler_ms',?1) ON CONFLICT(name) DO UPDATE SET value=value+excluded.value", [hit.compiler_ms])?;
        if let Some(event) = extra {
            tx.execute(
                "INSERT INTO counters VALUES(?1,1) ON CONFLICT(name) DO UPDATE SET value=value+1",
                [event],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn prune(&mut self, requested: Option<u64>) -> Result<u64> {
        crate::staging::clean(&self.dir)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let limit = match requested {
            Some(limit) => limit,
            None => stored_limit(&tx)?.unwrap_or(self.limit),
        };
        let removed = evict(&tx, limit)?;
        tx.commit()?;
        self.collect_objects(true)?;
        let _ = self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
        Ok(removed)
    }

    fn collect_objects(&mut self, sweep: bool) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        objects::Kind::Sealed.collect(&self.dir, &tx, sweep)?;
        objects::Kind::Frame.collect(&self.dir, &tx, sweep)?;
        tx.commit()?;
        Ok(())
    }

    pub fn stats(&self) -> Result<Stats> {
        let conn = self.conn.unchecked_transaction()?;
        let scalar = |q| -> rusqlite::Result<u64> { conn.query_row(q, [], |r| r.get(0)) };
        let mut events = conn.prepare("SELECT name,value FROM counters ORDER BY name")?;
        let database_bytes = [
            "artifacts-v1.sqlite3",
            "artifacts-v1.sqlite3-wal",
            "artifacts-v1.sqlite3-shm",
        ]
        .iter()
        .filter_map(|n| fs::metadata(self.dir.join(n)).ok())
        .map(|m| m.len())
        .sum();
        Ok(Stats {
            directory: self.dir.clone(),
            limit_bytes: stored_limit(&conn)?.unwrap_or(self.limit),
            charged_bytes: scalar("SELECT bytes FROM usage")?,
            database_bytes,
            entries: scalar("SELECT COUNT(*) FROM entries")?,
            chunks: scalar("SELECT COUNT(*) FROM chunks")?,
            pending_entries: scalar("SELECT COUNT(DISTINCT entry) FROM needs")?,
            probe_entries: scalar("SELECT COUNT(*) FROM probes")?,
            sealed_objects: scalar("SELECT COUNT(*) FROM objects")?,
            sealed_bytes: scalar("SELECT COALESCE(SUM(bytes),0) FROM objects")?,
            compressed_bytes: scalar("SELECT COALESCE(SUM(length(data)+external),0) FROM chunks")?,
            frame_file_bytes: scalar("SELECT COALESCE(SUM(external),0) FROM chunks")?,
            patched_files: scalar("SELECT COUNT(*) FROM patches")?,
            patch_bytes: scalar("SELECT COALESCE(SUM(length(data)),0) FROM patches")?,
            artifact_bytes: scalar("SELECT COALESCE(SUM(size),0) FROM files")?,
            counters: events
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?,
        })
    }
}

fn stored_limit(conn: &Connection) -> rusqlite::Result<Option<u64>> {
    conn.prepare_cached("SELECT max_bytes FROM cache_settings WHERE singleton=1")?
        .query_row([], |r| r.get(0))
        .optional()
}

fn probe_checksum(stdout: &[u8], stderr: &[u8]) -> Digest {
    *blake3::Hasher::new()
        .update(&(stdout.len() as u64).to_le_bytes())
        .update(stdout)
        .update(stderr)
        .finalize()
        .as_bytes()
}

// Pipelined Cargo consumers can finish before their producers. A verified
// output can discharge only requirements for those exact bytes and proof level.
fn certify_dependents(
    tx: &Transaction<'_>,
    entry: i64,
    codegen: bool,
    needs: &[Digest],
) -> Result<()> {
    let mut pending = false;
    for digest in needs {
        // A producer can commit between hashing the input and acquiring this
        // writer. Exclude the entry under construction from its own evidence.
        pending |= tx
            .prepare_cached(
                "INSERT OR IGNORE INTO needs SELECT ?1,?2,?3 WHERE NOT EXISTS (
             SELECT 1 FROM files f JOIN plans p ON p.entry=f.entry
             WHERE f.digest=?2 AND f.entry!=?1 AND p.codegen>=?3
             AND NOT EXISTS(SELECT 1 FROM needs n WHERE n.entry=f.entry))",
            )?
            .execute(params![entry, digest, codegen])?
            != 0;
    }
    if pending
        || !tx
            .prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM files f CROSS JOIN needs n
         WHERE f.entry=?1 AND n.digest=f.digest AND n.codegen<=?2)",
            )?
            .query_row(params![entry, codegen], |r| r.get::<_, bool>(0))?
    {
        return Ok(());
    }

    // The queue lives in SQLite's temporary database so wide fan-out does not
    // require a process-sized Rust collection. Pop before expanding a node.
    tx.execute_batch("CREATE TEMP TABLE IF NOT EXISTS proof_queue(entry INTEGER PRIMARY KEY)")?;
    tx.execute("INSERT INTO temp.proof_queue VALUES(?1)", [entry])?;
    let mut next = tx.prepare_cached(
        "SELECT q.entry,p.codegen,NOT EXISTS(SELECT 1 FROM needs n WHERE n.entry=q.entry)
         FROM temp.proof_queue q CROSS JOIN plans p WHERE p.entry=q.entry LIMIT 1",
    )?;
    let mut pop = tx.prepare_cached("DELETE FROM temp.proof_queue WHERE entry=?1")?;
    let mut enqueue = tx.prepare_cached(
        "INSERT OR IGNORE INTO temp.proof_queue SELECT n.entry FROM files f CROSS JOIN needs n
         WHERE f.entry=?1 AND n.digest=f.digest AND n.codegen<=?2",
    )?;
    let mut release = tx.prepare_cached(
        "DELETE FROM needs WHERE digest IN (SELECT digest FROM files WHERE entry=?1) AND codegen<=?2",
    )?;
    while let Some((id, strength, ready)) = next
        .query_row([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, bool>(1)?,
                r.get::<_, bool>(2)?,
            ))
        })
        .optional()?
    {
        pop.execute([id])?;
        if ready {
            enqueue.execute(params![id, strength])?;
            release.execute(params![id, strength])?;
        }
    }
    Ok(())
}

fn evict(tx: &Transaction<'_>, limit: u64) -> Result<u64> {
    let mut removed = 0;
    while tx.query_row("SELECT bytes FROM usage", [], |r| r.get::<_, u64>(0))? > limit {
        if tx.execute(
            "DELETE FROM probes WHERE key=(SELECT key FROM probes ORDER BY seen LIMIT 1)",
            [],
        )? != 0
        {
            continue;
        }
        if tx.execute(
            "DELETE FROM entries WHERE id=(SELECT n.entry FROM needs n JOIN entries e
             ON e.id=n.entry ORDER BY e.access LIMIT 1)",
            [],
        )? != 0
        {
            removed += 1;
            continue;
        }
        let n = tx.execute(
            "DELETE FROM entries WHERE id=(SELECT id FROM entries ORDER BY access,id LIMIT 1)",
            [],
        )?;
        ensure!(n == 1, "inconsistent cache accounting");
        removed += 1;
    }
    if removed != 0 {
        tx.execute("INSERT INTO counters VALUES('evicted',?1) ON CONFLICT(name) DO UPDATE SET value=value+excluded.value", [removed])?;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Store {
        fn restore(&mut self, key: &Digest, paths: &[PathBuf]) -> Result<Option<Vec<u8>>> {
            let Some(hit) = self.materialize(key, paths)? else {
                return Ok(None);
            };
            self.record_hit(&hit, None)?;
            Ok(Some(hit.diagnostics))
        }
    }

    #[test]
    fn large_input_samples_respect_legacy_entries_budget_and_eviction() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let files = [dir.path().join("file")];
        fs::write(&files[0], b"output")?;
        let mut store = Store::at(dir.path().join("cache"), 1 << 20)?;
        let recipe = [7; 32];
        let evidence = |sample| Evidence {
            sample,
            inputs: b"plan",
            sources: &[1; 32],
            needs: &[],
            topology: &[0; 32],
            codegen: true,
        };
        let first = [8; 32];
        let other = [9; 32];
        assert!(store.save(&recipe, &first, &files, b"", 5, Some(evidence(None)))?);
        let unsampled = store.stats()?.charged_bytes;
        assert!(store.may_match_sample(&recipe, &other)?);
        store.prune(Some(0))?;
        store.limit = unsampled;
        assert!(!store.save(&recipe, &first, &files, b"", 5, Some(evidence(Some(first))))?);
        store.limit += 128;
        assert!(store.save(&recipe, &first, &files, b"", 5, Some(evidence(Some(first))))?);
        assert_eq!(store.stats()?.charged_bytes, unsampled + 128);
        assert!(store.may_match_sample(&recipe, &first)?);
        assert!(!store.may_match_sample(&recipe, &other)?);
        store.limit = 1 << 20;
        assert!(store.save(&recipe, &other, &files, b"", 5, Some(evidence(None)))?);
        assert!(store.may_match_sample(&recipe, &other)?);
        store.prune(Some(0))?;
        assert_eq!(store.stats()?.charged_bytes, 0);
        assert!(!store.may_match_sample(&recipe, &first)?);
        Ok(())
    }

    #[test]
    fn extra_topologies_are_bounded_charged_and_evicted_with_the_entry() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let files = [dir.path().join("file")];
        fs::write(&files[0], b"output")?;
        let mut store = Store::at(dir.path().join("cache"), 1 << 20)?;
        let key = [8; 32];
        let sources = [9; 32];
        let ev = Evidence {
            sample: None,
            inputs: b"manifest",
            sources: &sources,
            needs: &[],
            topology: &[0; 32],
            codegen: true,
        };
        assert!(store.save(&[7; 32], &key, &files, b"", 1, Some(ev))?);
        let base = store.stats()?.charged_bytes;
        store.limit = base;
        store.refresh_topology(&key, &[1; 32], true)?;
        assert_eq!(store.matches_topology(&key, &[1; 32])?, Some(false));
        store.limit += 192;
        for n in 1..=7 {
            store.refresh_topology(&key, &[n; 32], true)?;
            assert!(store.has_source_topology(&sources, &[n; 32])?);
            assert_eq!(store.stats()?.charged_bytes, base + 192);
        }
        for n in 0..=7 {
            assert_eq!(
                store.matches_topology(&key, &[n; 32])?,
                Some(n == 0 || n >= 5)
            );
        }
        store.prune(Some(0))?;
        assert_eq!(store.stats()?.charged_bytes, 0);
        assert_eq!(store.matches_topology(&key, &[7; 32])?, None);
        Ok(())
    }

    #[test]
    fn native_preflight_size_cannot_authorize_bytes_or_replaced_paths() -> Result<()> {
        use crate::{fingerprint, invocation::Invocation};
        let dir = tempfile::tempdir()?;
        let extern_file = dir.path().join("libexample.rlib");
        fs::write(&extern_file, b"valid")?;
        let mut store = Store::at(dir.path().join("cache"), 1 << 20)?;
        let ev = Evidence {
            sample: None,
            inputs: b"manifest",
            sources: &[2; 32],
            needs: &[],
            topology: &[4; 32],
            codegen: true,
        };
        assert!(store.save(
            &[5; 32],
            &[1; 32],
            std::slice::from_ref(&extern_file),
            b"",
            1,
            Some(ev)
        )?);
        let inv = Invocation {
            source_arg: 0,
            mold: false,
            name: "unit".into(),
            codegen: true,
            executable: true,
            observe_native: true,
            linker: None,
            incremental: None,
            live_incremental: false,
            deferred_links: false,
            source: dir.path().join("main.rs"),
            externs: vec![extern_file.clone()],
            outputs: vec![],
            probe_args: vec![],
        };
        let since = SystemTime::now();
        let pre = fingerprint::native_externs(&inv, since, &store)?;
        assert!(fingerprint::certify_externs(&pre, true, since, &store, false).is_ok());
        fs::write(dir.path().join("other"), b"valid")?;
        fs::rename(dir.path().join("other"), &extern_file)?;
        assert!(fingerprint::certify_externs(&pre, true, since, &store, false).is_err());
        fs::write(&extern_file, b"wrong")?;
        let since = SystemTime::now();
        let pre = fingerprint::native_externs(&inv, since, &store)?;
        assert!(fingerprint::certify_externs(&pre, true, since, &store, false).is_err());
        // A prior full producer certification cannot be inferred from matching
        // samples, even for an edit buried in a same-length rlib.
        use std::os::unix::fs::FileExt;
        fs::write(&inv.source, b"fn main() {}")?;
        fs::write(&extern_file, vec![7; 5 << 20])?;
        let plan: fingerprint::Plan =
            serde_json::from_str(r#"{"files":[],"env":[],"large_extern":0}"#)?;
        let recipe = fingerprint::Recipe {
            key: [7; 32],
            toolchain: [9; 32],
            root: dir.path().to_owned(),
        };
        let hint = plan.sample(&recipe, &inv, SystemTime::now())?.unwrap();
        let evidence = Evidence {
            sample: None,
            inputs: b"proof",
            needs: &[],
            sources: &[3; 32],
            topology: &[4; 32],
            codegen: true,
        };
        store.save(
            &[5; 32],
            &[4; 32],
            std::slice::from_ref(&extern_file),
            b"",
            1,
            Some(evidence),
        )?;
        assert!(fingerprint::externs(&inv, SystemTime::now(), &store, false).is_ok());
        File::options()
            .write(true)
            .open(&extern_file)?
            .write_all_at(&[22], 123456)?;
        assert_eq!(plan.sample(&recipe, &inv, SystemTime::now())?, Some(hint));
        assert!(fingerprint::externs(&inv, SystemTime::now(), &store, false).is_err());
        Ok(())
    }

    #[test]
    fn shared_frame_files_recover_from_missing_and_corrupt_payloads() -> Result<()> {
        for missing in [false, true] {
            let d = tempfile::tempdir()?;
            let mut store = Store::at(d.path().join("cache"), 4 << 20)?;
            let path = d.path().join("out");
            let mut half = vec![0; 80 << 10];
            blake3::Hasher::new()
                .update(b"compressed frame file")
                .finalize_xof()
                .fill(&mut half);
            let data = half.repeat(2);
            fs::write(&path, &data)?;
            let paths = std::slice::from_ref(&path);
            for n in [1, 2] {
                assert!(store.save(&[0; 32], &[n; 32], paths, b"", 1, None)?);
            }
            let hash = *blake3::hash(&data).as_bytes();
            let object = objects::Kind::Frame.path(&store.dir, &hash);
            let encoded = object.metadata()?.len();
            let stats = store.stats()?;
            assert!(encoded < data.len() as u64 && encoded >= 64 << 10);
            assert_eq!(stats.frame_file_bytes, encoded);
            fs::remove_file(&path)?;
            assert!(store.restore(&[2; 32], paths)?.is_some());
            assert_eq!(fs::read(&path)?, data);
            if missing {
                fs::remove_file(&object)?;
            } else {
                fs::write(&object, vec![0; encoded as usize])?;
            }
            fs::write(&path, b"working copy")?;
            assert!(store.restore(&[1; 32], paths).is_err());
            assert_eq!(fs::read(path)?, b"working copy");
            assert_eq!(store.stats()?.charged_bytes, 0);
            assert!(!object.exists());
            let abandoned = objects::Kind::Frame.path(&store.dir, &[17; 32]);
            fs::create_dir_all(abandoned.parent().unwrap())?;
            fs::write(&abandoned, b"interrupted")?;
            let pending = store.dir.join("frames-v1/.pending");
            fs::write(&pending, b"unfinished")?;
            let unrelated = abandoned.with_file_name("unrelated");
            fs::write(&unrelated, b"keep")?;
            store.prune(Some(0))?;
            assert!(!abandoned.exists() && !pending.exists() && unrelated.exists());
        }
        Ok(())
    }

    #[test]
    fn redirected_frame_directories_do_not_receive_writes_or_cleanup() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut store = Store::at(dir.path().join("cache"), 4 << 20)?;
        let other = dir.path().join("unrelated");
        fs::create_dir(&other)?;
        fs::write(other.join(".pending"), b"keep")?;
        let mut bytes = vec![0; CHUNK];
        blake3::Hasher::new()
            .update(b"unrelated")
            .finalize_xof()
            .fill(&mut bytes);
        let hash = *blake3::hash(&bytes).as_bytes();
        let managed = store.dir.join("frames-v1");
        let names = objects::Kind::Frame.path(&store.dir, &hash);
        let outside = other.join(names.strip_prefix(&managed)?);
        fs::create_dir(outside.parent().unwrap())?;
        fs::write(&outside, b"keep")?;
        std::os::unix::fs::symlink(&other, &managed)?;
        let input = dir.path().join("compiler-output");
        fs::write(&input, bytes)?;
        assert!(
            store
                .save(&[2; 32], &[3; 32], &[input], b"", 1, None)
                .is_err()
        );
        assert!(store.prune(Some(0)).is_err());
        assert_eq!(fs::read(other.join(".pending"))?, b"keep");
        assert_eq!(fs::read(outside)?, b"keep");
        Ok(())
    }

    #[test]
    fn rolled_back_publication_leaves_existing_frames_and_no_new_files() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut store = Store::at(dir.path().join("cache"), 8 << 20)?;
        let paths = [dir.path().join("rlib")];
        let mut data = vec![0; CHUNK];
        blake3::Hasher::new()
            .update(b"rollback")
            .finalize_xof()
            .fill(&mut data);
        fs::write(&paths[0], &data)?;
        store.save(&[0; 32], &[1; 32], &paths, b"", 1, None)?;
        let original = *blake3::hash(&data).as_bytes();
        let charged = store.stats()?.charged_bytes;
        data[917] ^= 1;
        fs::write(&paths[0], &data)?;
        let other = *blake3::hash(&data).as_bytes();
        store.conn.execute_batch("CREATE TRIGGER interrupted BEFORE INSERT ON plans BEGIN SELECT RAISE(ABORT,'interrupted'); END")?;
        let plan = Evidence {
            sample: None,
            inputs: b"plan",
            sources: &[3; 32],
            needs: &[],
            topology: &[0; 32],
            codegen: true,
        };
        assert!(
            store
                .save(&[0; 32], &[2; 32], &paths, b"", 1, Some(plan))
                .is_err()
        );
        assert!(objects::Kind::Frame.path(&store.dir, &original).is_file());
        assert!(!objects::Kind::Frame.path(&store.dir, &other).exists());
        assert_eq!(store.stats()?.charged_bytes, charged);
        assert!(store.restore(&[1; 32], &paths)?.is_some());
        Ok(())
    }

    #[test]
    fn a_stale_reader_cannot_remove_a_newly_published_frame() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let cache = dir.path().join("cache");
        let paths = [dir.path().join("output")];
        let mut raw = vec![0; CHUNK];
        blake3::Hasher::new()
            .update(b"concurrent")
            .finalize_xof()
            .fill(&mut raw);
        fs::write(&paths[0], &raw)?;
        let mut first = Store::at(cache.clone(), 2 << 20)?;
        first.save(&[1; 32], &[2; 32], &paths, b"", 1, None)?;
        let tx = first.conn.transaction()?;
        let _: u64 = tx.query_row("SELECT count(*) FROM entries", [], |r| r.get(0))?;
        let mut second = Store::at(cache, 2 << 20)?;
        second.prune(Some(0))?;
        second.save(&[1; 32], &[2; 32], &paths, b"", 1, None)?;
        let version = tx.query_row("PRAGMA data_version", [], |r| r.get(0))?;
        tx.commit()?;
        first.invalidate(
            &[2; 32],
            &Damaged::Chunk(*blake3::hash(&raw).as_bytes()),
            version,
        )?;
        assert!(first.restore(&[2; 32], &paths)?.is_some());
        assert_eq!(fs::read(&paths[0])?, raw);
        Ok(())
    }

    #[test]
    fn sparse_packed_restores_preserve_middle_pages_and_trailing_length() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut store = Store::at(dir.path().join("cache"), 2 << 20)?;
        let path = dir.path().join("artifact");
        let paths = std::slice::from_ref(&path);
        for (key, size) in [(1, CHUNK - 73), (2, 13 * CHUNK + 317)] {
            let mut data = vec![0; size];
            data[19] = 81;
            data[size / 2 + 31] = 17;
            fs::write(&path, &data)?;
            store.save(&[0; 32], &[key; 32], paths, b"", 1, None)?;
            fs::write(&path, b"replace")?;
            assert!(store.restore(&[key; 32], paths)?.is_some());
            assert_eq!(fs::read(&path)?, data);
        }
        Ok(())
    }

    #[test]
    fn packed_archives_share_unaligned_metadata_and_shifted_members() -> Result<()> {
        fn member(out: &mut Vec<u8>, name: &str, data: &[u8]) {
            let header = format!(
                "{name:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                0,
                0,
                0,
                "100644",
                data.len()
            );
            assert_eq!(header.len(), 60);
            out.extend_from_slice(header.as_bytes());
            out.extend_from_slice(data);
            if data.len() % 2 == 1 {
                out.push(b'\n');
            }
        }
        let d = tempfile::tempdir()?;
        let files = [d.path().join("libx.rmeta"), d.path().join("libx.rlib")];
        let mut store = Store::at(d.path().join("store"), 16 << 20)?;
        let mut rng = 123u32;
        let random: Vec<_> = (0..CHUNK * 6 + 17)
            .map(|_| {
                rng ^= rng << 13;
                rng ^= rng >> 17;
                rng ^= rng << 5;
                rng as u8
            })
            .collect();
        let (meta, object) = random.split_at(CHUNK * 3 + 11);
        fs::write(&files[0], meta)?;
        let mut wrapped = vec![42; 73];
        wrapped.extend_from_slice(meta);
        wrapped.extend_from_slice(b"object footer");
        for k in [1u8, 2] {
            let mut archive = b"!<arch>\n".to_vec();
            member(&mut archive, "/", &vec![k; k as usize * 81]);
            member(&mut archive, "lib.rmeta/", &wrapped);
            member(&mut archive, "object.o/", object);
            fs::write(&files[1], &archive)?;
            assert!(store.save(&[1; 32], &[k; 32], &files, b"", 1, None)?);
            let stats = store.stats()?;
            assert!(
                stats.compressed_bytes < random.len() as u64 + 4096,
                "{stats:?}",
                stats = stats.compressed_bytes
            );
            fs::write(&files[1], b"replace")?;
            assert_eq!(store.restore(&[k; 32], &files)?, Some(vec![]));
            assert_eq!(fs::read(&files[1])?, archive);
            assert_eq!(fs::read(&files[0])?, meta);
        }
        store.prune(Some(0))?;
        assert_eq!(store.stats()?.charged_bytes, 0);
        Ok(())
    }

    #[test]
    fn packed_insertions_and_deletions_reuse_frames_and_restore_every_revision() -> Result<()> {
        let root = tempfile::tempdir()?;
        let file = root.path().join("asset");
        let outputs = std::slice::from_ref(&file);
        let mut store = Store::at(root.path().join("cache"), 32 << 20)?;
        let mut payload = vec![0; CHUNK * 24 + 19];
        blake3::Hasher::new()
            .update(b"shifted data")
            .finalize_xof()
            .fill(&mut payload);
        let variants = [(83, 0usize), (83, 19), (CHUNK + 7, 911), (83, 1)];
        let make = |(offset, n): (usize, usize)| {
            let mut bytes = Vec::with_capacity(payload.len() + n);
            bytes.extend_from_slice(&payload[..offset]);
            bytes.resize(bytes.len() + n, 42);
            bytes.extend_from_slice(&payload[offset..]);
            bytes
        };
        for (i, variant) in variants.into_iter().enumerate() {
            fs::write(&file, make(variant))?;
            assert!(store.save(&[8; 32], &[i as u8; 32], outputs, b"", 1, None)?);
        }
        let stats = store.stats()?;
        assert!(stats.compressed_bytes < (payload.len() * 2) as u64);
        assert!(stats.counters["packed_reused_bytes"] > (payload.len() * 2) as u64);
        for (i, variant) in variants.into_iter().enumerate() {
            assert!(store.restore(&[i as u8; 32], outputs)?.is_some());
            assert_eq!(fs::read(&file)?, make(variant));
        }
        store.prune(Some(0))?;
        assert_eq!(store.stats()?.charged_bytes, 0);
        Ok(())
    }

    #[test]
    fn packed_payloads_survive_both_reference_kinds_without_blob_updates() -> Result<()> {
        for remove_single_first in [true, false] {
            let root = tempfile::tempdir()?;
            let dir = root.path().join("cache");
            let path = root.path().join("output");
            let files = std::slice::from_ref(&path);
            let raw = vec![11; CHUNK];
            let mut store = Store::at(dir.clone(), 4 << 20)?;
            fs::write(&path, &raw)?;
            assert!(store.save(&[0; 32], &[1; 32], files, b"", 1, None)?);
            store.conn.execute_batch("CREATE TRIGGER reject_payload_update BEFORE UPDATE ON chunks BEGIN SELECT RAISE(ABORT,'immutable chunk updated'); END;")?;
            let double = raw.repeat(2);
            fs::write(&path, &double)?;
            assert!(store.save(&[0; 32], &[2; 32], files, b"", 1, None)?);
            // The migration replaces reclamation triggers without reading or
            // rewriting payloads. A v10 reader could still have written refs.
            store.conn.execute_batch("PRAGMA user_version=10")?;
            drop(store);
            let mut store = Store::at(dir, 4 << 20)?;
            let (evicted, kept, expected) = if remove_single_first {
                (1u8, 2u8, &double)
            } else {
                (2u8, 1u8, &raw)
            };
            store
                .conn
                .execute("DELETE FROM entries WHERE key=?1", [[evicted; 32]])?;
            assert_eq!(store.stats()?.chunks, 1);
            assert!(store.restore(&[kept; 32], files)?.is_some());
            assert_eq!(&fs::read(&path)?, expected);
            store.prune(Some(0))?;
            assert_eq!(store.stats()?.charged_bytes, 0);
        }
        Ok(())
    }

    #[test]
    #[ignore = "streams two 272 MiB artifacts for packed I/O qualification"]
    fn large_packed_revision_spills_and_restores_exact_bytes() -> Result<()> {
        use std::os::unix::fs::FileExt;
        const FRAMES: u64 = 1088;
        let root = tempfile::tempdir()?;
        let path = root.path().join("large");
        let mut store = Store::at(root.path().join("cache"), 8 << 20)?;
        let file = File::create(&path)?;
        file.set_len(FRAMES * CHUNK as u64)?;
        for i in 0..FRAMES {
            file.write_all_at(&i.to_le_bytes(), i * CHUNK as u64)?;
        }
        let mut hasher = blake3::Hasher::new();
        hasher.update_reader(File::open(&path)?)?;
        let expected = hasher.finalize();
        let outputs = std::slice::from_ref(&path);
        assert!(store.save(&[0; 32], &[1; 32], outputs, b"", 1, None)?);
        file.write_all_at(b"revision", 7)?;
        assert!(store.save(&[0; 32], &[2; 32], outputs, b"", 1, None)?);
        drop(file);
        assert_eq!(
            store.stats()?.counters["packed_reused_bytes"],
            (FRAMES - 1) * CHUNK as u64
        );
        assert_eq!(store.stats()?.chunks, FRAMES + 1);
        assert!(store.restore(&[1; 32], outputs)?.is_some());
        hasher.reset().update_reader(File::open(&path)?)?;
        assert_eq!(hasher.finalize(), expected);
        store.prune(Some(0))?;
        assert_eq!(store.stats()?.charged_bytes, 0);
        Ok(())
    }

    #[test]
    fn variants_share_checked_manifests_and_single_chunk_payloads() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut store = Store::at(root.path().join("cache"), 1024 * 1024)?;
        let outputs = [root.path().join("metadata")];
        fs::write(&outputs[0], b"shared output")?;
        let inputs = b"repeated/source/path.rs ".repeat(1000);
        let evidence = || Evidence {
            sample: None,
            inputs: &inputs,
            sources: &[6; 32],
            needs: &[],
            topology: &[2; 32],
            codegen: false,
        };
        for i in 1..=12 {
            assert!(store.save(&[8; 32], &[i; 32], &outputs, b"", 1, Some(evidence()))?);
        }
        let (refs, bytes): (usize, usize) =
            store
                .conn
                .query_row("SELECT refs,length(data) FROM manifests", [], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?;
        assert_eq!(refs, 12);
        assert!(bytes < inputs.len() / 10);
        assert_eq!(
            store
                .conn
                .query_row("SELECT COUNT(*) FROM parts", [], |r| r.get::<_, u64>(0))?,
            0
        );
        assert_eq!(
            store.plans(&[8; 32])?.0.as_slice(),
            std::slice::from_ref(&inputs)
        );
        store
            .conn
            .execute("UPDATE manifests SET data=zeroblob(length(data))", [])?;
        assert!(store.plans(&[8; 32])?.0.is_empty());
        assert_eq!(store.stats()?.charged_bytes, 0);
        assert!(store.save(&[8; 32], &[1; 32], &outputs, b"", 1, Some(evidence()))?);
        assert_eq!(store.plans(&[8; 32])?.0, [inputs]);
        assert_eq!(store.restore(&[1; 32], &outputs)?, Some(vec![]));
        store.prune(Some(0))?;
        assert_eq!(store.stats()?.charged_bytes, 0);
        Ok(())
    }

    #[test]
    fn chunks_are_shared_lru_is_bounded_and_corruption_does_not_escape() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut store = Store::at(root.path().join("cache"), 2 << 20)?;
        let output = root.path().join("out.rlib");
        let files = vec![output.clone(), root.path().join("zero.rmeta")];
        fs::write(&files[1], [])?;
        fs::write(&output, vec![7; CHUNK * 3])?;
        for k in [1, 2] {
            assert!(store.save(&[0; 32], &[k; 32], &files, b"warning\n", 33, None)?);
        }
        assert_eq!(store.stats()?.chunks, 1);
        fs::write(&output, b"different")?;
        assert_eq!(
            store.restore(&[1; 32], &files)?,
            Some(b"warning\n".to_vec())
        );
        assert_eq!(fs::read(&output)?, vec![7; CHUNK * 3]);
        let size = store.stats()?.charged_bytes;
        assert_eq!(store.prune(Some(size - 1))?, 1);
        assert!(store.restore(&[2; 32], &files)?.is_none());
        fs::write(&output, b"old")?;
        store
            .conn
            .execute("UPDATE chunks SET data=zeroblob(length(data))", [])?;
        assert!(store.restore(&[1; 32], &files).is_err());
        assert_eq!(fs::read(output)?, b"old");
        assert_eq!(store.stats()?.charged_bytes, 0);
        Ok(())
    }

    #[test]
    fn check_metadata_cannot_certify_codegen_and_destination_failure_keeps_data() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut store = Store::at(root.path().join("cache"), 1 << 20)?;
        let output = root.path().join("lib.rmeta");
        fs::write(&output, b"metadata")?;
        let proof = Evidence {
            sample: None,
            inputs: b"{}",
            sources: &[7; 32],
            needs: &[],
            topology: &[0; 32],
            codegen: false,
        };
        store.save(
            &[0; 32],
            &[1; 32],
            std::slice::from_ref(&output),
            &[],
            0,
            Some(proof),
        )?;
        let digest = *blake3::hash(b"metadata").as_bytes();
        assert!(store.knows_artifact(&digest, false)?);
        assert!(!store.knows_artifact(&digest, true)?);
        let impossible = output.join("not-a-directory");
        assert!(store.restore(&[1; 32], &[impossible]).is_err());
        assert_eq!(store.stats()?.entries, 1);
        assert!(store.restore(&[1; 32], &[output])?.is_some());
        Ok(())
    }

    #[test]
    fn pipelined_records_cannot_vouch_for_each_other_without_proof() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut store = Store::at(root.path().join("cache"), 1 << 20)?;
        let file = root.path().join("output");
        let save =
            |store: &mut Store, id: u8, bytes: &[u8], needs: &[Digest], codegen| -> Result<()> {
                fs::write(&file, bytes)?;
                let proof = Evidence {
                    sample: None,
                    inputs: b"{}",
                    sources: &[7; 32],
                    topology: &[1; 32],
                    needs,
                    codegen,
                };
                store.save(
                    &[id; 32],
                    &[id; 32],
                    std::slice::from_ref(&file),
                    &[],
                    0,
                    Some(proof),
                )?;
                Ok(())
            };
        let a = *blake3::hash(b"producer").as_bytes();
        let b = *blake3::hash(b"downstream").as_bytes();
        save(&mut store, 1, b"downstream", &[a], true)?;
        save(&mut store, 2, b"leaf", &[b], true)?;
        // Carry outstanding 0.8 evidence through the index and policy migration.
        store.conn.execute_batch(
            "DROP INDEX waiting_for_artifact; DROP TABLE cache_settings; PRAGMA user_version=15;",
        )?;
        drop(store);
        let mut store = Store::at(root.path().join("cache"), 1 << 20)?;
        assert!(store.plans(&[2; 32])?.0.is_empty());
        assert!(!store.knows_artifact(&b, true)?);
        assert!(
            store
                .restore(&[1; 32], std::slice::from_ref(&file))?
                .is_none()
        );
        save(&mut store, 3, b"producer", &[], false)?;
        assert!(store.plans(&[2; 32])?.0.is_empty());
        save(&mut store, 4, b"producer", &[], true)?;
        assert!(!store.plans(&[2; 32])?.0.is_empty());
        assert!(store.knows_artifact(&b, true)?);

        // The producer committed after the wrapper observed its input but
        // before it published. Its existing independent proof still counts.
        save(&mut store, 5, b"late", &[a, b], true)?;
        assert!(!store.plans(&[5; 32])?.0.is_empty());

        let left = *blake3::hash(b"left").as_bytes();
        let right = *blake3::hash(b"right").as_bytes();
        save(&mut store, 6, b"fan-in", &[left, right], true)?;
        save(&mut store, 7, b"left", &[right], true)?;
        save(&mut store, 8, b"right", &[left], true)?;
        save(&mut store, 9, b"left", &[left], true)?;
        for id in 6..=9 {
            assert!(store.plans(&[id; 32])?.0.is_empty());
        }
        save(&mut store, 10, b"right", &[], false)?;
        assert!(!store.knows_artifact(&left, true)?);
        save(&mut store, 11, b"right", &[], true)?;
        for id in 6..=9 {
            assert!(!store.plans(&[id; 32])?.0.is_empty());
        }
        Ok(())
    }

    #[test]
    fn already_open_publishers_use_the_current_shared_limit() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("cache");
        let mut old = Store::at(path.clone(), 0)?;
        let mut admin = Store::at(path, 1 << 20)?;
        let file = root.path().join("output");
        fs::write(&file, "previously compiled output")?;
        admin.set_budget(Some(1 << 20))?;
        assert!(old.save(
            &[1; 32],
            &[2; 32],
            std::slice::from_ref(&file),
            &[],
            1,
            None
        )?);
        old.prune(None)?;
        assert_eq!(old.stats()?.entries, 1);
        admin.set_budget(Some(0))?;
        assert!(!old.save(&[1; 32], &[3; 32], &[file], &[], 1, None)?);
        old.prune(None)?;
        assert_eq!(old.stats()?.entries, 0);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sealed_clones_are_writable_independent_and_collected() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut store = Store::at(root.path().join("cache"), 8 << 20)?;
        store.allow_sealed = None;
        let path = root.path().join("artifact");
        let original = vec![3; 2 * CHUNK];
        fs::write(&path, &original)?;
        let outputs = std::slice::from_ref(&path);
        for id in [1, 2] {
            store.save(&[0; 32], &[id; 32], outputs, &[], 0, None)?;
            store.limit = original.len() as u64 + 2048;
        }
        if store.stats()?.sealed_objects == 0 {
            ensure!(
                env::var_os("JATO_TEST_SEALED").is_none(),
                "expected kernel-sealed storage on this test filesystem"
            );
            return Ok(());
        }
        let object = crate::sealed::path(&store.dir, blake3::hash(&original).as_bytes());
        assert_eq!(store.stats()?.sealed_objects, 1);
        assert_eq!(store.stats()?.chunks, 0);
        assert_eq!(
            store.stats()?.counters["sealed_reused_bytes"],
            original.len() as u64
        );
        fs::write(&path, b"changed source")?;
        assert!(store.restore(&[1; 32], outputs)?.is_some());
        assert_eq!(fs::read(&path)?, original);
        fs::write(&path, b"changed restored target")?;
        assert_eq!(fs::read(&object)?, original);
        assert!(OpenOptions::new().write(true).open(&object).is_err());
        fs::remove_file(&object)?;
        fs::write(&object, &original)?; // plain copied cache still verifies
        assert!(store.restore(&[2; 32], outputs)?.is_some());
        fs::write(&object, vec![9; original.len()])?;
        fs::write(&path, b"untouched")?;
        assert!(store.restore(&[1; 32], outputs).is_err());
        assert_eq!(fs::read(&path)?, b"untouched");
        assert_eq!(store.stats()?.entries, 0);
        assert_eq!(store.stats()?.charged_bytes, 0);
        assert!(!object.exists());
        let orphan = crate::sealed::path(&store.dir, &[9; 32]);
        fs::create_dir_all(orphan.parent().unwrap())?;
        fs::write(&orphan, b"aborted publish")?;
        store.prune(Some(0))?;
        assert!(!orphan.exists());
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sealed_revisions_share_the_original_across_eviction_and_validate_every_restore() -> Result<()>
    {
        let d = tempfile::tempdir()?;
        let mut store = Store::at(d.path().join("cache"), 32 << 20)?;
        store.allow_sealed = None;
        let path = d.path().join("app");
        let paths = std::slice::from_ref(&path);
        let mut base = vec![0; 5 << 20];
        blake3::Hasher::new()
            .update(b"sealed variants")
            .finalize_xof()
            .fill(&mut base);
        let mut shifted = base.clone();
        shifted.splice(8194..8194, [4; 13]);
        let mut edited = base.clone();
        edited[183477] ^= 0xff;
        for (i, bytes) in [&base, &shifted, &edited].into_iter().enumerate() {
            fs::write(&path, bytes)?;
            assert!(store.save(&[1; 32], &[i as u8; 32], paths, b"", 1, None)?);
            if i == 0 && store.stats()?.sealed_objects == 0 {
                ensure!(
                    env::var_os("JATO_TEST_SEALED").is_none(),
                    "expected fs-verity"
                );
                return Ok(());
            }
        }
        let stats = store.stats()?;
        assert_eq!((stats.sealed_objects, stats.patched_files), (1, 2));
        assert!(stats.patch_bytes < 32 << 10);
        store
            .conn
            .execute("DELETE FROM entries WHERE key=?1", [[0; 32]])?;
        store.collect_objects(false)?;
        let object = crate::sealed::path(&store.dir, blake3::hash(&base).as_bytes());
        for copied in [false, true] {
            if copied {
                // An ordinary backup can preserve the base bytes while
                // dropping the kernel's fs-verity attribute.
                fs::remove_file(&object)?;
                fs::write(&object, &base)?;
            }
            for (i, bytes) in [(1, &shifted), (2, &edited)] {
                fs::write(&path, b"live target")?;
                assert!(store.restore(&[i; 32], paths)?.is_some());
                assert_eq!(fs::read(&path)?, *bytes);
            }
        }
        store.conn.execute("UPDATE patches SET data=zeroblob(length(data)) WHERE entry=(SELECT id FROM entries WHERE key=?1)",[[1;32]])?;
        fs::write(&path, b"working copy")?;
        assert!(store.restore(&[1; 32], paths).is_err());
        assert_eq!(fs::read(&path)?, b"working copy");
        assert_eq!(store.stats()?.patched_files, 1);
        fs::remove_file(object)?;
        assert!(store.restore(&[2; 32], paths).is_err());
        assert_eq!(fs::read(&path)?, b"working copy");
        assert_eq!(store.stats()?.charged_bytes, 0);
        Ok(())
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn sealed_reuse_validates_unsampled_bytes_and_survives_last_reference_eviction() -> Result<()> {
        use std::os::unix::fs::FileExt;
        let root = tempfile::tempdir()?;
        let mut store = Store::at(root.path().join("cache"), 8 << 20)?;
        store.allow_sealed = None;
        let path = root.path().join("artifact");
        let data = vec![17; CHUNK * 2];
        fs::write(&path, &data)?;
        let outputs = std::slice::from_ref(&path);
        store.save(&[0; 32], &[1; 32], outputs, b"", 0, None)?;
        if store.stats()?.sealed_objects == 0 {
            ensure!(
                env::var_os("JATO_TEST_SEALED").is_none(),
                "expected fs-verity"
            );
            return Ok(());
        }
        let size = data.len() as u64;
        let hash = *blake3::hash(&data).as_bytes();
        let object = crate::sealed::path(&store.dir, &hash);
        let verity = crate::sealed::measure(&File::open(&object)?)?;
        let source = File::open(&path)?;
        let held = crate::sealed::reuse(&source, &path, &store.dir, size, &store.conn)?.unwrap();
        File::options()
            .write(true)
            .open(&path)?
            .write_all_at(&[44], 17000)?;
        assert!(crate::sealed::reuse(&source, &path, &store.dir, size, &store.conn)?.is_none());
        // Both candidates have the same length and sampled bytes. The later
        // one can still be reused by its full digest.
        store.save(&[0; 32], &[2; 32], outputs, b"", 0, None)?;
        assert_eq!(store.stats()?.sealed_objects, 2);
        store.save(&[0; 32], &[3; 32], outputs, b"", 0, None)?;
        assert_eq!(store.stats()?.counters["sealed_reused_bytes"], size);
        store.prune(Some(0))?;
        assert!(!object.exists());
        crate::sealed::publish(&held, &object)?;
        assert_eq!(crate::sealed::measure(&File::open(&object)?)?, verity);
        assert_eq!(fs::read(object)?, data);
        Ok(())
    }

    #[test]
    fn oversize_entry_leaves_working_set_intact() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut store = Store::at(root.path().join("cache"), 2048)?;
        let output = root.path().join("out");
        fs::write(&output, b"tiny")?;
        store.save(
            &[0; 32],
            &[1; 32],
            std::slice::from_ref(&output),
            &[],
            0,
            None,
        )?;
        let mut bytes = vec![0; 4096];
        blake3::Hasher::new().finalize_xof().fill(&mut bytes);
        fs::write(&output, bytes)?;
        assert!(!store.save(
            &[0; 32],
            &[2; 32],
            std::slice::from_ref(&output),
            &[],
            0,
            None
        )?);
        assert!(store.restore(&[1; 32], &[output])?.is_some());
        Ok(())
    }
}
