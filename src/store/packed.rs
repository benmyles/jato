//! Stage ordered frames without retaining a per-frame vector. The private
//! spool holds compressed bytes or descriptors for bytes available elsewhere.
use super::objects::{self, Kind};
use super::{CHUNK, Damaged, Digest};
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

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
const RAW_SOURCE: u32 = u32::MAX - 1;
const PROBE_MIN: usize = 16 * 1024;
const FILE_MIN: usize = 64 * 1024;
const INSERT_CHUNK: &str = "INSERT INTO chunks(hash,raw,data,refs,external) VALUES(?1,?2,?3,1,?4) ON CONFLICT(hash) DO NOTHING";

fn charge(encoded: usize, file: bool) -> usize {
    96 + if file {
        encoded.div_ceil(4096) * 4096 + 256
    } else {
        encoded
    }
}

// Only the SQLite writer publishes or removes a frame. Renaming one private
// pending file makes publication atomic for readers on an older WAL snapshot.
fn write_frame(dir: &Path, digest: &Digest, bytes: &[u8]) -> Result<()> {
    let dest = Kind::Frame.path(dir, digest);
    fs::create_dir_all(dest.parent().unwrap())?;
    let pending = dir.join("frames-v1/.pending");
    let create = || {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&pending)
    };
    let mut file = match create() {
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            objects::remove(&pending)?;
            create()?
        }
        result => result?,
    };
    file.write_all(bytes)?;
    fs::rename(pending, dest)?;
    Ok(())
}

// The destination is a newly created staging inode. Seeking over complete
// zero pages there reproduces those bytes without dirtying output pages.
pub(super) fn write_output(file: &mut File, bytes: &[u8]) -> Result<bool> {
    const ZERO: [u8; 4096] = [0; 4096];
    let hole = if bytes.len() >= 2 * ZERO.len() && bytes.ends_with(&[0; 16]) {
        bytes
            .rchunks_exact(ZERO.len())
            .take_while(|b| *b == ZERO)
            .count()
            * ZERO.len()
    } else {
        0
    };
    if hole < 2 * ZERO.len() {
        file.write_all(bytes)?;
        return Ok(false);
    }
    file.write_all(&bytes[..bytes.len() - hole])?;
    file.seek_relative(hole as i64)?;
    Ok(true)
}

pub(super) fn payload<'a>(
    dir: &Path,
    hash: &Digest,
    raw: usize,
    inline: &'a [u8],
    external: usize,
    buffer: &'a mut Vec<u8>,
) -> Result<&'a [u8]> {
    if external == 0 {
        return Ok(inline);
    }
    ensure!(
        inline.is_empty() && external <= raw && raw <= CHUNK,
        Damaged::Chunk(*hash)
    );
    let mut file = crate::content::open_with_flags(
        &Kind::Frame.path(dir, hash),
        libc::O_NONBLOCK | libc::O_NOFOLLOW,
    )
    .map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ELOOP) {
            anyhow::Error::new(Damaged::Chunk(*hash))
        } else {
            e.into()
        }
    })?;
    let m = file.metadata()?;
    ensure!(
        m.is_file() && m.len() == external as u64,
        Damaged::Chunk(*hash)
    );
    buffer.resize(external, 0);
    file.read_exact(buffer).map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            anyhow::Error::new(Damaged::Chunk(*hash))
        } else {
            e.into()
        }
    })?;
    Ok(buffer)
}

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
    dir: PathBuf,
    pub has_files: bool,
    pub reused: u64,
}

impl Recorder {
    pub fn new(dir: &Path, populated: bool) -> Self {
        Self {
            spool: tempfile::spooled_tempfile_in(CHUNK, dir),
            seen: Seen::default(),
            encoder: Encoder::default(),
            populated,
            dir: dir.to_owned(),
            has_files: false,
            reused: 0,
        }
    }

    pub fn frame(&mut self, conn: &Connection, digest: Digest, raw: &[u8]) -> Result<u64> {
        let mut tag = 0;
        let mut cost = 0;
        let mut data = None;
        if self.seen.insert(digest)? {
            let size: Option<(usize, usize)> = if self.populated && raw.len() >= PROBE_MIN {
                conn.prepare_cached("SELECT length(data),external FROM chunks WHERE hash=?1 AND raw=?2 AND length(data)+external BETWEEN 1 AND raw")?
                    .query_row(params![&digest, raw.len()], |r| Ok((r.get(0)?,r.get(1)?))).optional()?
            } else {
                None
            };
            if let Some((inline, external)) = size {
                tag = EXISTING;
                cost = charge(inline + external, external != 0);
                self.reused += raw.len() as u64;
            } else {
                let bytes = self.encoder.encode(raw)?;
                cost = charge(bytes.len(), bytes.len() >= FILE_MIN);
                if bytes.len() == raw.len() && raw.len() >= PROBE_MIN {
                    // The original file already holds the exact publication
                    // payload. Verify it again before inserting it into SQLite.
                    tag = RAW_SOURCE;
                } else {
                    tag = bytes.len() as u32;
                    data = Some(bytes);
                }
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
        Ok(cost as u64)
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
            EXISTING | RAW_SOURCE => {
                if tag == EXISTING
                    && tx
                        .prepare_cached(
                            "SELECT EXISTS(SELECT 1 FROM chunks WHERE hash=?1 AND raw=?2)",
                        )?
                        .query_row(params![&digest, raw], |r| r.get::<_, bool>(0))?
                {
                    return Ok((digest, raw as u64));
                }
                // GC can evict an existing frame before publication. Both
                // it and a raw-source descriptor require the captured bytes.
                let file = match source {
                    Some(f) => f,
                    None => source.insert(crate::content::open_with_flags(path, libc::O_NONBLOCK)?),
                };
                let bytes = &mut buffer[..raw];
                file.read_exact_at(bytes, offset)?;
                ensure!(
                    blake3::hash(bytes).as_bytes() == &digest,
                    "compiler output changed before packed publication"
                );
                if tag == EXISTING {
                    self.reused -= raw as u64;
                    self.encoder.encode(bytes)?
                } else {
                    bytes
                }
            }
            n => {
                ensure!(n as usize <= raw, "invalid staged packed frame");
                let bytes = &mut buffer[..n as usize];
                self.spool.read_exact(bytes)?;
                bytes
            }
        };
        let (inline, external) = if bytes.len() >= FILE_MIN {
            (&[][..], bytes.len())
        } else {
            (bytes, 0)
        };
        let added = tx
            .prepare_cached(INSERT_CHUNK)?
            .execute(params![&digest, raw, inline, external])?;
        if added != 0 && external != 0 {
            self.has_files = true;
            write_frame(&self.dir, &digest, bytes)?;
        }
        Ok((digest, raw as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incompressible_payloads_stay_in_the_source_until_verified_publication() -> Result<()> {
        let d = tempfile::tempdir()?;
        let path = d.path().join("output");
        let mut store = super::super::Store::at(d.path().join("cache"), 4 << 20)?;
        let mut raw = vec![0; CHUNK];
        blake3::Hasher::new()
            .update(b"raw frame")
            .finalize_xof()
            .fill(&mut raw);
        let hash = *blake3::hash(&raw).as_bytes();
        let mut buffer = vec![0; CHUNK];
        for changed in [false, true] {
            let mut record = Recorder::new(&store.dir, false);
            assert_eq!(
                record.frame(&store.conn, hash, &raw)?,
                charge(CHUNK, true) as u64
            );
            assert_eq!(record.spool.stream_position()?, 40);
            record.rewind()?;
            std::fs::write(&path, &raw)?;
            if changed {
                File::options()
                    .write(true)
                    .open(&path)?
                    .write_all_at(&[!raw[77]], 77)?;
            }
            let tx = store.conn.transaction()?;
            let result = record.publish(&tx, &path, &mut None, 0, &mut buffer);
            if changed {
                assert!(result.is_err());
            } else {
                assert_eq!(result?, (hash, CHUNK as u64));
                let (inline, external): (Vec<u8>, usize) =
                    tx.query_row("SELECT data,external FROM chunks", [], |r| {
                        Ok((r.get(0)?, r.get(1)?))
                    })?;
                assert_eq!(external, CHUNK);
                assert_eq!(
                    payload(&store.dir, &hash, raw.len(), &inline, external, &mut buffer)?,
                    raw
                );
            }
            tx.rollback()?;
            assert_eq!(store.stats()?.charged_bytes, 0);
        }
        Ok(())
    }

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
