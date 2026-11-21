use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Serialize;
use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File};
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

pub struct Store {
    conn: Connection,
    pub dir: PathBuf,
    limit: u64,
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
}

impl Store {
    pub fn open() -> Result<Self> {
        Self::at(cache_dir()?, cache_limit()?)
    }

    fn at(dir: PathBuf, limit: u64) -> Result<Self> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        let mut conn = Connection::open(dir.join("artifacts-v1.sqlite3"))?;
        conn.busy_timeout(Duration::from_secs(3))?;
        conn.execute_batch(
            "PRAGMA foreign_keys=ON; PRAGMA cache_size=-1024; PRAGMA synchronous=NORMAL;",
        )?;
        let version: u32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version == 0 {
            conn.execute_batch("PRAGMA auto_vacuum=FULL; PRAGMA journal_mode=WAL;")?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(SCHEMA)?;
            tx.commit()?;
        } else {
            ensure!(version == 1, "unrecognized Jato cache format {version}");
        }
        Ok(Self { conn, dir, limit })
    }

    pub fn knows_recipe(&self, recipe: &Digest) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM entries WHERE recipe=?1)",
            [recipe],
            |r| r.get(0),
        )?)
    }

    pub fn knows_artifact(&self, digest: &Digest) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM files WHERE digest=?1)",
            [digest],
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
    ) -> Result<bool> {
        // Compress in parallel with other compilers, outside SQLite's single writer.
        // Anonymous staging files are reclaimed by the OS even on process death.
        let mut spool = tempfile::tempfile_in(&self.dir)?;
        let mut buffer = vec![0; CHUNK];
        let mut compressor = zstd::bulk::Compressor::new(1)?;
        let mut packed_total = 0u64;
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
            };
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
                let compressed = compressor.compress(raw)?;
                let packed = if compressed.len() < n {
                    compressed.as_slice()
                } else {
                    raw
                };
                packed_total += (packed.len() + 128) as u64;
                if packed_total + stderr.len() as u64 > self.limit {
                    return Ok(false);
                }
                spool.write_all(packed)?;
                hasher.update(raw);
                file.size += n as u64;
                file.frames.push(Frame {
                    hash: *blake3::hash(raw).as_bytes(),
                    raw: n,
                    packed: packed.len(),
                });
            }
            file.hash = *hasher.finalize().as_bytes();
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
            for (ordinal, frame) in file.frames.iter().enumerate() {
                let packed = &mut buffer[..frame.packed];
                spool.read_exact(packed)?;
                tx.prepare_cached("INSERT INTO chunks VALUES(?1,?2,?3,1) ON CONFLICT(hash) DO UPDATE SET refs=refs+1")?
                    .execute(params![&frame.hash, frame.raw, &*packed])?;
                tx.prepare_cached("INSERT INTO parts VALUES(?1,?2,?3,?4)")?
                    .execute(params![entry, slot, ordinal, &frame.hash])?;
            }
        }
        evict(&tx, self.limit)?;
        let kept = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM entries WHERE id=?1)",
            [entry],
            |r| r.get(0),
        )?;
        tx.commit()?;
        Ok(kept)
    }

    pub fn restore(&mut self, key: &Digest, outputs: &[PathBuf]) -> Result<Option<Vec<u8>>> {
        let tx = self.conn.transaction()?;
        let entry = tx.query_row("SELECT id,stderr,stderr_hash,compile_ms FROM entries WHERE key=?1 AND length(stderr)<=?2",
            params![key, DIAGNOSTIC_LIMIT], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?, r.get::<_, Digest>(2)?, r.get::<_, u64>(3)?))).optional()?;
        let Some((id, stderr, hash, ms)) = entry else {
            return Ok(None);
        };
        let staged = (|| -> Result<Vec<NamedTempFile>> {
            ensure!(
                blake3::hash(&stderr).as_bytes() == &hash,
                "damaged diagnostic stream"
            );
            let count: usize =
                tx.query_row("SELECT COUNT(*) FROM files WHERE entry=?1", [id], |r| {
                    r.get(0)
                })?;
            ensure!(count == outputs.len(), "incomplete output set");
            let mut staged = Vec::with_capacity(count);
            for (slot, path) in outputs.iter().enumerate() {
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
                let mut tmp =
                    NamedTempFile::new_in(path.parent().context("output lacks a directory")?)?;
                let mut hasher = blake3::Hasher::new();
                let mut written = 0u64;
                let mut stmt = tx.prepare_cached("SELECT c.hash,c.raw,c.data FROM parts p JOIN chunks c ON c.hash=p.chunk WHERE entry=?1 AND slot=?2 ORDER BY ordinal")?;
                let mut rows = stmt.query(params![id, slot])?;
                while let Some(row) = rows.next()? {
                    let hash: Digest = row.get(0)?;
                    let raw: usize = row.get(1)?;
                    let packed = row.get_ref(2)?.as_blob()?;
                    ensure!(raw <= CHUNK && packed.len() <= raw, "invalid cache frame");
                    let decoded;
                    let bytes = if packed.len() == raw {
                        packed
                    } else {
                        decoded = zstd::bulk::decompress(packed, raw)?;
                        &decoded
                    };
                    ensure!(
                        bytes.len() == raw && blake3::hash(bytes).as_bytes() == &hash,
                        "damaged cache chunk"
                    );
                    tmp.write_all(bytes)?;
                    hasher.update(bytes);
                    written += raw as u64;
                }
                ensure!(
                    written == size && hasher.finalize().as_bytes() == &digest,
                    "incomplete or damaged artifact"
                );
                tmp.as_file()
                    .set_permissions(fs::Permissions::from_mode(mode & 0o777))?;
                staged.push(tmp);
            }
            Ok(staged)
        })();
        tx.commit()?;
        let staged = match staged {
            Ok(files) => files,
            Err(e) => {
                // Drop every entry sharing the damaged entry's chunks. Otherwise a
                // deduplicating writer could keep reusing a corrupt shared frame.
                self.conn.execute("DELETE FROM entries WHERE key=?1 OR id IN (SELECT p.entry FROM parts p JOIN parts bad ON p.chunk=bad.chunk WHERE bad.entry=(SELECT id FROM entries WHERE key=?1))", [key])?;
                return Err(e);
            }
        };
        for (tmp, path) in staged.into_iter().zip(outputs) {
            tmp.persist(path)?;
        }
        // A failed statistics update cannot invalidate already verified outputs.
        let _ = self.conn.execute(
            "UPDATE entries SET access=?1 WHERE key=?2",
            params![now(), key],
        );
        let _ = self.count("hit", 1);
        let _ = self.count("reused_compiler_ms", ms);
        Ok(Some(stderr))
    }

    pub fn prune(&mut self, limit: u64) -> Result<u64> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let removed = evict(&tx, limit)?;
        tx.commit()?;
        Ok(removed)
    }

    pub fn stats(&self) -> Result<Stats> {
        let conn = &self.conn;
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
            assert!(store.save(&[0; 32], &[k; 32], &files, b"warning\n", 33)?);
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
    fn oversize_entry_leaves_working_set_intact() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut store = Store::at(root.path().join("cache"), 2048)?;
        let output = root.path().join("out");
        fs::write(&output, b"tiny")?;
        store.save(&[0; 32], &[1; 32], std::slice::from_ref(&output), &[], 0)?;
        let mut bytes = vec![0; 4096];
        blake3::Hasher::new().finalize_xof().fill(&mut bytes);
        fs::write(&output, bytes)?;
        assert!(!store.save(&[0; 32], &[2; 32], std::slice::from_ref(&output), &[], 0)?);
        assert!(store.restore(&[1; 32], &[output])?.is_some());
        Ok(())
    }
}
