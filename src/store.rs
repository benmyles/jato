use crate::staging::Output;
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CHUNK: usize = 256 * 1024;
const DEFAULT_LIMIT: u64 = 1024 * 1024 * 1024;
const SCHEMA: &str = include_str!("schema.sql");
const SCHEMA_VERSION: u32 = 9;
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

pub fn cache_limit() -> Result<u64> {
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

#[derive(Serialize)]
pub struct Stats {
    pub directory: PathBuf,
    pub limit_bytes: u64,
    pub charged_bytes: u64,
    pub database_bytes: u64,
    pub entries: u64,
    pub chunks: u64,
    pub compressed_bytes: u64,
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

struct Frame {
    hash: Digest,
    raw: usize,
    packed: usize,
}

struct Prepared {
    hash: Digest,
    size: u64,
    mode: u32,
    frames: Vec<Frame>,
    sealed: Option<crate::sealed::Object>,
}

#[derive(Default)]
struct Decoder {
    zstd: Option<zstd::bulk::Decompressor<'static>>,
    buffer: Vec<u8>,
}

impl Decoder {
    fn decode<'a>(&'a mut self, packed: &'a [u8], raw: usize, hash: Digest) -> Result<&'a [u8]> {
        ensure!(raw <= CHUNK && packed.len() <= raw, Damaged::Chunk(hash));
        if raw == packed.len() {
            return Ok(packed);
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
        let mut store = Self::at(cache_dir()?, cache_limit()?)?;
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
        conn.busy_timeout(Duration::from_secs(3))?;
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
                if version > 0 {
                    tx.execute_batch(
                        "ALTER TABLE files ADD COLUMN single INTEGER NOT NULL DEFAULT 0",
                    )?;
                }
                if version >= 2 {
                    tx.execute_batch(
                        "ALTER TABLE plans ADD COLUMN manifest INTEGER REFERENCES manifests",
                    )?;
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

    pub fn stable_compiler(&self, compiler: &std::ffi::OsStr, identity: &Digest) -> Result<bool> {
        if let Some(stable) = self
            .conn
            .query_row(
                "SELECT stable FROM compilers WHERE identity=?1",
                [identity],
                |r| r.get(0),
            )
            .optional()?
        {
            return Ok(stable);
        }
        let stable = crate::admission::stable_compiler(compiler)?;
        self.record_compiler(identity, stable)?;
        Ok(stable)
    }

    pub fn record_compiler(&self, identity: &Digest, stable: bool) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO compilers VALUES(?1,?2,?3)",
            params![identity, stable, now()],
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
        let budget = (self.limit / 32).min(256 * 1024);
        if stdout.len() + stderr.len() > 65536
            || (stdout.len() + stderr.len() + 160) as u64 > budget
        {
            return Ok(());
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
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
        let evicted = evict(&tx, self.limit)?;
        tx.commit()?;
        if evicted != 0 {
            self.collect_objects(false)?;
        }
        Ok(())
    }

    pub fn plans(&mut self, recipe: &Digest) -> Result<Vec<Vec<u8>>> {
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
        Ok(plans)
    }

    pub fn has_sources(&self, digest: &Digest) -> Result<bool> {
        Ok(self
            .conn
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM source_variants WHERE digest=?1)")?
            .query_row([digest], |r| r.get(0))?)
    }

    pub fn topology(&self, key: &Digest) -> Result<Option<Digest>> {
        Ok(self
            .conn
            .query_row(
                "SELECT topology FROM entries JOIN plans ON entry=id WHERE key=?1",
                [key],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn refresh_topology(&self, key: &Digest, topology: &Digest) -> Result<()> {
        self.conn.execute(
            "UPDATE plans SET topology=?1 WHERE entry=(SELECT id FROM entries WHERE key=?2)",
            params![topology, key],
        )?;
        Ok(())
    }

    pub fn knows_artifact(&self, digest: &Digest, codegen: bool) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM files JOIN plans ON plans.entry=files.entry WHERE digest=?1 AND (?2=0 OR codegen=1) AND NOT EXISTS(SELECT 1 FROM needs WHERE needs.entry=files.entry))",
            params![digest,codegen],
            |r| r.get(0),
        )?)
    }

    pub fn count(&self, event: &str, value: u64) -> Result<()> {
        self.conn.execute("INSERT INTO counters VALUES(?1,?2) ON CONFLICT(name) DO UPDATE SET value=value+excluded.value", params![event, value])?;
        Ok(())
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
        let mut spool = tempfile::spooled_tempfile_in(CHUNK, &self.dir);
        let mut compressed = Vec::new();
        let mut buffer = Vec::new();
        let mut compressor = None;
        let manifest = plan.as_ref().map(|p| encode_plan(p.inputs)).transpose()?;
        let plan_cost = plan
            .as_ref()
            .zip(manifest.as_ref())
            .map_or(0, |(p, (_, bytes))| 272 + bytes.len() + 96 * p.needs.len());
        let mut charge = (320 + stderr.len() + 128 * outputs.len() + plan_cost) as u64;
        if charge > self.limit || stderr.len() > DIAGNOSTIC_LIMIT {
            return Ok(false);
        }
        let used = self
            .conn
            .query_row("SELECT bytes FROM usage", [], |r| r.get::<_, u64>(0))?;
        let headroom = self.limit.saturating_sub(used);
        let mut files = Vec::new();
        for path in outputs {
            let mut source =
                File::open(path).with_context(|| format!("opening {}", path.display()))?;
            let metadata = source.metadata()?;
            ensure!(metadata.is_file(), "expected regular compiler output");
            let mut file = Prepared {
                hash: [0; 32],
                size: 0,
                mode: metadata.permissions().mode() & 0o777,
                frames: Vec::new(),
                sealed: None,
            };
            let length = metadata.len();
            if length >= CHUNK as u64
                && charge.saturating_add(length).saturating_add(256) <= headroom
                && *self
                    .allow_sealed
                    .get_or_insert_with(|| crate::sealed::supported(&self.dir))
            {
                if let Ok(object) = crate::sealed::prepare(&source, &self.dir) {
                    ensure!(
                        crate::fingerprint::stamp(&metadata)
                            == crate::fingerprint::stamp(&fs::metadata(path)?),
                        "compiler output changed during capture"
                    );
                    file.hash = object.hash;
                    file.size = length;
                    charge += length + 256;
                    file.sealed = Some(object);
                    files.push(file);
                    continue;
                }
                // Cross-device targets and filesystems lacking verity retain
                // the same compressed chunk cache as smaller artifacts.
                self.allow_sealed = Some(false);
            }
            let mut hasher = blake3::Hasher::new();
            let window = length.min(CHUNK as u64) as usize;
            if buffer.len() < window {
                buffer.resize(window, 0);
            }
            loop {
                let mut n = 0;
                while n < buffer.len() {
                    let read = source.read(&mut buffer[n..])?;
                    if read == 0 {
                        break;
                    }
                    n += read;
                }
                if n == 0 {
                    break;
                }
                let raw = &buffer[..n];
                let packed = if n >= 64 {
                    let encoder = match &mut compressor {
                        Some(encoder) => encoder,
                        None => compressor.insert(zstd::bulk::Compressor::new(1)?),
                    };
                    compressed.clear();
                    compressed.reserve(zstd::zstd_safe::compress_bound(n));
                    encoder.compress_to_buffer(raw, &mut compressed)?;
                    if compressed.len() < n {
                        compressed.as_slice()
                    } else {
                        raw
                    }
                } else {
                    raw
                };
                let overhead = if length <= CHUNK as u64 { 96 } else { 192 };
                charge += (packed.len() + overhead) as u64;
                if charge > self.limit {
                    return Ok(false);
                }
                spool.write_all(packed)?;
                if metadata.len() > CHUNK as u64 {
                    hasher.update(raw);
                }
                file.size += n as u64;
                file.frames.push(Frame {
                    hash: *blake3::hash(raw).as_bytes(),
                    raw: n,
                    packed: packed.len(),
                });
            }
            ensure!(
                file.size == metadata.len()
                    && crate::fingerprint::stamp(&metadata)
                        == crate::fingerprint::stamp(&fs::metadata(path)?),
                "compiler output changed while recording"
            );
            file.hash = if file.frames.len() == 1 {
                file.frames[0].hash
            } else {
                *hasher.finalize().as_bytes()
            };
            files.push(file);
        }
        spool.rewind()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let added = tx.execute("INSERT OR IGNORE INTO entries(key,recipe,access,stderr,stderr_hash,compile_ms) VALUES(?1,?2,?3,?4,?5,?6)",
            params![key, recipe, now(), stderr, blake3::hash(stderr).as_bytes(), elapsed_ms])?;
        if added == 0 {
            return Ok(true);
        }
        let entry = tx.last_insert_rowid();
        for (slot, file) in files.iter().enumerate() {
            let single = file.frames.len() == 1;
            tx.execute(
                "INSERT INTO files(entry,slot,digest,size,mode,single) VALUES(?1,?2,?3,?4,?5,?6)",
                params![entry, slot, &file.hash, file.size, file.mode, single],
            )?;
            if let Some(object) = &file.sealed {
                crate::sealed::publish(object, &crate::sealed::path(&self.dir, &file.hash))?;
                tx.execute("INSERT INTO objects VALUES(?1,?2,?3,1) ON CONFLICT(hash) DO UPDATE SET refs=refs+1",
                    params![file.hash,object.verity,file.size])?;
                tx.execute(
                    "INSERT INTO clones VALUES(?1,?2,?3)",
                    params![entry, slot, file.hash],
                )?;
            }
            for (ordinal, frame) in file.frames.iter().enumerate() {
                let packed = &mut buffer[..frame.packed];
                spool.read_exact(packed)?;
                tx.prepare_cached("INSERT INTO chunks VALUES(?1,?2,?3,1) ON CONFLICT(hash) DO UPDATE SET refs=refs+1")?
                    .execute(params![&frame.hash, frame.raw, &*packed])?;
                if !single {
                    tx.prepare_cached("INSERT INTO parts VALUES(?1,?2,?3,?4)")?
                        .execute(params![entry, slot, ordinal, &frame.hash])?;
                }
            }
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
            for digest in plan.needs {
                tx.execute(
                    "INSERT OR IGNORE INTO needs VALUES(?1,?2,?3)",
                    params![entry, digest, plan.codegen],
                )?;
            }
            // Pipelined Cargo consumers may finish before their producer. Only
            // expose a record after every missing input has independent proof.
            loop {
                let changed = tx.execute(
                    "DELETE FROM needs WHERE EXISTS (
                    SELECT 1 FROM files f JOIN plans p ON p.entry=f.entry
                    WHERE f.digest=needs.digest AND p.codegen>=needs.codegen
                    AND NOT EXISTS(SELECT 1 FROM needs parent WHERE parent.entry=f.entry)
                )",
                    [],
                )?;
                if changed == 0 {
                    break;
                }
            }
        }
        let time = now();
        let sweep = files.iter().any(|f| f.sealed.is_some())
            && tx.execute(
                "INSERT INTO maintenance VALUES('sealed',?1) ON CONFLICT(name)
             DO UPDATE SET last_run=excluded.last_run WHERE last_run < ?2",
                params![time, time - 3_600_000_000],
            )? == 1;
        let evicted = evict(&tx, self.limit)?;
        let kept = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM entries WHERE id=?1)",
            [entry],
            |r| r.get(0),
        )?;
        tx.commit()?;
        if evicted > 0 || sweep {
            self.collect_objects(sweep)?;
        }
        Ok(kept)
    }

    pub fn restore(&mut self, key: &Digest, outputs: &[PathBuf]) -> Result<Option<Vec<u8>>> {
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
                        CASE WHEN length(c.data)<=?2 THEN c.data END
                 FROM files f LEFT JOIN clones x ON x.entry=f.entry AND x.slot=f.slot AND x.hash=f.digest
                 LEFT JOIN objects o ON o.hash=x.hash
                 LEFT JOIN chunks c ON f.single=1 AND c.hash=f.digest
                 WHERE f.entry=?1 ORDER BY f.slot"
            )?;
            let mut records = files.query(params![id, CHUNK])?;
            let mut staged = crate::staging::for_outputs(outputs)?;
            let mut decoder = Decoder::default();
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
                    let bytes = decoder.decode(packed, raw, digest)?;
                    ensure!(
                        blake3::hash(bytes).as_bytes() == &digest,
                        Damaged::Chunk(digest)
                    );
                    ensure!(size == raw as u64, Damaged::Entry);
                    tmp.write_all(bytes)?;
                    continue;
                }
                let verity = row.get::<_, Option<Digest>>(5)?;
                if let Some(verity) = verity {
                    let path = crate::sealed::path(&self.dir, &digest);
                    let mut source = File::open(path).map_err(|e| {
                        if e.kind() == std::io::ErrorKind::NotFound {
                            anyhow::Error::new(Damaged::Object(digest))
                        } else {
                            e.into()
                        }
                    })?;
                    crate::sealed::restore(&mut source, tmp, &verity, &digest, size)?;
                    continue;
                }
                let mut hasher = blake3::Hasher::new();
                let mut written = 0u64;
                let mut stmt = tx.prepare_cached("SELECT c.hash,c.raw,CASE WHEN length(c.data)<=262144 THEN c.data END FROM parts p JOIN chunks c ON c.hash=p.chunk WHERE entry=?1 AND slot=?2 ORDER BY ordinal")?;
                let mut rows = stmt.query(params![id, slot])?;
                while let Some(row) = rows.next()? {
                    let hash: Digest = row.get(0)?;
                    let raw: usize = row.get(1)?;
                    let packed = row
                        .get_ref(2)?
                        .as_blob()
                        .map_err(|_| Damaged::Chunk(hash))?;
                    let bytes = decoder.decode(packed, raw, hash)?;
                    tmp.write_all(bytes)?;
                    hasher.update(bytes);
                    written += raw as u64;
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
        tx.commit()?;
        let staged = match staged {
            Ok(files) => files,
            Err(e) => {
                if let Some(damage) = e.downcast_ref::<Damaged>() {
                    match damage {
                        Damaged::Chunk(hash) => self.conn.execute(
                            "DELETE FROM entries WHERE id IN (SELECT entry FROM parts WHERE chunk=?1 UNION SELECT entry FROM files WHERE single=1 AND digest=?1)", [hash],
                        )?,
                        Damaged::Object(hash) => self.conn.execute(
                            "DELETE FROM entries WHERE id IN (SELECT entry FROM clones WHERE hash=?1)", [hash],
                        )?,
                        Damaged::Entry => self.conn.execute("DELETE FROM entries WHERE key=?1", [key])?,
                    };
                    self.collect_objects(false)?;
                    let _ = self.count("corrupt", 1);
                }
                return Err(e);
            }
        };
        for (tmp, path) in staged.into_iter().zip(outputs) {
            tmp.publish(path)?;
        }
        // A failed statistics update cannot invalidate already verified outputs.
        let _ = (|| -> Result<()> {
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute(
                "UPDATE entries SET access=?1 WHERE key=?2",
                params![now(), key],
            )?;
            tx.execute("INSERT INTO counters VALUES ('hit',1),('reused_compiler_ms',?1) ON CONFLICT(name) DO UPDATE SET value=value+excluded.value", [ms])?;
            tx.commit()?;
            Ok(())
        })();
        Ok(Some(stderr))
    }

    pub fn prune(&mut self, limit: u64) -> Result<u64> {
        crate::staging::clean(&self.dir)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
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
        let mut select = tx.prepare("SELECT hash FROM retired LIMIT 128")?;
        let mut forget = tx.prepare("DELETE FROM retired WHERE hash=?1")?;
        loop {
            let retired = select
                .query_map([], |r| r.get::<_, Digest>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if retired.is_empty() {
                break;
            }
            for digest in retired {
                let p = crate::sealed::path(&self.dir, &digest);
                match fs::remove_file(p) {
                    Ok(()) => (),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                    Err(e) => return Err(e.into()),
                }
                forget.execute([digest])?;
            }
        }
        drop((select, forget));
        if sweep {
            crate::sealed::sweep(&self.dir, &tx)?;
        }
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
            limit_bytes: self.limit,
            charged_bytes: scalar("SELECT bytes FROM usage")?,
            database_bytes,
            entries: scalar("SELECT COUNT(*) FROM entries")?,
            chunks: scalar("SELECT COUNT(*) FROM chunks")?,
            pending_entries: scalar("SELECT COUNT(DISTINCT entry) FROM needs")?,
            probe_entries: scalar("SELECT COUNT(*) FROM probes")?,
            sealed_objects: scalar("SELECT COUNT(*) FROM objects")?,
            sealed_bytes: scalar("SELECT COALESCE(SUM(bytes),0) FROM objects")?,
            compressed_bytes: scalar("SELECT COALESCE(SUM(length(data)),0) FROM chunks")?,
            artifact_bytes: scalar("SELECT COALESCE(SUM(size),0) FROM files")?,
            counters: events
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?,
        })
    }
}

fn probe_checksum(stdout: &[u8], stderr: &[u8]) -> Digest {
    *blake3::Hasher::new()
        .update(&(stdout.len() as u64).to_le_bytes())
        .update(stdout)
        .update(stderr)
        .finalize()
        .as_bytes()
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

    #[test]
    fn variants_share_checked_manifests_and_single_chunk_payloads() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut store = Store::at(root.path().join("cache"), 1024 * 1024)?;
        let outputs = [root.path().join("metadata")];
        fs::write(&outputs[0], b"shared output")?;
        let inputs = b"repeated/source/path.rs ".repeat(1000);
        let evidence = || Evidence {
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
            store.plans(&[8; 32])?.as_slice(),
            std::slice::from_ref(&inputs)
        );
        store
            .conn
            .execute("UPDATE manifests SET data=zeroblob(length(data))", [])?;
        assert!(store.plans(&[8; 32])?.is_empty());
        assert_eq!(store.stats()?.charged_bytes, 0);
        assert!(store.save(&[8; 32], &[1; 32], &outputs, b"", 1, Some(evidence()))?);
        assert_eq!(store.plans(&[8; 32])?, [inputs]);
        assert_eq!(store.restore(&[1; 32], &outputs)?, Some(vec![]));
        store.prune(0)?;
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
        assert_eq!(store.prune(size - 1)?, 1);
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
        assert!(store.plans(&[2; 32])?.is_empty());
        assert!(!store.knows_artifact(&b, true)?);
        assert!(
            store
                .restore(&[1; 32], std::slice::from_ref(&file))?
                .is_none()
        );
        save(&mut store, 3, b"producer", &[], false)?;
        assert!(store.plans(&[2; 32])?.is_empty());
        save(&mut store, 4, b"producer", &[], true)?;
        assert!(!store.plans(&[2; 32])?.is_empty());
        assert!(store.knows_artifact(&b, true)?);
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
        store.prune(0)?;
        assert!(!orphan.exists());
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
