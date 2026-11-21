//! Rootless mounts can display every unmapped owner as 65534. Ask the
//! invoking user namespace about the identical inode before trusting a GNU
//! executable or using the system-file fast path. No file contents or file
//! descriptors are exposed by the service.
use crate::fingerprint::stamp;
use anyhow::{Context, Result, ensure};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, Metadata};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::{
    ffi::OsStrExt,
    fs::MetadataExt,
    net::{SocketAddr, UnixListener, UnixStream},
    process::CommandExt,
};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

const ENDPOINT: &str = "JATO_WORKTREE_HOST";
const TIMEOUT: Duration = Duration::from_millis(250);
const PATH_LIMIT: usize = 4096;
const FORWARDED: [i32; 4] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT];
const UNBLOCK: &str = "JATO_WORKTREE_UNBLOCK";

pub(super) fn restore_payload_signals() -> Result<()> {
    let bits = std::env::var(UNBLOCK)?.parse::<u8>()?;
    unsafe {
        let mut mask = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        for (i, sig) in FORWARDED.iter().enumerate() {
            if bits & (1 << i) != 0 {
                libc::sigaddset(&mut mask, *sig);
            }
        }
        let e = libc::pthread_sigmask(libc::SIG_UNBLOCK, &mask, std::ptr::null_mut());
        if e != 0 {
            return Err(io::Error::from_raw_os_error(e).into());
        }
    }
    Ok(())
}

fn peer(stream: &UnixStream) -> io::Result<libc::ucred> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of_val(&cred) as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(cred)
    }
}

struct Client {
    address: SocketAddr,
    pid: i32,
    known: HashMap<[u64; 7], bool>,
}

impl Client {
    fn from_env() -> Option<Self> {
        if std::env::var_os("JATO_WORKTREE_ACTIVE").as_deref() != Some(OsStr::new("1")) {
            return None;
        }
        if fs::read_to_string("/proc/self/uid_map")
            .ok()?
            .lines()
            .any(|l| l.split_whitespace().next() == Some("0"))
        {
            return None;
        }
        let name = std::env::var(ENDPOINT).ok()?;
        let (pid, nonce) = name.strip_prefix("jato-view-")?.split_once('-')?;
        if nonce.len() != 32 || !nonce.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let pid = pid.parse().ok()?;
        let mut parent = unsafe { libc::getppid() };
        for _ in 0..64 {
            if parent == pid {
                return Some(Self {
                    address: SocketAddr::from_abstract_name(name.as_bytes()).ok()?,
                    pid,
                    known: HashMap::new(),
                });
            }
            if parent <= 1 {
                break;
            }
            let stat = fs::read_to_string(format!("/proc/{parent}/stat")).ok()?;
            parent = stat
                .rsplit_once(") ")?
                .1
                .split_whitespace()
                .nth(1)?
                .parse()
                .ok()?;
        }
        None
    }

    fn ask(&self, path: &Path, identity: &[u64; 7]) -> io::Result<bool> {
        let bytes = path.as_os_str().as_bytes();
        if bytes.len() > PATH_LIMIT {
            return Ok(false);
        }
        let mut socket = UnixStream::connect_addr(&self.address)?;
        let owner = peer(&socket)?;
        if owner.pid != self.pid || owner.uid != unsafe { libc::geteuid() } {
            return Ok(false);
        }
        socket.set_read_timeout(Some(TIMEOUT))?;
        socket.set_write_timeout(Some(TIMEOUT))?;
        let mut request = Vec::with_capacity(bytes.len() + 2);
        request.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
        request.extend_from_slice(bytes);
        socket.write_all(&request)?;
        let mut reply = [0u8; 57];
        socket.read_exact(&mut reply)?;
        Ok(reply[0] == 1
            && identity
                .iter()
                .zip(reply[1..].as_chunks::<8>().0)
                .all(|(a, b)| a.to_le_bytes() == *b))
    }
}

thread_local! {static CLIENT: RefCell<Option<Client>>=RefCell::new(Client::from_env());}

pub fn system_owner(path: &Path, m: &Metadata) -> bool {
    if m.uid() == 0 {
        return true;
    }
    CLIENT.with_borrow_mut(|client| {
        let Some(client) = client else {
            return false;
        };
        let id = stamp(m);
        if let Some(value) = client.known.get(&id) {
            return *value;
        }
        let Ok(value) = client.ask(path, &id) else {
            return false;
        };
        if client.known.len() < 2048 {
            client.known.insert(id, value);
        }
        value
    })
}

fn serve(mut socket: UnixStream) -> io::Result<()> {
    if peer(&socket)?.uid != unsafe { libc::geteuid() } {
        return Ok(());
    }
    socket.set_read_timeout(Some(TIMEOUT))?;
    socket.set_write_timeout(Some(TIMEOUT))?;
    let mut size = [0u8; 2];
    socket.read_exact(&mut size)?;
    let n = u16::from_le_bytes(size) as usize;
    if n == 0 || n > PATH_LIMIT {
        return Ok(());
    }
    let mut buffer = [0u8; PATH_LIMIT];
    socket.read_exact(&mut buffer[..n])?;
    let path = Path::new(OsStr::from_bytes(&buffer[..n]));
    let mut reply = [0u8; 57];
    if crate::native::system_file(path)
        && fs::canonicalize(path).is_ok_and(|p| p == path)
        && let Ok(m) = fs::symlink_metadata(path)
        && m.is_file()
        && m.uid() == 0
    {
        reply[0] = 1;
        for (dst, val) in reply[1..].as_chunks_mut::<8>().0.iter_mut().zip(stamp(&m)) {
            dst.copy_from_slice(&val.to_le_bytes());
        }
    }
    socket.write_all(&reply)
}

struct Signals {
    fd: OwnedFd,
    previous: libc::sigset_t,
}

impl Signals {
    fn new() -> io::Result<Self> {
        unsafe {
            let mut mask = std::mem::zeroed();
            let mut previous = std::mem::zeroed();
            libc::sigemptyset(&mut mask);
            for s in FORWARDED {
                libc::sigaddset(&mut mask, s);
            }
            let fd = libc::signalfd(-1, &mask, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let fd = OwnedFd::from_raw_fd(fd);
            let e = libc::pthread_sigmask(libc::SIG_BLOCK, &mask, &mut previous);
            if e != 0 {
                return Err(io::Error::from_raw_os_error(e));
            }
            Ok(Self { fd, previous })
        }
    }
    fn forward(&self, group: bool, inner: Option<&(i32, Option<OwnedFd>)>) -> Option<i32> {
        unsafe {
            let mut info: libc::signalfd_siginfo = std::mem::zeroed();
            while libc::read(
                self.fd.as_raw_fd(),
                (&mut info as *mut libc::signalfd_siginfo).cast(),
                std::mem::size_of_val(&info),
            ) == std::mem::size_of_val(&info) as isize
            {
                let sig = info.ssi_signo as i32;
                let Some((pid, fd)) = inner else {
                    return Some(sig);
                };
                if group || info.ssi_code != libc::SI_KERNEL {
                    // Leave the bubblewrap monitor alive to return the
                    // payload's actual status after it handles the signal.
                    if group && libc::kill(-*pid, sig) == 0 {
                        continue;
                    }
                    if let Some(fd) = fd {
                        libc::syscall(
                            libc::SYS_pidfd_send_signal,
                            fd.as_raw_fd(),
                            sig,
                            std::ptr::null::<libc::siginfo_t>(),
                            0,
                        );
                    } else {
                        libc::kill(*pid, sig);
                    }
                }
            }
        }
        None
    }
}
impl Drop for Signals {
    fn drop(&mut self) {
        unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.previous, std::ptr::null_mut());
        }
    }
}

fn pidfd(pid: i32) -> Option<OwnedFd> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
    (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) })
}

pub fn run(mut cmd: Command, payload: &[OsString]) -> Result<i32> {
    let mut nonce = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut nonce)?;
    let name = format!(
        "jato-view-{}-{:032x}",
        std::process::id(),
        u128::from_le_bytes(nonce)
    );
    let server = UnixListener::bind_addr(&SocketAddr::from_abstract_name(name.as_bytes())?)?;
    server.set_nonblocking(true)?;
    let signals = Signals::new()?;
    let (reader, writer) = io::pipe()?;
    cmd.env(ENDPOINT, name)
        .arg("--info-fd")
        .arg(writer.as_raw_fd().to_string())
        .arg("--")
        .args(payload);
    let group = ![0, 1, 2]
        .into_iter()
        .any(|fd| unsafe { libc::tcgetpgrp(fd) == libc::getpgrp() });
    if group {
        cmd.process_group(0);
    }
    cmd.env("JATO_WORKTREE_PROCESS_GROUP", if group { "1" } else { "0" });
    // A foreground TTY broadcasts Ctrl-C to both the program and the mount
    // supervisor. Keep the supervisor alive while the program handles it.
    let mut bits = 0u8;
    for (i, sig) in FORWARDED.iter().enumerate() {
        if unsafe { libc::sigismember(&signals.previous, *sig) } == 0 {
            bits |= 1 << i;
        }
    }
    cmd.env(UNBLOCK, bits.to_string());
    unsafe {
        cmd.pre_exec(move || {
            if libc::fcntl(writer.as_raw_fd(), libc::F_SETFD, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .context("starting bubblewrap for worktree paths (install bwrap on Linux)")?;
    drop(cmd);
    let pid = child.id() as i32;
    let completion = pidfd(pid);
    #[derive(serde::Deserialize)]
    struct Info {
        #[serde(rename = "child-pid")]
        child: i32,
    }
    let mut reader = Some(reader);
    let mut info_bytes = Vec::new();
    let mut inner = None;
    let result = (|| -> Result<i32> {
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(crate::compiler::exit_code(status));
            }
            let mut fds = [
                signals.fd.as_raw_fd(),
                server.as_raw_fd(),
                completion.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                reader.as_ref().map_or(-1, AsRawFd::as_raw_fd),
            ]
            .map(|fd| libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            });
            let ready = unsafe {
                libc::poll(
                    fds.as_mut_ptr(),
                    fds.len() as _,
                    if completion.is_some() { -1 } else { 40 },
                )
            };
            if ready < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            ensure!(
                ready >= 0,
                "waiting on worktree command: {}",
                io::Error::last_os_error()
            );
            if fds[3].revents != 0
                && let Some(pipe) = &mut reader
            {
                let mut chunk = [0; 1024];
                match pipe.read(&mut chunk) {
                    Ok(0) => reader = None,
                    Ok(n) => {
                        info_bytes.extend_from_slice(&chunk[..n]);
                        if let Ok(info) = serde_json::from_slice::<Info>(&info_bytes) {
                            inner = Some((info.child, pidfd(info.child)));
                            reader = None;
                        } else if info_bytes.len() >= 16384 {
                            reader = None;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => (),
                    Err(_) => reader = None,
                }
            }
            if fds[0].revents != 0
                && let Some(sig) = signals.forward(group, inner.as_ref())
            {
                // The command has not reported its PID during mount setup.
                let _ = child.kill();
                let _ = child.wait();
                return Ok(128 + sig);
            }
            if fds[1].revents != 0
                && let Ok((s, _)) = server.accept()
            {
                let _ = serve(s);
            }
        }
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ownership_reply_is_bound_to_a_canonical_system_inode() -> anyhow::Result<()> {
        fn query(path: &Path) -> io::Result<[u8; 57]> {
            let (mut client, server) = UnixStream::pair()?;
            let bytes = path.as_os_str().as_bytes();
            client.write_all(&(bytes.len() as u16).to_le_bytes())?;
            client.write_all(bytes)?;
            serve(server)?;
            let mut out = [0; 57];
            client.read_exact(&mut out)?;
            Ok(out)
        }
        let temp = tempfile::NamedTempFile::new()?;
        assert_eq!(query(temp.path())?, [0; 57]);
        let path = fs::canonicalize("/usr/bin/env")?;
        let m = fs::metadata(&path)?;
        if m.uid() == 0 && crate::native::system_file(&path) {
            let reply = query(&path)?;
            assert_eq!(reply[0], 1);
            for (word, num) in reply[1..].as_chunks::<8>().0.iter().zip(stamp(&m)) {
                assert_eq!(*word, num.to_le_bytes());
            }
            assert_eq!(query(&Path::new("/usr/lib/../bin").join("env"))?, [0; 57]);
        }
        Ok(())
    }
}
