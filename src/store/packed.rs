//! Stage ordered frames without retaining a per-frame vector. The private
//! spool holds either compressed bytes or a reference to an existing chunk.
use super::{CHUNK, Digest};
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;

// Archive boundaries delimit independent regions. Inside a long region,
// content-defined cuts can meet the same bytes again after an insertion.
pub(super) fn cut(window: &[u8], remaining: u64) -> usize {
    if remaining <= CHUNK as u64 {
        return window.len();
    }
    fastcdc::v2020::FastCDC::new(window, CHUNK / 4, CHUNK / 2, CHUNK)
        .cut(0, window.len())
        .1
}

const MEMORY_DIGESTS: usize = 1024;
const EXISTING: u32 = u32::MAX;
const PROBE_MIN: usize = 16 * 1024;
const INSERT_CHUNK: &str = "INSERT INTO chunks VALUES(?1,?2,?3,1) ON CONFLICT(hash) DO NOTHING";

// Preserve exact standalone admission accounting even when several GiB of
// frames repeat in a different order. Only large distinct sets spill.
#[derive(Default)]
struct Seen {
    memory: HashSet<Digest>,
    spill: Option<Connection>,
}

impl Seen {
    fn insert(&mut self, hash: Digest) -> Result<bool> {
        const INSERT: &str = "INSERT OR IGNORE INTO seen VALUES(?1)";
        if self.spill.is_none() {
            if self.memory.len() < MEMORY_DIGESTS {
                return Ok(self.memory.insert(hash));
            }
            if self.memory.contains(&hash) {
                return Ok(false);
            }
            let conn = Connection::open("")?;
            conn.execute_batch("PRAGMA cache_size=-128; PRAGMA journal_mode=OFF; CREATE TABLE seen(hash BLOB PRIMARY KEY) WITHOUT ROWID; BEGIN")?;
            {
                let mut stmt = conn.prepare_cached(INSERT)?;
                for digest in std::mem::take(&mut self.memory) {
                    stmt.execute([digest])?;
                }
            }
            self.spill = Some(conn);
        }
        let conn = self.spill.as_ref().unwrap();
        Ok(conn.prepare_cached(INSERT)?.execute([hash])? == 1)
    }
}

#[derive(Default)]
struct Encoder {
    compressor: Option<zstd::bulk::Compressor<'static>>,
    buffer: Vec<u8>,
}

impl Encoder {
    fn encode<'a>(&'a mut self, raw: &'a [u8]) -> Result<&'a [u8]> {
        if raw.len() >= 64 {
            let encoder = match &mut self.compressor {
                Some(encoder) => encoder,
                None => self.compressor.insert(zstd::bulk::Compressor::new(1)?),
            };
            self.buffer.clear();
            self.buffer
                .reserve(zstd::zstd_safe::compress_bound(raw.len()));
            encoder.compress_to_buffer(raw, &mut self.buffer)?;
            if self.buffer.len() < raw.len() {
                return Ok(&self.buffer);
            }
        }
        Ok(raw)
    }
}

pub(super) struct Recorder {
    spool: tempfile::SpooledTempFile,
    seen: Seen,
    encoder: Encoder,
    populated: bool,
    pub reused: u64,
}

impl Recorder {
    pub fn new(dir: &Path, populated: bool) -> Self {
        Self {
            spool: tempfile::spooled_tempfile_in(CHUNK, dir),
            seen: Seen::default(),
            encoder: Encoder::default(),
            populated,
            reused: 0,
        }
    }

    pub fn frame(&mut self, conn: &Connection, digest: Digest, raw: &[u8]) -> Result<u64> {
        let mut tag = 0;
        let mut charge = 0;
        let mut data = None;
        if self.seen.insert(digest)? {
            let size: Option<usize> = if self.populated && raw.len() >= PROBE_MIN {
                conn.prepare_cached("SELECT length(data) FROM chunks WHERE hash=?1 AND raw=?2 AND length(data) BETWEEN 1 AND raw")?
                    .query_row(params![&digest, raw.len()], |r| r.get(0)).optional()?
            } else {
                None
            };
            if let Some(size) = size {
                tag = EXISTING;
                charge = size + 96;
                self.reused += raw.len() as u64;
            } else {
                let bytes = self.encoder.encode(raw)?;
                tag = bytes.len() as u32;
                charge = bytes.len() + 96;
                data = Some(bytes);
            }
        }
        let mut header = [0; 40];
        header[..32].copy_from_slice(&digest);
        header[32..36].copy_from_slice(&(raw.len() as u32).to_le_bytes());
        header[36..].copy_from_slice(&tag.to_le_bytes());
        self.spool.write_all(&header)?;
        if let Some(data) = data {
            self.spool.write_all(data)?;
        }
        Ok(charge as u64)
    }

    pub fn rewind(&mut self) -> Result<()> {
        Ok(self.spool.rewind()?)
    }

    pub fn publish(
        &mut self,
        tx: &Transaction<'_>,
        path: &Path,
        source: &mut Option<File>,
        offset: u64,
        buffer: &mut [u8],
    ) -> Result<(Digest, u64)> {
        let mut header = [0; 40];
        self.spool.read_exact(&mut header)?;
        let digest: Digest = header[..32].try_into()?;
        let raw = u32::from_le_bytes(header[32..36].try_into()?) as usize;
        let tag = u32::from_le_bytes(header[36..].try_into()?);
        ensure!(raw > 0 && raw <= buffer.len(), "invalid staged raw frame");
        let bytes = match tag {
            // A previous frame of this entry published the same hash in
            // this transaction. Its ordered reference already owns the bytes.
            0 => return Ok((digest, raw as u64)),
            EXISTING => {
                if tx
                    .prepare_cached("SELECT EXISTS(SELECT 1 FROM chunks WHERE hash=?1 AND raw=?2)")?
                    .query_row(params![&digest, raw], |r| r.get::<_, bool>(0))?
                {
                    return Ok((digest, raw as u64));
                }
                // A GC or a concurrent writer evicted it between the probe
                // and this write transaction. Recover only the original bytes.
                let file = match source {
                    Some(f) => f,
                    None => source.insert(File::open(path)?),
                };
                let bytes = &mut buffer[..raw];
                file.read_exact_at(bytes, offset)?;
                ensure!(
                    blake3::hash(bytes).as_bytes() == &digest,
                    "compiler output changed before packed publication"
                );
                self.reused -= raw as u64;
                self.encoder.encode(bytes)?
            }
            n => {
                ensure!(n as usize <= raw, "invalid staged packed frame");
                let bytes = &mut buffer[..n as usize];
                self.spool.read_exact(bytes)?;
                bytes
            }
        };
        tx.prepare_cached(INSERT_CHUNK)?
            .execute(params![&digest, raw, bytes])?;
        Ok((digest, raw as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eviction_between_staging_and_publish_recovers_only_matching_bytes() -> Result<()> {
        let d = tempfile::tempdir()?;
        let path = d.path().join("output");
        let mut store = super::super::Store::at(d.path().join("cache"), 4 << 20)?;
        let raw = vec![23; CHUNK];
        let hash = *blake3::hash(&raw).as_bytes();
        let mut buffer = vec![0; CHUNK];
        for changed in [false, true] {
            std::fs::write(&path, &raw)?;
            store.save(
                &[1; 32],
                &[2; 32],
                std::slice::from_ref(&path),
                b"",
                1,
                None,
            )?;
            let mut record = Recorder::new(&store.dir, true);
            record.frame(&store.conn, hash, &raw)?;
            record.frame(&store.conn, hash, &raw)?;
            assert_eq!(record.reused, CHUNK as u64);
            record.rewind()?;
            store.prune(0)?;
            if changed {
                std::fs::write(&path, vec![24; CHUNK])?;
            }
            let tx = store.conn.transaction()?;
            let mut source = None;
            let recovered = record.publish(&tx, &path, &mut source, 0, &mut buffer);
            if changed {
                assert!(recovered.is_err());
            } else {
                assert_eq!(recovered?, (hash, CHUNK as u64));
                assert_eq!(record.reused, 0);
                record.publish(&tx, &path, &mut source, CHUNK as u64, &mut buffer)?;
                let data: Vec<u8> = tx.query_row("SELECT data FROM chunks", [], |r| r.get(0))?;
                assert_eq!(zstd::bulk::decompress(&data, CHUNK)?, raw);
            }
            tx.rollback()?;
            assert_eq!(store.stats()?.charged_bytes, 0);
        }
        Ok(())
    }
    #[test]
    fn spilled_index_recognizes_old_and_new_digests() -> Result<()> {
        let mut seen = Seen::default();
        for round in 0..2 {
            for i in 0..MEMORY_DIGESTS + 37 {
                assert_eq!(
                    seen.insert(*blake3::hash(&i.to_le_bytes()).as_bytes())?,
                    round == 0
                );
            }
        }
        assert!(seen.memory.is_empty() && seen.spill.is_some());
        Ok(())
    }
}
