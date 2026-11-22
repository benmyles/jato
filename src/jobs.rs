//! Share the machine's processors among builds.
//!
//! Cargo runs as many compilers at once as there are processors. Five
//! builds started together run five times that, each compiler gets a
//! fifth of a processor and all of its memory, and every build takes far
//! longer than five in a row would have. Compilers started through Jato
//! take a place first. There are as many places as processors, shared by
//! every build that uses the same cache, so the builds queue for the
//! machine instead of dividing it.
//!
//! A place is a lock on a file, which the kernel gives up when its holder
//! ends in any way. Whoever waits holds a second lock, the turn, while
//! looking for a free place, so that places go to those who have waited
//! and not to whoever asked last.
use std::fs::{self, File, TryLockError};
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

/// Set for a compiler that holds a place, so that a compiler it starts in
/// turn does not wait for a second one.
pub const HELD: &str = "JATO_JOB";

/// A place among the running compilers.
pub struct Place {
    file: Option<File>,
    /// How long it took to get.
    pub waited: Duration,
}

/// How many compilers may run at once. Zero means any number.
pub fn limit() -> usize {
    match std::env::var("JATO_JOBS") {
        Ok(value) => value.trim().parse().unwrap_or(0),
        Err(_) => std::thread::available_parallelism().map_or(0, |n| n.get()),
    }
}

fn lock(file: &File) -> io::Result<()> {
    loop {
        match file.lock() {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => (),
            done => return done,
        }
    }
}

/// Wait for a place. Anything that goes wrong with the locks themselves
/// lets the compiler run: a build must never stop for want of bookkeeping.
pub fn take(cache: &Path) -> Place {
    if std::env::var_os(HELD).is_some() {
        return Place::free();
    }
    take_one_of(cache, limit())
}

fn take_one_of(cache: &Path, limit: usize) -> Place {
    if limit == 0 {
        return Place::free();
    }
    let dir = cache.join("locks").join("jobs");
    let open = |name: &str| {
        fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(name))
    };
    let begun = Instant::now();
    let Ok(turn) = fs::create_dir_all(&dir).and_then(|()| open("turn")) else {
        return Place::free();
    };
    if lock(&turn).is_err() {
        return Place::free();
    }
    let mut places: Vec<Option<File>> = (0..limit).map(|_| None).collect();
    loop {
        for (i, place) in places.iter_mut().enumerate() {
            if place.is_none() {
                *place = open(&i.to_string()).ok();
            }
            let Some(file) = place else {
                return Place::free();
            };
            match file.try_lock() {
                Ok(()) => {
                    return Place {
                        file: place.take(),
                        waited: begun.elapsed(),
                    };
                }
                Err(TryLockError::WouldBlock) => (),
                Err(TryLockError::Error(_)) => return Place::free(),
            }
        }
        // Only the holder of the turn looks, so looking often costs little.
        std::thread::sleep(Duration::from_millis(2));
    }
}

impl Place {
    fn free() -> Self {
        Self {
            file: None,
            waited: Duration::ZERO,
        }
    }

    /// Keep the place for the program this process is about to become.
    pub fn keep_through_exec(mut self) {
        use std::os::fd::AsRawFd;
        if let Some(file) = self.file.take() {
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, 0) };
            std::mem::forget(file);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn no_more_than_the_limit_hold_a_place_and_the_next_gets_one_freed() {
        let dir = tempfile::tempdir().unwrap();
        let first = take_one_of(dir.path(), 2);
        let second = take_one_of(dir.path(), 2);
        assert!(first.file.is_some() && second.file.is_some());
        let (sent, got) = mpsc::channel();
        let cache = dir.path().to_owned();
        let waiter = std::thread::spawn(move || {
            let third = take_one_of(&cache, 2);
            sent.send(third.waited).unwrap();
            third
        });
        assert!(got.recv_timeout(Duration::from_millis(150)).is_err());
        drop(first);
        let waited = got.recv_timeout(Duration::from_secs(20)).unwrap();
        assert!(waited >= Duration::from_millis(100), "{waited:?}");
        let third = waiter.join().unwrap();
        assert!(third.file.is_some());
        // No limit, and no directory to keep locks in, both mean go ahead.
        assert!(take_one_of(dir.path(), 0).file.is_none());
        assert!(take_one_of(Path::new("/proc/nowhere"), 2).file.is_none());
        drop((second, third));
        assert!(take_one_of(dir.path(), 1).file.is_some());
    }
}
