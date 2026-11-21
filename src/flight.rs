//! Coordinate overlapping misses. No peer supplies artifacts or diagnostics.
//! A wakeup only asks this wrapper to verify its own inputs a second time.
use crate::store::Digest;
use std::path::Path;

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::ffi::OsStr;
    use std::io::{self, Read};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::{
        fs::MetadataExt,
        net::{SocketAddr, UnixListener},
    };
    use std::time::{Duration, Instant};

    pub struct Ticket(Role);
    enum Role {
        // Closing this listener wakes every queued peer, including on exec
        // or SIGKILL. There is no server thread and no filesystem rendezvous.
        Owner { _listener: UnixListener },
        Peer([u8; 40]),
    }

    impl Ticket {
        pub fn enter(dir: &Path, recipe: &Digest, root: &Path) -> Option<Self> {
            if std::env::var_os("JATO_COALESCE").as_deref() == Some(OsStr::new("0")) {
                return None;
            }
            let cache = std::fs::metadata(dir).ok()?;
            let mut h = blake3::Hasher::new_keyed(recipe);
            h.update(&cache.dev().to_le_bytes());
            h.update(&cache.ino().to_le_bytes());
            h.update(&unsafe { libc::geteuid() }.to_le_bytes());
            // Different top-level edits can compile concurrently. The prefix
            // is only a scheduling hint; full content determines every hit.
            let mut prefix = [0; 4096];
            let mut source = crate::content::open_with_flags(root, libc::O_NONBLOCK).ok()?;
            if !source.metadata().ok()?.is_file() {
                return None;
            }
            let n = source.read(&mut prefix).ok()?;
            h.update(&prefix[..n]);
            let mut name = [0; 40];
            name[..8].copy_from_slice(b"jato-job");
            name[8..].copy_from_slice(h.finalize().as_bytes());
            let addr = SocketAddr::from_abstract_name(name).ok()?;
            match UnixListener::bind_addr(&addr) {
                Ok(listener) => Some(Self(Role::Owner {
                    _listener: listener,
                })),
                Err(e) if e.kind() == io::ErrorKind::AddrInUse => Some(Self(Role::Peer(name))),
                Err(_) => None,
            }
        }

        pub fn is_owner(&self) -> bool {
            matches!(self.0, Role::Owner { .. })
        }

        pub fn wait(&self) -> bool {
            let Role::Peer(name) = &self.0 else {
                return false;
            };
            // A saturated listen queue must never block connect. An absent
            // owner may already have committed, so check the store again.
            let fd = unsafe {
                libc::socket(
                    libc::AF_UNIX,
                    libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                    0,
                )
            };
            if fd < 0 {
                return false;
            }
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
            addr.sun_family = libc::AF_UNIX as _;
            for (dst, byte) in addr.sun_path[1..].iter_mut().zip(name) {
                *dst = *byte as _;
            }
            let len = std::mem::offset_of!(libc::sockaddr_un, sun_path) + 1 + name.len();
            if unsafe {
                libc::connect(
                    fd.as_raw_fd(),
                    (&addr as *const libc::sockaddr_un).cast(),
                    len as _,
                )
            } < 0
            {
                return matches!(
                    io::Error::last_os_error().raw_os_error(),
                    Some(libc::ECONNREFUSED | libc::ENOENT)
                );
            }
            let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
            let mut len = std::mem::size_of_val(&cred) as libc::socklen_t;
            if unsafe {
                libc::getsockopt(
                    fd.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    (&mut cred as *mut libc::ucred).cast(),
                    &mut len,
                )
            } < 0
                || cred.uid != unsafe { libc::geteuid() }
            {
                return false;
            }
            let mut event = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let end = Instant::now() + Duration::from_secs(60);
            loop {
                let left = end.saturating_duration_since(Instant::now()).as_millis() as i32;
                let n = unsafe { libc::poll(&mut event, 1, left) };
                if n >= 0 {
                    return n > 0
                        && event.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0;
                }
                if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    return false;
                }
            }
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        use std::process::{Command, Stdio};

        #[test]
        fn owner_exit_wakes_peers_and_releases_the_name() {
            const CHILD: &str = "JATO_TEST_FLIGHT_CHILD";
            if let Some(dir) = std::env::var_os(CHILD) {
                let dir = Path::new(&dir);
                let _owner = Ticket::enter(dir, &[7; 32], &dir.join("source")).unwrap();
                std::fs::write(dir.join("ready"), []).unwrap();
                // The parent kills this process so Rust destructors cannot
                // run. EOF also bounds its lifetime if the parent exits.
                let _ = std::io::stdin().read(&mut [0]);
                return;
            }
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("source");
            std::fs::write(&source, b"original").unwrap();
            let mut owner = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "flight::linux::tests::owner_exit_wakes_peers_and_releases_the_name",
                ])
                .env(CHILD, dir.path())
                .env_remove("JATO_COALESCE")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(8);
            while !dir.path().join("ready").exists() {
                assert!(owner.try_wait().unwrap().is_none() && Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
            let ticket = Ticket::enter(dir.path(), &[7; 32], &source).unwrap();
            assert!(matches!(ticket.0, Role::Peer(_)));
            std::fs::write(&source, b"modified").unwrap();
            assert!(matches!(
                Ticket::enter(dir.path(), &[7; 32], &source).unwrap().0,
                Role::Owner { .. }
            ));
            std::fs::write(&source, b"original").unwrap();
            std::thread::scope(|s| {
                let waiter = s.spawn(|| ticket.wait());
                owner.kill().unwrap();
                owner.wait().unwrap();
                assert!(waiter.join().unwrap());
            });
            assert!(matches!(
                Ticket::enter(dir.path(), &[7; 32], &source).unwrap().0,
                Role::Owner { .. }
            ));
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::Ticket;

#[cfg(not(target_os = "linux"))]
pub struct Ticket;
#[cfg(not(target_os = "linux"))]
impl Ticket {
    pub fn enter(_: &Path, _: &Digest, _: &Path) -> Option<Self> {
        None
    }
    pub fn is_owner(&self) -> bool {
        false
    }
    pub fn wait(&self) -> bool {
        false
    }
}
