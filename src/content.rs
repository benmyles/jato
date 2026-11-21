//! Hash independent linker and rustc inputs using idle Cargo job slots.
use crate::fingerprint::{recent, stamp};
use crate::store::Digest;
use anyhow::{Context, Result, ensure};
use std::fs::{self, File, Metadata};
use std::path::Path;
use std::sync::{
    OnceLock,
    atomic::{AtomicUsize, Ordering},
};
use std::time::SystemTime;

static JOBSERVER: OnceLock<Option<jobserver::Client>> = OnceLock::new();

pub fn inherit_jobs() {
    JOBSERVER.get_or_init(|| {
        // Called at wrapper entry before opening the store or starting any
        // threads. Validate the inherited pipe descriptors before using them.
        unsafe { jobserver::Client::from_env_ext(true).client.ok() }
    });
}

pub struct Input<'a> {
    pub path: &'a Path,
    identity: [u64; 7],
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
        })
    }

    fn digest(&self) -> Result<Digest> {
        let mut file =
            File::open(self.path).with_context(|| format!("opening {}", self.path.display()))?;
        ensure!(
            stamp(&file.metadata()?) == self.identity,
            "input replaced before hashing"
        );
        let digest = blake3::Hasher::new().update_reader(&mut file)?.finalize();
        ensure!(
            stamp(&fs::metadata(self.path)?) == self.identity,
            "input changed while hashing"
        );
        Ok(*digest.as_bytes())
    }
}

pub fn digests(inputs: &[Input<'_>]) -> Result<Vec<Digest>> {
    let serial = || inputs.iter().map(Input::digest).collect();
    const BYTES_PER_WORKER: u64 = 2 * 1024 * 1024;
    let bytes = inputs
        .iter()
        .fold(0u64, |n, f| n.saturating_add(f.identity[2]));
    let workers = inputs
        .len()
        .min(4)
        .min(bytes.div_ceil(BYTES_PER_WORKER) as usize);
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

    // Start the largest files first while preserving compiler argument order
    // in the returned digests. Workers stream one file at a time.
    let mut order: Vec<_> = (0..inputs.len()).collect();
    order.sort_unstable_by_key(|&i| std::cmp::Reverse(inputs[i].identity[2]));
    let next = AtomicUsize::new(0);
    let result: Vec<_> = inputs.iter().map(|_| OnceLock::new()).collect();
    let work = || {
        while let Some(&i) = order.get(next.fetch_add(1, Ordering::Relaxed)) {
            let _ = result[i].set(inputs[i].digest());
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
    result
        .into_iter()
        .map(|slot| slot.into_inner().expect("all input jobs completed"))
        .collect()
}
