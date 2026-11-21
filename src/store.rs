use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Serialize;
use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tempfile::NamedTempFile;

const CHUNK: usize = 256 * 1024;
const DEFAULT_LIMIT: u64 = 1024 * 1024 * 1024;
const SCHEMA: &str = include_str!("schema.sql");
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
        conn.busy_timeout(Duration::from_secs(3))?;
        conn.execute_batch(
            "PRAGMA foreign_keys=ON; PRAGMA cache_size=-1024; PRAGMA mmap_size=33554432; PRAGMA synchronous=NORMAL; PRAGMA journal_size_limit=4194304;",
        )?;
        let version: u32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        ensure!(version <= 6, "unrecognized Jato cache format {version}");
        if version < 6 {
            let init = OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(dir.join("schema.lock"))?;
            init.lock()?;
            let version: u32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            if version == 0 {
                conn.execute_batch("PRAGMA auto_vacuum=FULL; PRAGMA journal_mode=WAL;")?;
            }
            if version < 6 {
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                if version == 2 {
                    tx.execute_batch(
                        "ALTER TABLE plans ADD COLUMN codegen INTEGER NOT NULL DEFAULT 0",
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
        self.conn.execute(
            "INSERT OR IGNORE INTO compilers VALUES(?1,?2,?3)",
            params![identity, stable, now()],
        )?;
        Ok(stable)
    }

    pub fn plans(&self, recipe: &Digest) -> Result<Vec<Vec<u8>>> {
        let mut q = self.conn.prepare_cached("SELECT inputs FROM entries JOIN plans ON entry=id WHERE recipe=?1 AND NOT EXISTS(SELECT 1 FROM needs WHERE needs.entry=id) AND length(inputs)<=262144 ORDER BY access DESC LIMIT 4")?;
        let mut plans = Vec::new();
        for row in q.query_map([recipe], |r| r.get::<_, Vec<u8>>(0))? {
            let bytes = row?;
            if !plans.contains(&bytes) {
                plans.push(bytes);
            }
        }
        Ok(plans)
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
        let mut compressed = Vec::with_capacity(zstd::zstd_safe::compress_bound(CHUNK));
        let mut buffer = vec![0; CHUNK];
        let mut compressor = zstd::bulk::Compressor::new(1)?;
        let plan_cost = plan
            .as_ref()
            .map_or(0, |p| 80 + p.inputs.len() + 96 * p.needs.len());
        let mut charge = (320 + stderr.len() + 128 * outputs.len() + plan_cost) as u64;
        if charge > self.limit || stderr.len() > DIAGNOSTIC_LIMIT {
            return Ok(false);
        }
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
                && charge.saturating_add(length).saturating_add(256) <= self.limit
                && *self
                    .allow_sealed
                    .get_or_insert_with(|| crate::sealed::supported(&self.dir))
            {
                if let Ok(object) = crate::sealed::prepare(&source, &self.dir) {
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
                compressor.compress_to_buffer(raw, &mut compressed)?;
                let packed = if compressed.len() < n {
                    compressed.as_slice()
                } else {
                    raw
                };
                charge += (packed.len() + 192) as u64;
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
                file.size == metadata.len(),
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
            tx.execute(
                "INSERT INTO files VALUES(?1,?2,?3,?4,?5)",
                params![entry, slot, &file.hash, file.size, file.mode],
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
                tx.prepare_cached("INSERT INTO parts VALUES(?1,?2,?3,?4)")?
                    .execute(params![entry, slot, ordinal, &frame.hash])?;
            }
        }
        if let Some(plan) = plan {
            tx.execute(
                "INSERT INTO plans VALUES(?1,?2,?3,?4)",
                params![entry, plan.inputs, plan.topology, plan.codegen],
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
        let evicted = evict(&tx, self.limit)?;
        let kept = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM entries WHERE id=?1)",
            [entry],
            |r| r.get(0),
        )?;
        tx.commit()?;
        if evicted > 0 {
            self.collect_objects(false)?;
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
        let staged = (|| -> Result<Vec<NamedTempFile>> {
            ensure!(blake3::hash(&stderr).as_bytes() == &hash, Damaged::Entry);
            let count: usize =
                tx.query_row("SELECT COUNT(*) FROM files WHERE entry=?1", [id], |r| {
                    r.get(0)
                })?;
            ensure!(count == outputs.len(), Damaged::Entry);
            let mut staged = crate::staging::for_outputs(outputs)?;
            let mut decompressor = zstd::bulk::Decompressor::new()?;
            let mut decoded = Vec::with_capacity(CHUNK);
            for (slot, tmp) in staged.iter_mut().enumerate() {
                let (digest, size, mode) = tx.query_row(
                    "SELECT digest,size,mode FROM files WHERE entry=?1 AND slot=?2",
                    params![id, slot],
                    |r| {
                        Ok((
                            r.get::<_, Digest>(0)?,
                            r.get::<_, u64>(1)?,
                            r.get::<_, u32>(2)?,
                        ))
                    },
                )?;
                let verity=tx.query_row("SELECT o.verity FROM clones c JOIN objects o ON o.hash=c.hash WHERE c.entry=?1 AND c.slot=?2 AND o.hash=?3",params![id,slot,digest],|r|r.get::<_,Digest>(0)).optional()?;
                if let Some(verity) = verity {
                    let path = crate::sealed::path(&self.dir, &digest);
                    let mut source = File::open(path).map_err(|e| {
                        if e.kind() == std::io::ErrorKind::NotFound {
                            anyhow::Error::new(Damaged::Object(digest))
                        } else {
                            e.into()
                        }
                    })?;
                    crate::sealed::restore(&mut source, tmp.as_file_mut(), &verity, &digest, size)?;
                    tmp.as_file()
                        .set_permissions(fs::Permissions::from_mode(mode & 0o777))?;
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
                    ensure!(raw <= CHUNK && packed.len() <= raw, Damaged::Chunk(hash));
                    let bytes = if packed.len() == raw {
                        packed
                    } else {
                        decompressor
                            .decompress_to_buffer(packed, &mut decoded)
                            .map_err(|_| Damaged::Chunk(hash))?;
                        &decoded
                    };
                    ensure!(bytes.len() == raw, Damaged::Chunk(hash));
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
                        decoded.resize(raw, 0);
                        tmp.read_exact(&mut decoded)?;
                        ensure!(
                            blake3::hash(&decoded).as_bytes() == &chunk,
                            Damaged::Chunk(chunk)
                        );
                    }
                    return Err(Damaged::Entry.into());
                }
                tmp.as_file()
                    .set_permissions(fs::Permissions::from_mode(mode & 0o777))?;
            }
            Ok(staged)
        })();
        tx.commit()?;
        let staged = match staged {
            Ok(files) => files,
            Err(e) => {
                if let Some(damage) = e.downcast_ref::<Damaged>() {
                    match damage {
                        Damaged::Chunk(hash)=>self.conn.execute("DELETE FROM entries WHERE id IN (SELECT entry FROM parts WHERE chunk=?1)",[hash])?,
                        Damaged::Object(hash)=>self.conn.execute("DELETE FROM entries WHERE id IN (SELECT entry FROM clones WHERE hash=?1)",[hash])?,
                        Damaged::Entry=>self.conn.execute("DELETE FROM entries WHERE key=?1",[key])?,
                    };
                    self.collect_objects(false)?;
                    let _ = self.count("corrupt", 1);
                }
                return Err(e);
            }
        };
        for (tmp, path) in staged.into_iter().zip(outputs) {
            tmp.persist(path)?;
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
        Ok(removed)
    }

    fn collect_objects(&mut self, sweep: bool) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut q = tx.prepare("SELECT hash FROM retired")?;
        let retired = q
            .query_map([], |r| r.get::<_, Digest>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(q);
        for digest in retired {
            let p = crate::sealed::path(&self.dir, &digest);
            match fs::remove_file(p) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
            tx.execute("DELETE FROM retired WHERE hash=?1", [digest])?;
        }
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

fn evict(tx: &Transaction<'_>, limit: u64) -> Result<u64> {
    let mut removed = 0;
    while tx.query_row("SELECT bytes FROM usage", [], |r| r.get::<_, u64>(0))? > limit {
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
