//! Optional shared / remote content-addressed cache (`file://` or `http://`).
//!
//! Configured via `JATO_REMOTE` or `jato remote <URL>`. Every remote entry and
//! object is cryptographically verified before entering the local cache:
//! - Entries are checked with `Manifest::decode_verified(&manifest, recipe, &key)`
//!   and `blake3(&detail) == manifest.detail`.
//! - Local inputs are verified with `manifest.holds()` BEFORE fetching any
//!   output objects.
//! - Every downloaded object is decompressed and checked against its expected
//!   BLAKE3 digest before adoption into the local store.
//!
//! Transport errors are non-fatal: if the remote is unreachable or damaged,
//! Jato falls back cleanly to the local cache and Cargo.
use crate::scan::Digest;
use anyhow::{Context, Result, anyhow, bail};
use std::collections::HashSet;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

const ENTRY_MAGIC: &[u8; 8] = b"JATOE1\0\0";
const MAX_VARIANTS: usize = 24;
const MAX_HTTP_ENTRY_BYTES: usize = 128 << 20; // 128 MiB cap on manifest+detail
const MAX_HTTP_RECIPE_BYTES: usize = 64 << 10; // 64 KiB cap on recipe key list
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
pub enum Backend {
    File(PathBuf),
    Http {
        host: String,
        port: u16,
        prefix: String,
        token: Option<String>,
    },
}

#[derive(Clone, Debug)]
pub struct Remote {
    pub url: String,
    pub backend: Backend,
    pub readonly: bool,
}

/// Decoded remote entry payload ready for verification and adoption.
pub struct RemoteEntry {
    pub cost_ms: u64,
    pub manifest: Vec<u8>,
    pub detail: Vec<u8>,
}

impl Remote {
    pub fn parse(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();
        let readonly = env::var_os("JATO_REMOTE_READONLY").is_some_and(|v| v == "1");
        let token = env::var("JATO_REMOTE_TOKEN")
            .ok()
            .filter(|t| !t.trim().is_empty());
        if let Some(rest) = trimmed.strip_prefix("file://") {
            let path = PathBuf::from(rest);
            if !path.is_absolute() {
                bail!("remote file:// path must be absolute: {trimmed}");
            }
            return Ok(Self {
                url: format!("file://{}", path.display()),
                backend: Backend::File(path),
                readonly,
            });
        }
        if trimmed.starts_with('/') {
            let path = PathBuf::from(trimmed);
            return Ok(Self {
                url: format!("file://{}", path.display()),
                backend: Backend::File(path),
                readonly,
            });
        }
        if let Some(rest) = trimmed.strip_prefix("http://") {
            let (authority, prefix) = match rest.find('/') {
                Some(idx) => (&rest[..idx], rest[idx..].trim_end_matches('/').to_owned()),
                None => (rest, String::new()),
            };
            if authority.is_empty() {
                bail!("missing host in remote URL: {trimmed}");
            }
            let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
                let (h, tail) = bracketed
                    .split_once(']')
                    .ok_or_else(|| anyhow!("unclosed IPv6 host in remote URL: {trimmed}"))?;
                let port = match tail.strip_prefix(':') {
                    Some(p) => p.parse::<u16>().context("invalid port in remote URL")?,
                    None if tail.is_empty() => 80,
                    None => bail!("invalid authority in remote URL: {trimmed}"),
                };
                (h.to_owned(), port)
            } else if let Some((h, p)) = authority.rsplit_once(':') {
                (
                    h.to_owned(),
                    p.parse::<u16>().context("invalid port in remote URL")?,
                )
            } else {
                (authority.to_owned(), 80)
            };
            if host.is_empty() {
                bail!("empty host in remote URL: {trimmed}");
            }
            return Ok(Self {
                url: trimmed.trim_end_matches('/').to_owned(),
                backend: Backend::Http {
                    host,
                    port,
                    prefix,
                    token,
                },
                readonly,
            });
        }
        bail!(
            "unsupported remote URL {trimmed:?} (use file:///path or http://host[:port][/prefix])"
        );
    }

    /// Fetch the list of candidate entry keys recorded for `recipe` (newest first).
    pub fn recipe_keys(&self, recipe: &Digest) -> Result<Vec<Digest>> {
        let hex = hex_of(recipe);
        let rel = format!("v1/recipes/{}/{hex}", &hex[..2]);
        let Some(bytes) = self.get_bytes(&rel, MAX_HTTP_RECIPE_BYTES)? else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for line in String::from_utf8_lossy(&bytes).lines() {
            if let Some(d) = parse_hex_digest(line.trim())
                && seen.insert(d)
            {
                out.push(d);
                if out.len() >= MAX_VARIANTS {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Fetch and decompress a remote entry (`manifest` + `detail` + `cost_ms`).
    pub fn fetch_entry(&self, key: &Digest) -> Result<Option<RemoteEntry>> {
        let hex = hex_of(key);
        let rel = format!("v1/entries/{}/{hex}.zst", &hex[..2]);
        let Some(compressed) = self.get_bytes(&rel, MAX_HTTP_ENTRY_BYTES)? else {
            return Ok(None);
        };
        let mut raw = Vec::new();
        let mut dec = zstd::stream::read::Decoder::new(&compressed[..])?;
        dec.by_ref()
            .take(MAX_HTTP_ENTRY_BYTES as u64 + 1)
            .read_to_end(&mut raw)?;
        if raw.len() > MAX_HTTP_ENTRY_BYTES {
            bail!("remote entry exceeds size limit");
        }
        Ok(decode_entry_frame(&raw))
    }

    /// Fetch a remote object (stored zstd-compressed at `v1/objects/<aa>/<hex>.zst`),
    /// stream-decompress into `dest`, verify its decompressed BLAKE3 digest equals
    /// `digest`, and optionally keep it compressed if `keep_packed` is true.
    /// Returns `(size, stored_bytes, packed)` on success.
    pub fn fetch_object_to(
        &self,
        digest: &Digest,
        dest: &Path,
        keep_packed: bool,
    ) -> Result<Option<(u64, u64, bool)>> {
        let hex = hex_of(digest);
        let rel = format!("v1/objects/{}/{hex}.zst", &hex[..2]);
        let Some(mut reader) = self.open_stream(&rel)? else {
            return Ok(None);
        };
        if keep_packed {
            // Write compressed bytes to `dest` while streaming a zstd decoder
            // over the bytes in parallel to verify the uncompressed BLAKE3 digest!
            let mut out = File::create(dest)?;
            let mut compressed_buf = Vec::new();
            let mut stored_bytes = 0u64;
            let mut chunk = [0u8; 64 << 10];
            loop {
                let n = reader.read(&mut chunk)?;
                if n == 0 {
                    break;
                }
                out.write_all(&chunk[..n])?;
                compressed_buf.extend_from_slice(&chunk[..n]);
                stored_bytes += n as u64;
            }
            out.flush()?;
            drop(out);
            let mut dec = zstd::stream::read::Decoder::new(&compressed_buf[..])?;
            let mut hasher = blake3::Hasher::new();
            let mut size = 0u64;
            loop {
                let n = dec.read(&mut chunk)?;
                if n == 0 {
                    break;
                }
                hasher.update(&chunk[..n]);
                size += n as u64;
            }
            if hasher.finalize().as_bytes() != digest {
                let _ = fs::remove_file(dest);
                bail!("remote object {hex} failed BLAKE3 verification");
            }
            Ok(Some((size, stored_bytes, true)))
        } else {
            // Stream-decompress directly to `dest` while hashing with BLAKE3.
            let mut dec = zstd::stream::read::Decoder::new(reader)?;
            let mut out = File::create(dest)?;
            let mut hasher = blake3::Hasher::new();
            let mut size = 0u64;
            let mut buf = [0u8; 64 << 10];
            loop {
                let n = dec.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                out.write_all(&buf[..n])?;
                hasher.update(&buf[..n]);
                size += n as u64;
            }
            out.flush()?;
            drop(out);
            if hasher.finalize().as_bytes() != digest {
                let _ = fs::remove_file(dest);
                bail!("remote object {hex} failed BLAKE3 verification");
            }
            Ok(Some((size, size, false)))
        }
    }

    /// Push an entry and its missing objects to the remote cache.
    #[allow(clippy::too_many_arguments)]
    pub fn push_entry(
        &self,
        local_dir: &Path,
        recipe: &Digest,
        key: &Digest,
        manifest_bytes: &[u8],
        detail: &[u8],
        cost_ms: u64,
        digests: &[Digest],
    ) -> Result<()> {
        if self.readonly {
            return Ok(());
        }
        // Upload missing objects in parallel (up to 8 concurrent workers).
        let missing: Vec<Digest> = digests.to_vec();
        if !missing.is_empty() {
            let threads = std::thread::available_parallelism()
                .map_or(4, |n| n.get())
                .clamp(1, 8)
                .min(missing.len());
            let each = missing.len().div_ceil(threads);
            let err: std::sync::Mutex<Option<anyhow::Error>> = std::sync::Mutex::new(None);
            std::thread::scope(|scope| {
                for chunk in missing.chunks(each) {
                    let err = &err;
                    scope.spawn(move || {
                        for d in chunk {
                            if err.lock().unwrap_or_else(|e| e.into_inner()).is_some() {
                                return;
                            }
                            if let Err(e) = self.push_object(local_dir, d) {
                                *err.lock().unwrap_or_else(|e| e.into_inner()) = Some(e);
                                return;
                            }
                        }
                    });
                }
            });
            if let Some(e) = err.into_inner().unwrap_or_else(|e| e.into_inner()) {
                return Err(e);
            }
        }
        // Upload the entry frame (zstd-compressed).
        let key_hex = hex_of(key);
        let entry_rel = format!("v1/entries/{}/{key_hex}.zst", &key_hex[..2]);
        if !self.exists(&entry_rel)? {
            let frame = encode_entry_frame(cost_ms, manifest_bytes, detail)?;
            let compressed = zstd::bulk::compress(&frame, 3)?;
            self.put_bytes(&entry_rel, &compressed)?;
        }
        // Update the recipe key list.
        self.record_recipe_key(recipe, key)?;
        Ok(())
    }

    fn push_object(&self, local_dir: &Path, digest: &Digest) -> Result<()> {
        let hex = hex_of(digest);
        let rel = format!("v1/objects/{}/{hex}.zst", &hex[..2]);
        if self.exists(&rel)? {
            return Ok(());
        }
        let packed_path = crate::cas::object_path(local_dir, digest, true);
        if packed_path.is_file() {
            let data = fs::read(&packed_path)?;
            return self.put_bytes(&rel, &data);
        }
        let raw_path = crate::cas::object_path(local_dir, digest, false);
        if !raw_path.is_file() {
            bail!("local object {hex} missing before remote push");
        }
        let mut src = File::open(&raw_path)?;
        let mut compressed = Vec::new();
        {
            let mut enc = zstd::stream::write::Encoder::new(&mut compressed, 3)?;
            io::copy(&mut src, &mut enc)?;
            enc.finish()?;
        }
        self.put_bytes(&rel, &compressed)
    }

    fn record_recipe_key(&self, recipe: &Digest, key: &Digest) -> Result<()> {
        let hex = hex_of(recipe);
        let key_hex = hex_of(key);
        let rel = format!("v1/recipes/{}/{hex}", &hex[..2]);
        match &self.backend {
            Backend::File(root) => {
                let target = root.join(&rel);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                let lock_path = target.with_extension("lock");
                let lock_file = OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .write(true)
                    .open(&lock_path)?;
                lock_file.lock()?;
                let existing = fs::read_to_string(&target).unwrap_or_default();
                let updated = merge_recipe_lines(&key_hex, &existing);
                let tmp = target.with_extension(format!("tmp.{}", std::process::id()));
                fs::write(&tmp, updated)?;
                if fs::rename(&tmp, &target).is_err() {
                    let _ = fs::remove_file(&tmp);
                }
                drop(lock_file);
                Ok(())
            }
            Backend::Http { .. } => {
                let existing = self
                    .get_bytes(&rel, MAX_HTTP_RECIPE_BYTES)?
                    .map(|b| String::from_utf8_lossy(&b).into_owned())
                    .unwrap_or_default();
                let updated = merge_recipe_lines(&key_hex, &existing);
                self.put_bytes(&rel, updated.as_bytes())
            }
        }
    }

    fn exists(&self, rel: &str) -> Result<bool> {
        match &self.backend {
            Backend::File(root) => Ok(root.join(rel).is_file()),
            Backend::Http {
                host,
                port,
                prefix,
                token,
            } => http_head(host, *port, prefix, rel, token.as_deref()),
        }
    }

    fn get_bytes(&self, rel: &str, max_bytes: usize) -> Result<Option<Vec<u8>>> {
        match &self.backend {
            Backend::File(root) => match fs::read(root.join(rel)) {
                Ok(b) if b.len() <= max_bytes => Ok(Some(b)),
                Ok(_) => bail!("remote file {rel} exceeds size limit"),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            },
            Backend::Http {
                host,
                port,
                prefix,
                token,
            } => http_get(host, *port, prefix, rel, token.as_deref(), max_bytes),
        }
    }

    fn open_stream(&self, rel: &str) -> Result<Option<Box<dyn Read>>> {
        match &self.backend {
            Backend::File(root) => match File::open(root.join(rel)) {
                Ok(f) => Ok(Some(Box::new(f))),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            },
            Backend::Http {
                host,
                port,
                prefix,
                token,
            } => http_get_stream(host, *port, prefix, rel, token.as_deref()),
        }
    }

    fn put_bytes(&self, rel: &str, data: &[u8]) -> Result<()> {
        match &self.backend {
            Backend::File(root) => {
                let target = root.join(rel);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                let tmp = target.with_extension(format!(
                    "tmp.{}.{:x}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .subsec_nanos()
                ));
                fs::write(&tmp, data)?;
                if let Err(e) = fs::rename(&tmp, &target) {
                    let _ = fs::remove_file(&tmp);
                    return Err(e.into());
                }
                Ok(())
            }
            Backend::Http {
                host,
                port,
                prefix,
                token,
            } => http_put(host, *port, prefix, rel, token.as_deref(), data),
        }
    }
}

fn hex_of(d: &Digest) -> String {
    blake3::Hash::from(*d).to_hex().to_string()
}

fn parse_hex_digest(s: &str) -> Option<Digest> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn merge_recipe_lines(new_key: &str, existing: &str) -> String {
    let mut out = String::with_capacity(existing.len() + 65);
    out.push_str(new_key);
    out.push('\n');
    let mut count = 1;
    for line in existing.lines() {
        let line = line.trim();
        if line.len() == 64 && line != new_key && parse_hex_digest(line).is_some() {
            out.push_str(line);
            out.push('\n');
            count += 1;
            if count >= MAX_VARIANTS {
                break;
            }
        }
    }
    out
}

fn encode_entry_frame(cost_ms: u64, manifest: &[u8], detail: &[u8]) -> Result<Vec<u8>> {
    let mlen = u32::try_from(manifest.len()).context("manifest too large for remote")?;
    let dlen = u32::try_from(detail.len()).context("detail too large for remote")?;
    let mut buf = Vec::with_capacity(24 + manifest.len() + detail.len());
    buf.extend_from_slice(ENTRY_MAGIC);
    buf.extend_from_slice(&cost_ms.to_le_bytes());
    buf.extend_from_slice(&mlen.to_le_bytes());
    buf.extend_from_slice(&dlen.to_le_bytes());
    buf.extend_from_slice(manifest);
    buf.extend_from_slice(detail);
    Ok(buf)
}

fn decode_entry_frame(raw: &[u8]) -> Option<RemoteEntry> {
    if raw.len() < 24 || &raw[..8] != ENTRY_MAGIC {
        return None;
    }
    let cost_ms = u64::from_le_bytes(raw[8..16].try_into().ok()?);
    let mlen = u32::from_le_bytes(raw[16..20].try_into().ok()?) as usize;
    let dlen = u32::from_le_bytes(raw[20..24].try_into().ok()?) as usize;
    if 24usize.checked_add(mlen)?.checked_add(dlen)? != raw.len() {
        return None;
    }
    Some(RemoteEntry {
        cost_ms,
        manifest: raw[24..24 + mlen].to_vec(),
        detail: raw[24 + mlen..].to_vec(),
    })
}

// ---- minimal zero-dependency HTTP/1.1 client ------------------------------

fn connect_http(host: &str, port: u16) -> Result<TcpStream> {
    let mut last_err = None;
    let addrs = (host, port).to_socket_addrs()?;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(stream) => {
                let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
                let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
                return Ok(stream);
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err
        .map(anyhow::Error::from)
        .unwrap_or_else(|| anyhow!("could not resolve {host}:{port}")))
}

fn request_path(prefix: &str, rel: &str) -> String {
    if prefix.is_empty() {
        format!("/{rel}")
    } else {
        format!("{prefix}/{rel}")
    }
}

fn host_header(host: &str, port: u16) -> String {
    if host.contains(':') {
        if port == 80 {
            format!("[{host}]")
        } else {
            format!("[{host}]:{port}")
        }
    } else if port == 80 {
        host.to_owned()
    } else {
        format!("{host}:{port}")
    }
}

struct HttpHead {
    status: u16,
    content_length: Option<u64>,
    chunked: bool,
}

fn read_http_head(reader: &mut BufReader<TcpStream>) -> Result<HttpHead> {
    let mut status_line = String::new();
    let n = reader.read_line(&mut status_line)?;
    if n == 0 {
        bail!("empty HTTP response");
    }
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| anyhow!("malformed HTTP status line: {status_line:?}"))?;
    let mut content_length = None;
    let mut chunked = false;
    let mut header_count = 0;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        header_count += 1;
        if header_count > 128 {
            bail!("too many HTTP headers");
        }
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim();
            let v = v.trim();
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.parse::<u64>().ok();
            } else if k.eq_ignore_ascii_case("transfer-encoding")
                && v.to_ascii_lowercase().contains("chunked")
            {
                chunked = true;
            }
        }
    }
    Ok(HttpHead {
        status,
        content_length,
        chunked,
    })
}

fn write_auth(req: &mut String, token: Option<&str>) {
    if let Some(t) = token {
        req.push_str("Authorization: Bearer ");
        req.push_str(t);
        req.push_str("\r\n");
    }
}

fn http_head(host: &str, port: u16, prefix: &str, rel: &str, token: Option<&str>) -> Result<bool> {
    let mut stream = connect_http(host, port)?;
    let path = request_path(prefix, rel);
    let mut req = format!(
        "HEAD {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
        host_header(host, port)
    );
    write_auth(&mut req, token);
    req.push_str("\r\n");
    stream.write_all(req.as_bytes())?;
    let mut reader = BufReader::new(stream);
    let head = read_http_head(&mut reader)?;
    match head.status {
        200 | 204 => Ok(true),
        404 => Ok(false),
        other => bail!("HTTP HEAD {path} returned {other}"),
    }
}

fn http_put(
    host: &str,
    port: u16,
    prefix: &str,
    rel: &str,
    token: Option<&str>,
    body: &[u8],
) -> Result<()> {
    let mut stream = connect_http(host, port)?;
    let path = request_path(prefix, rel);
    let mut req = format!(
        "PUT {path} HTTP/1.1\r\nHost: {}\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n",
        host_header(host, port),
        body.len()
    );
    write_auth(&mut req, token);
    req.push_str("\r\n");
    stream.write_all(req.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let head = read_http_head(&mut reader)?;
    if (200..300).contains(&head.status) {
        Ok(())
    } else {
        bail!("HTTP PUT {path} returned {}", head.status)
    }
}

struct ChunkedReader<R> {
    inner: R,
    remaining: usize,
    done: bool,
}

impl<R: BufRead> Read for ChunkedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done {
            return Ok(0);
        }
        while self.remaining == 0 {
            let mut line = String::new();
            let n = self.inner.read_line(&mut line)?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated HTTP chunked body",
                ));
            }
            let size_hex = line.split(';').next().unwrap_or("").trim();
            let size = usize::from_str_radix(size_hex, 16)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad chunk size"))?;
            if size == 0 {
                self.done = true;
                return Ok(0);
            }
            self.remaining = size;
        }
        let want = buf.len().min(self.remaining);
        let n = self.inner.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated HTTP chunk",
            ));
        }
        self.remaining -= n;
        if self.remaining == 0 {
            let mut crlf = [0u8; 2];
            self.inner.read_exact(&mut crlf)?;
        }
        Ok(n)
    }
}

fn http_get_stream(
    host: &str,
    port: u16,
    prefix: &str,
    rel: &str,
    token: Option<&str>,
) -> Result<Option<Box<dyn Read>>> {
    let mut stream = connect_http(host, port)?;
    let path = request_path(prefix, rel);
    let mut req = format!(
        "GET {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
        host_header(host, port)
    );
    write_auth(&mut req, token);
    req.push_str("\r\n");
    stream.write_all(req.as_bytes())?;
    let mut reader = BufReader::new(stream);
    let head = read_http_head(&mut reader)?;
    match head.status {
        200 => {
            if head.chunked {
                Ok(Some(Box::new(ChunkedReader {
                    inner: reader,
                    remaining: 0,
                    done: false,
                })))
            } else if let Some(len) = head.content_length {
                Ok(Some(Box::new(reader.take(len))))
            } else {
                Ok(Some(Box::new(reader)))
            }
        }
        404 => Ok(None),
        other => bail!("HTTP GET {path} returned {other}"),
    }
}

fn http_get(
    host: &str,
    port: u16,
    prefix: &str,
    rel: &str,
    token: Option<&str>,
    max_bytes: usize,
) -> Result<Option<Vec<u8>>> {
    let Some(mut stream) = http_get_stream(host, port, prefix, rel, token)? else {
        return Ok(None);
    };
    let mut out = Vec::new();
    stream
        .by_ref()
        .take(max_bytes as u64 + 1)
        .read_to_end(&mut out)?;
    if out.len() > max_bytes {
        bail!("HTTP response for {rel} exceeds {max_bytes} bytes");
    }
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_file_and_http_remote_urls() {
        let f = Remote::parse("file:///var/cache/jato-shared").unwrap();
        assert_eq!(f.url, "file:///var/cache/jato-shared");
        assert!(
            matches!(f.backend, Backend::File(ref p) if p == Path::new("/var/cache/jato-shared"))
        );

        let plain = Remote::parse("/srv/jato").unwrap();
        assert_eq!(plain.url, "file:///srv/jato");

        let h = Remote::parse("http://127.0.0.1:8080/cache/").unwrap();
        assert_eq!(h.url, "http://127.0.0.1:8080/cache");
        assert!(matches!(
            h.backend,
            Backend::Http { ref host, port: 8080, ref prefix, .. }
            if host == "127.0.0.1" && prefix == "/cache"
        ));

        let v6 = Remote::parse("http://[::1]:9000").unwrap();
        assert!(matches!(
            v6.backend,
            Backend::Http { ref host, port: 9000, ref prefix, .. }
            if host == "::1" && prefix.is_empty()
        ));

        assert!(Remote::parse("ftp://example.com").is_err());
        assert!(Remote::parse("file://relative/path").is_err());
    }

    #[test]
    fn entry_frame_round_trip_and_chunked_reader() {
        let frame = encode_entry_frame(1234, b"manifest-bytes", b"detail-bytes").unwrap();
        let dec = decode_entry_frame(&frame).unwrap();
        assert_eq!(dec.cost_ms, 1234);
        assert_eq!(dec.manifest, b"manifest-bytes");
        assert_eq!(dec.detail, b"detail-bytes");
        assert!(decode_entry_frame(&frame[..10]).is_none());

        let chunked = b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let mut r = ChunkedReader {
            inner: BufReader::new(&chunked[..]),
            remaining: 0,
            done: false,
        };
        let mut out = String::new();
        r.read_to_string(&mut out).unwrap();
        assert_eq!(out, "hello world");
    }
}
