//! Hash independent linker and rustc inputs using idle Cargo job slots.
use crate::fingerprint::{recent, stamp};
use crate::store::Digest;
use anyhow::{Context, Result, ensure};
use blake3::hazmat::{HasherExt, Mode, merge_subtrees_non_root, merge_subtrees_root};
use std::fs::{self, File, Metadata};
use std::io::Read;
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::{
    OnceLock,
    atomic::{AtomicUsize, Ordering},
};
use std::time::SystemTime;

static JOBSERVER: OnceLock<Option<jobserver::Client>> = OnceLock::new();

pub fn open(path: &Path) -> std::io::Result<File> {
    // Callers expect a regular file. A replacement FIFO between stat and open
    // must not block the compiler.
    open_with_flags(path, libc::O_NONBLOCK)
}

pub(crate) fn open_with_flags(path: &Path, flags: libc::c_int) -> std::io::Result<File> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        match fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOATIME | flags)
            .open(path)
        {
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::PermissionDenied
                        | std::io::ErrorKind::InvalidInput
                        | std::io::ErrorKind::Unsupported
                ) => {}
            result => return result,
        }
    }
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(path)
}

pub fn inherit_jobs() {
    JOBSERVER.get_or_init(|| {
        // Called at wrapper entry before opening the store or starting any
        // threads. Validate the inherited pipe descriptors before using them.
        unsafe { jobserver::Client::from_env_ext(true).client.ok() }
    });
}

pub struct Input<'a> {
    path: &'a Path,
    identity: [u64; 7],
    pub(crate) modified: SystemTime,
    handle: Option<File>,
}

struct Positioned<'a> {
    file: &'a File,
    offset: u64,
}

impl Read for Positioned<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let n = self.file.read_at(buffer, self.offset)?;
        self.offset += n as u64;
        Ok(n)
    }
}

impl<'a> Input<'a> {
    pub fn new(path: &'a Path, meta: &Metadata, since: SystemTime) -> Result<Self> {
        ensure!(
            meta.is_file() && !recent(meta, since),
            "input changed during compilation: {}",
            path.display()
        );
        Ok(Self {
            path,
            identity: stamp(meta),
            modified: meta.modified()?,
            handle: None,
        })
    }

    pub fn with_file(path: &'a Path, file: File, since: SystemTime) -> Result<Self> {
        let mut input = Self::new(path, &file.metadata()?, since)?;
        input.handle = Some(file);
        Ok(input)
    }

    pub fn size(&self) -> u64 {
        self.identity[2]
    }

    pub fn path(&self) -> &Path {
        self.path
    }

    fn read(&self, range: Range<u64>) -> Result<Digest> {
        let opened;
        let file = match &self.handle {
            Some(f) => f,
            None => {
                opened =
                    open(self.path).with_context(|| format!("opening {}", self.path.display()))?;
                ensure!(
                    stamp(&opened.metadata()?) == self.identity,
                    "input replaced before hashing"
                );
                &opened
            }
        };
        let whole = range.start == 0 && range.end == self.identity[2];
        let mut h = blake3::Hasher::new();
        if !whole {
            h.set_input_offset(range.start);
        }
        let mut reader = Positioned {
            file,
            offset: range.start,
        }
        .take(range.end - range.start);
        h.update_reader(&mut reader)?;
        ensure!(
            reader.limit() == 0 && stamp(&fs::metadata(self.path)?) == self.identity,
            "input changed while hashing"
        );
        Ok(if whole {
            *h.finalize().as_bytes()
        } else {
            h.finalize_non_root()
        })
    }
}

// Equal, aligned power-of-two stripes are BLAKE3 subtrees. Only the final
// stripe can be short. At most four descriptors per input are needed even for
// multi-gigabyte files. Regular reads allow a concurrent truncation to fail cleanly.
const MIN_STRIPE: u64 = 2 * 1024 * 1024;
fn ranges(size: u64, workers: usize) -> impl Iterator<Item = Range<u64>> {
    let width = size
        .div_ceil(workers as u64)
        .next_power_of_two()
        .max(MIN_STRIPE);
    let count = size.div_ceil(width).max(1);
    (0..count).map(move |i| (i * width)..size.min((i + 1) * width))
}

fn subtree(parts: &[Digest]) -> Digest {
    if let [one] = parts {
        return *one;
    }
    let (left, right) = parts.split_at(parts.len().next_power_of_two() / 2);
    merge_subtrees_non_root(&subtree(left), &subtree(right), Mode::Hash)
}

pub(crate) fn root(parts: &[Digest]) -> Digest {
    if let [whole] = parts {
        return *whole;
    }
    let (left, right) = parts.split_at(parts.len().next_power_of_two() / 2);
    *merge_subtrees_root(&subtree(left), &subtree(right), Mode::Hash).as_bytes()
}

pub fn digests(inputs: &[Input<'_>], bytes_per_worker: u64) -> Result<Vec<Digest>> {
    let serial = || {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            // Overlap a small amount of I/O for actual dependencies on the
            // serial path. This also works with cargo -j1 on fresh reflinks.
            let mut remaining = 2 << 20;
            for input in inputs.iter().skip(1) {
                if let Some(file) = &input.handle
                    && input.identity[2] >= 256 << 10
                {
                    let count = input.identity[2].min(512 << 10).min(remaining);
                    unsafe {
                        libc::posix_fadvise(
                            file.as_raw_fd(),
                            0,
                            count as libc::off_t,
                            libc::POSIX_FADV_WILLNEED,
                        );
                    }
                    remaining -= count;
                    if remaining == 0 {
                        break;
                    }
                }
            }
        }
        inputs.iter().map(|i| i.read(0..i.identity[2])).collect()
    };
    let bytes = inputs
        .iter()
        .fold(0u64, |n, f| n.saturating_add(f.identity[2]));
    let workers = bytes.div_ceil(bytes_per_worker).min(4) as usize;
    let Some(server) = JOBSERVER
        .get()
        .and_then(Option::as_ref)
        .filter(|_| workers > 1)
    else {
        return serial();
    };
    let permits: Vec<_> = (1..workers)
        .map_while(|_| server.try_acquire().ok().flatten())
        .collect();
    if permits.is_empty() {
        return serial();
    }
    let mut tasks = Vec::new();
    let mut groups = Vec::with_capacity(inputs.len());
    for (i, input) in inputs.iter().enumerate() {
        let start = tasks.len();
        tasks.extend(ranges(input.identity[2], permits.len() + 1).map(|span| (i, span)));
        groups.push(start..tasks.len());
    }
    let mut order: Vec<_> = (0..tasks.len()).collect();
    order.sort_unstable_by_key(|&i| std::cmp::Reverse(tasks[i].1.end - tasks[i].1.start));
    let next = AtomicUsize::new(0);
    let result: Vec<_> = tasks.iter().map(|_| OnceLock::new()).collect();
    let work = || {
        while let Some(&i) = order.get(next.fetch_add(1, Ordering::Relaxed)) {
            let (input, span) = &tasks[i];
            let _ = result[i].set(inputs[*input].read(span.clone()));
        }
    };
    std::thread::scope(|scope| {
        for permit in permits {
            // If a worker cannot start, the caller still drains its work.
            let worker = &work;
            let _ = std::thread::Builder::new()
                .stack_size(512 * 1024)
                .spawn_scoped(scope, move || {
                    let _permit = permit;
                    worker();
                });
        }
        work();
    });
    let digests = result
        .into_iter()
        .map(|slot| slot.into_inner().expect("all input jobs completed"))
        .collect::<Result<Vec<_>>>()?;
    Ok(groups
        .into_iter()
        .map(|group| root(&digests[group]))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_retained_descriptor_cannot_vouch_for_a_replaced_path() -> Result<()> {
        let d = tempfile::tempdir()?;
        let file = d.path().join("dependency");
        let replacement = d.path().join("replacement");
        fs::write(&file, b"first")?;
        let input = Input::with_file(
            &file,
            open(&file)?,
            SystemTime::now() + std::time::Duration::from_secs(1),
        )?;
        fs::write(&replacement, b"other")?;
        fs::rename(replacement, &file)?;
        assert!(input.read(0..5).is_err());
        Ok(())
    }

    #[test]
    fn streamed_stripes_match_standard_blake3_for_every_tree_shape() -> Result<()> {
        let d = tempfile::tempdir()?;
        let path = d.path().join("input");
        let bytes: Vec<_> = (0..(8 * MIN_STRIPE as usize + 1))
            .map(|i| (i.wrapping_mul(31337) >> 5) as u8)
            .collect();
        for size in [
            0,
            1,
            1023,
            1024,
            1025,
            MIN_STRIPE - 1,
            MIN_STRIPE,
            MIN_STRIPE + 1,
            3 * MIN_STRIPE - 1,
            3 * MIN_STRIPE,
            4 * MIN_STRIPE + 1,
            8 * MIN_STRIPE + 1,
        ] {
            let data = &bytes[..size as usize];
            fs::write(&path, data)?;
            let since = SystemTime::now() + std::time::Duration::from_secs(1);
            let input = Input::new(&path, &fs::metadata(&path)?, since)?;
            for workers in 1..=4 {
                let parts = ranges(size, workers)
                    .map(|r| input.read(r))
                    .collect::<Result<Vec<_>>>()?;
                assert_eq!(
                    root(&parts),
                    *blake3::hash(data).as_bytes(),
                    "{size} bytes, {workers} workers"
                );
            }
        }
        Ok(())
    }
}
