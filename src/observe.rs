//! Record which files a command reads, lists, runs and writes.
//!
//! The command runs under a seccomp filter that reports system calls that
//! open, examine, list, run, create or remove a path to a thread in this
//! process. Reading where a symbolic link points is not reported: programs
//! do that for every part of every path they tidy, and the links that
//! matter are found again from the paths that were used. Every call is allowed to continue;
//! nothing is denied or emulated. A cache entry is only as sound as its list
//! of inputs, and this is where that list comes from. It includes what proc
//! macros, linkers, assemblers and build scripts open, which rustc's dep-info
//! never mentions.
use crate::scan::Digest;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// Identity of a file's contents as far as the kernel can tell without
/// reading it. Two equal stamps taken at different times mean the bytes did
/// not change in between.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, Default)]
pub struct Stamp {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime: i64,
    pub ctime: i64,
}

impl Stamp {
    pub fn of(meta: &fs::Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime: meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
            ctime: meta.ctime() * 1_000_000_000 + meta.ctime_nsec(),
        }
    }

    /// Whether two looks at a file a moment apart saw the same contents.
    /// The change time alone moves when the file is given another name,
    /// as rustc does with a metadata file while other compilers read it.
    pub fn same_contents(&self, later: &Self) -> bool {
        (self.dev, self.ino, self.size, self.mtime)
            == (later.dev, later.ino, later.size, later.mtime)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    File,
    Dir,
    Other,
    Absent,
}

pub fn kind_of(meta: &fs::Metadata) -> Kind {
    if meta.is_file() {
        Kind::File
    } else if meta.is_dir() {
        Kind::Dir
    } else {
        Kind::Other
    }
}

#[derive(Clone, Debug)]
pub struct Access {
    /// What the path led to the first time the command used it.
    pub kind: Kind,
    pub stamp: Option<Stamp>,
    /// How many names the file had then.
    pub links: u64,
    /// The target, if the path itself was a symbolic link.
    pub link: Option<PathBuf>,
    /// Opened for reading while it held what was there before the command.
    pub read: bool,
    /// Examined with stat, access or readlink before the command wrote it.
    pub probed: bool,
    /// Names in the directory when it was first enumerated, except those the
    /// command had created itself.
    pub listing: Option<Vec<OsString>>,
    pub executed: bool,
    /// Created, truncated, renamed into place or opened for writing.
    pub written: bool,
    /// The command made this file, or emptied it before writing. What it
    /// holds from then on did not come from outside.
    pub created: bool,
    /// Opened for writing while it still held earlier contents.
    pub updated: bool,
    pub removed: bool,
    /// Nothing was there at first, and later something was that the
    /// command read without having written it.
    pub appeared: bool,
    /// Where those earlier contents were when the command started, if the
    /// command moved them here.
    pub origin: Option<PathBuf>,
    /// The command line of the program that wrote over what was there.
    pub writer: Option<Box<str>>,
    /// Used by a program other than the one the command itself is: one it
    /// started, or a part of it that was recorded apart.
    pub others: bool,
    /// When the command first used the path and when it made the file its
    /// own, counted in the system calls reported until then. Zero for a
    /// file it never made.
    pub first: u64,
    pub made: u64,
}

impl Access {
    /// Take in what a part of the command did with the same path under a
    /// record of its own, as if the command had done it all itself.
    ///
    /// Where the order of two things cannot be told from the two records,
    /// the one that keeps more of what was there from outside is assumed.
    pub fn absorb(&mut self, part: Access) {
        // Whichever looked first saw what was there before the command.
        let (mut whole, later) = if part.first < self.first {
            (part, self.clone())
        } else {
            (self.clone(), part)
        };
        // What the later one found was the command's own work only if the
        // file had been made by then.
        let own = whole.created && whole.made <= later.first;
        if !own {
            whole.read |= later.read;
            whole.probed |= later.probed;
            whole.appeared |= later.appeared;
            whole.updated |= later.updated;
            if whole.listing.is_none() {
                whole.listing = later.listing;
            }
            if later.created && !whole.created {
                whole.created = true;
                whole.made = later.made;
            }
        }
        whole.executed |= later.executed;
        whole.written |= later.written;
        // Whether it is there now says which of the two acted last.
        whole.removed |= later.removed;
        if whole.origin.is_none() {
            whole.origin = later.origin;
        }
        if whole.writer.is_none() {
            whole.writer = later.writer;
        }
        whole.others |= later.others;
        *self = whole;
    }
}

impl Report {
    /// Take in the record of a part of the command that was kept apart.
    pub fn absorb(&mut self, part: Report) {
        for (path, access) in part.paths {
            match self.paths.get_mut(&path) {
                Some(known) => {
                    known.absorb(access);
                    if known.removed && fs::symlink_metadata(&path).is_ok() {
                        known.removed = false;
                    }
                }
                None => drop(self.paths.insert(path, access)),
            }
        }
        self.network |= part.network;
        self.sockets.extend(part.sockets);
        self.sockets.truncate(64);
        if self.incomplete.is_none() {
            self.incomplete = part.incomplete;
        }
        self.survivors |= part.survivors;
        self.noted += part.noted;
        for (call, count) in part.reported {
            *self.reported.entry(call).or_default() += count;
        }
    }
}

#[derive(Default, Debug)]
pub struct Report {
    pub paths: HashMap<PathBuf, Access>,
    /// The command opened a network connection.
    pub network: bool,
    /// Unix sockets the command connected to, and the command line of the
    /// program that did.
    pub sockets: Vec<(PathBuf, String)>,
    /// Why the record cannot be trusted to be complete, if it cannot.
    pub incomplete: Option<String>,
    /// A program the command started was still running when it exited.
    pub survivors: bool,
    pub events: u64,
    /// How many of the command's file calls it told of itself, without
    /// waiting to be asked. See `fast/shim.rs`.
    pub noted: u64,
    /// How many calls of each kind the kernel reported, by call number.
    pub reported: std::collections::BTreeMap<u32, u64>,
    /// Names used directly inside the directory `Config::state` names.
    pub state: std::collections::BTreeSet<OsString>,
}

#[derive(Default, Clone)]
pub struct Config {
    /// Nothing at or beneath these paths is recorded.
    pub ignore: Vec<PathBuf>,
    /// Directories whose enumeration is not an input.
    pub unlisted: Vec<PathBuf>,
    /// One of the ignored directories, for which the names used directly
    /// inside it are noted all the same.
    pub state: Option<PathBuf>,
    /// The library to load into the command, through which it tells of
    /// the files it reads without waiting to be asked. See `fast/shim.rs`.
    pub fast: Option<PathBuf>,
}

/// The library that watched programs load, as it was built with this
/// program. Empty where there is none.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const SHIM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/libjato_fast.so"));
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
const SHIM: &[u8] = &[];

/// Whether a path names a copy of that library, wherever it is kept.
pub fn is_fast_library(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.starts_with("jato-fast-") && name.ends_with(".so"))
}

/// A file holding that library inside the cache directory, written if it
/// is not there yet. None where there is no such library, or when
/// `JATO_NOTES=0` asks for every call to be reported by the kernel.
pub fn fast_library(cache: &Path) -> Option<PathBuf> {
    if SHIM.is_empty() || std::env::var_os("JATO_NOTES").is_some_and(|v| v == "0") {
        return None;
    }
    let name = format!(
        "jato-fast-{}.so",
        &blake3::hash(SHIM).to_hex().as_str()[..16]
    );
    let path = cache.join("lib").join(name);
    if fs::metadata(&path).is_ok_and(|meta| meta.len() == SHIM.len() as u64) {
        return Some(path);
    }
    // Written under another name and moved into place whole: a program
    // being started must never load half of it.
    fs::create_dir_all(path.parent()?).ok()?;
    let making = path.with_extension(format!("{}", std::process::id()));
    fs::write(&making, SHIM).ok()?;
    if fs::rename(&making, &path).is_err() {
        let _ = fs::remove_file(&making);
    }
    path.is_file().then_some(path)
}

/// Digest of a set of directory entry names.
pub fn listing_digest(mut names: Vec<OsString>) -> Digest {
    // What tools put down and take away as they run is no part of what a
    // directory holds: Cargo's locks and the journals of its database,
    // which come and go in its home directory, and the files Jato itself
    // stages beside the ones it puts back.
    names.retain(|name| {
        let name = name.as_bytes();
        let lock = name.starts_with(b".cargo-") && name.ends_with(b"lock");
        let own = memchr::memmem::find(name, b".jato-").is_some();
        !(lock || own || name.starts_with(b".package-cache") || name.starts_with(b".global-cache"))
    });
    names.sort_unstable();
    let mut hash = blake3::Hasher::new_derive_key("jato directory listing v1");
    for name in &names {
        hash.update(&(name.len() as u64).to_le_bytes());
        hash.update(name.as_bytes());
    }
    *hash.finalize().as_bytes()
}

fn ephemeral(path: &Path) -> bool {
    let bytes = path.as_os_str().as_bytes();
    [&b"/proc"[..], b"/sys", b"/dev"]
        .iter()
        .any(|p| bytes.starts_with(p) && matches!(bytes.get(p.len()), None | Some(b'/')))
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod imp {
    use super::*;

    #[cfg(target_arch = "x86_64")]
    mod nr {
        pub const OPEN: u32 = 2;
        pub const STAT: u32 = 4;
        pub const LSTAT: u32 = 6;
        pub const ACCESS: u32 = 21;
        pub const CONNECT: u32 = 42;
        pub const EXECVE: u32 = 59;
        pub const TRUNCATE: u32 = 76;
        pub const GETDENTS: u32 = 78;
        pub const RENAME: u32 = 82;
        pub const MKDIR: u32 = 83;
        pub const RMDIR: u32 = 84;
        pub const CREAT: u32 = 85;
        pub const LINK: u32 = 86;
        pub const UNLINK: u32 = 87;
        pub const SYMLINK: u32 = 88;
        pub const PIVOT_ROOT: u32 = 155;
        pub const CHROOT: u32 = 161;
        pub const MOUNT: u32 = 165;
        pub const GETDENTS64: u32 = 217;
        pub const OPENAT: u32 = 257;
        pub const MKDIRAT: u32 = 258;
        pub const NEWFSTATAT: u32 = 262;
        pub const UNLINKAT: u32 = 263;
        pub const RENAMEAT: u32 = 264;
        pub const LINKAT: u32 = 265;
        pub const SYMLINKAT: u32 = 266;
        pub const FACCESSAT: u32 = 269;
        pub const UNSHARE: u32 = 272;
        pub const NAME_TO_HANDLE_AT: u32 = 303;
        pub const OPEN_BY_HANDLE_AT: u32 = 304;
        pub const SETNS: u32 = 308;
        pub const RENAMEAT2: u32 = 316;
        pub const SECCOMP: u32 = 317;
        pub const EXECVEAT: u32 = 322;
        pub const STATX: u32 = 332;
        pub const IO_URING_SETUP: u32 = 425;
        pub const OPENAT2: u32 = 437;
        pub const FACCESSAT2: u32 = 439;
        pub const ARCH: u32 = 0xc000_003e;
        /// Calls that only this architecture's table has.
        pub const LEGACY: &[u32] = &[
            OPEN, STAT, LSTAT, ACCESS, GETDENTS, RENAME, MKDIR, RMDIR, CREAT, LINK, UNLINK, SYMLINK,
        ];
    }
    #[cfg(target_arch = "aarch64")]
    mod nr {
        // The generic table has none of the calls that take a bare path.
        pub const OPEN: u32 = u32::MAX;
        pub const STAT: u32 = u32::MAX - 1;
        pub const LSTAT: u32 = u32::MAX - 2;
        pub const ACCESS: u32 = u32::MAX - 3;
        pub const GETDENTS: u32 = u32::MAX - 4;
        pub const RENAME: u32 = u32::MAX - 5;
        pub const MKDIR: u32 = u32::MAX - 6;
        pub const RMDIR: u32 = u32::MAX - 7;
        pub const CREAT: u32 = u32::MAX - 8;
        pub const LINK: u32 = u32::MAX - 9;
        pub const UNLINK: u32 = u32::MAX - 10;
        pub const SYMLINK: u32 = u32::MAX - 11;
        pub const MKDIRAT: u32 = 34;
        pub const UNLINKAT: u32 = 35;
        pub const SYMLINKAT: u32 = 36;
        pub const LINKAT: u32 = 37;
        pub const RENAMEAT: u32 = 38;
        pub const MOUNT: u32 = 40;
        pub const PIVOT_ROOT: u32 = 41;
        pub const TRUNCATE: u32 = 45;
        pub const FACCESSAT: u32 = 48;
        pub const CHROOT: u32 = 51;
        pub const OPENAT: u32 = 56;
        pub const GETDENTS64: u32 = 61;
        pub const NEWFSTATAT: u32 = 79;
        pub const UNSHARE: u32 = 97;
        pub const CONNECT: u32 = 203;
        pub const EXECVE: u32 = 221;
        pub const NAME_TO_HANDLE_AT: u32 = 264;
        pub const OPEN_BY_HANDLE_AT: u32 = 265;
        pub const SETNS: u32 = 268;
        pub const RENAMEAT2: u32 = 276;
        pub const SECCOMP: u32 = 277;
        pub const EXECVEAT: u32 = 281;
        pub const STATX: u32 = 291;
        pub const IO_URING_SETUP: u32 = 425;
        pub const OPENAT2: u32 = 437;
        pub const FACCESSAT2: u32 = 439;
        pub const ARCH: u32 = 0xc000_00b7;
        pub const LEGACY: &[u32] = &[];
    }

    const SECCOMP_SET_MODE_FILTER: libc::c_ulong = 1;
    const FLAG_NEW_LISTENER: libc::c_ulong = 1 << 3;
    const FLAG_WAIT_KILLABLE_RECV: libc::c_ulong = 1 << 5;
    /// What this program and the library in watched programs agree on.
    mod abi {
        #![allow(dead_code)]
        include!("fast/abi.rs");
    }
    /// The descriptor a watched program is given the socket for its notes
    /// on.
    const NOTES_FD: RawFd = 217;

    const RET_USER_NOTIF: u32 = 0x7fc0_0000;
    const RET_ALLOW: u32 = 0x7fff_0000;
    const NOTIF_FLAG_CONTINUE: u32 = 1;
    // _IOWR('!', 0, struct seccomp_notif), _IOWR('!', 1, struct seccomp_notif_resp)
    // and _IOW('!', 4, __u64) from linux/seccomp.h.
    const IOCTL_RECV: libc::c_ulong = 0xc050_2100;
    const IOCTL_SEND: libc::c_ulong = 0xc018_2101;
    const IOCTL_SET_FLAGS: libc::c_ulong = 0x4008_2104;
    const FD_SYNC_WAKE_UP: u64 = 1;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Insn {
        code: u16,
        jt: u8,
        jf: u8,
        k: u32,
    }
    #[repr(C)]
    struct Program {
        len: u16,
        filter: *const Insn,
    }
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct Data {
        nr: i32,
        arch: u32,
        ip: u64,
        args: [u64; 6],
    }
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct Notif {
        id: u64,
        pid: u32,
        flags: u32,
        data: Data,
    }
    #[repr(C)]
    #[derive(Default)]
    struct Resp {
        id: u64,
        val: i64,
        error: i32,
        flags: u32,
    }

    const LD_ABS: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
    const JEQ: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
    const JGE: u16 = 0x35; // BPF_JMP | BPF_JGE | BPF_K
    const JSET: u16 = 0x45; // BPF_JMP | BPF_JSET | BPF_K
    const RET: u16 = 0x06; // BPF_RET | BPF_K
    const AT_EMPTY_PATH: u32 = 0x1000;

    /// Calls reported whatever their arguments.
    const ALWAYS: &[u32] = &[
        nr::OPENAT,
        nr::OPENAT2,
        nr::EXECVE,
        nr::EXECVEAT,
        nr::FACCESSAT,
        nr::FACCESSAT2,
        nr::GETDENTS64,
        nr::RENAMEAT,
        nr::RENAMEAT2,
        nr::LINKAT,
        nr::SYMLINKAT,
        nr::MKDIRAT,
        nr::UNLINKAT,
        nr::TRUNCATE,
        nr::CONNECT,
        nr::IO_URING_SETUP,
        nr::NAME_TO_HANDLE_AT,
        nr::OPEN_BY_HANDLE_AT,
        nr::CHROOT,
        nr::MOUNT,
        nr::PIVOT_ROOT,
        nr::UNSHARE,
        nr::SETNS,
        nr::SECCOMP,
    ];

    /// The filter. `listing` has the calls that read a directory's entries
    /// reported as well, for a command that is started with a directory
    /// already open: any other has to open one first, and that is seen.
    fn filter(listing: bool) -> Vec<Insn> {
        let simple: Vec<u32> = ALWAYS
            .iter()
            .chain(nr::LEGACY)
            .copied()
            .filter(|call| listing || !matches!(*call, nr::GETDENTS64 | nr::GETDENTS))
            .collect();
        let insn = |code, jt, jf, k| Insn { code, jt, jf, k };
        let mut prog = vec![
            insn(LD_ABS, 0, 0, 4), // architecture
            insn(JEQ, 0, 0, nr::ARCH),
        ];
        // A call made from the one address the library in a watched
        // program calls from, of a kind that only reads, has been noted by
        // that library and goes ahead. An open is looked at again here:
        // it must not create, empty or write.
        if cfg!(target_arch = "x86_64") {
            let quiet = [
                nr::STAT,
                nr::LSTAT,
                nr::ACCESS,
                nr::NEWFSTATAT,
                nr::STATX,
                nr::FACCESSAT,
                nr::FACCESSAT2,
            ];
            let writes = (libc::O_ACCMODE | libc::O_CREAT | libc::O_TRUNC | libc::O_TMPFILE) as u32
                & !(libc::O_DIRECTORY as u32);
            // The block's instructions: five to compare the address, one
            // for each call, three for the open, and the one that lets a
            // call through, which is the last.
            let block = quiet.len() + 9;
            let start = prog.len();
            let past = |from: usize| (start + block - from - 1) as u8;
            prog.push(insn(LD_ABS, 0, 0, 8)); // where the call was made, low half
            prog.push(insn(JEQ, 0, past(start + 1), abi::STUB_RETURN as u32));
            prog.push(insn(LD_ABS, 0, 0, 12)); // and high half
            prog.push(insn(
                JEQ,
                0,
                past(start + 3),
                (abi::STUB_RETURN >> 32) as u32,
            ));
            prog.push(insn(LD_ABS, 0, 0, 0)); // call number
            for call in quiet {
                let here = prog.len();
                prog.push(insn(JEQ, past(here) - 1, 0, call));
            }
            let here = prog.len();
            prog.push(insn(JEQ, 0, past(here), nr::OPENAT));
            prog.push(insn(LD_ABS, 0, 0, 16 + 8 * 2)); // its flags
            prog.push(insn(JSET, 1, 0, writes));
            prog.push(insn(RET, 0, 0, RET_ALLOW));
            debug_assert_eq!(prog.len(), start + block);
        }
        let number = prog.len();
        prog.push(insn(LD_ABS, 0, 0, 0)); // call number
        // The x32 ABI shares the architecture value and sets a high bit.
        let x32 = cfg!(target_arch = "x86_64");
        if x32 {
            prog.push(insn(JGE, 0, 0, 0x4000_0000));
        }
        let tests = prog.len();
        // After the tests: 0 allow, 1-2 fstatat flags, 3-4 statx flags,
        // 5 notify, 6 allow.
        let tail = tests + simple.len() + 2;
        let notify = tail + 5;
        for (i, call) in simple.iter().enumerate() {
            let here = tests + i;
            prog.push(insn(JEQ, (notify - here - 1) as u8, 0, *call));
        }
        let here = prog.len();
        prog.push(insn(JEQ, (tail + 1 - here - 1) as u8, 0, nr::NEWFSTATAT));
        let here = prog.len();
        prog.push(insn(JEQ, (tail + 3 - here - 1) as u8, 0, nr::STATX));
        debug_assert_eq!(prog.len(), tail);
        prog.push(insn(RET, 0, 0, RET_ALLOW));
        // fstat is spelled as a stat of an empty path relative to a
        // descriptor. It names no path and is by far the most frequent call.
        prog.push(insn(LD_ABS, 0, 0, 16 + 8 * 3)); // newfstatat flags
        prog.push(insn(JSET, 3, 2, AT_EMPTY_PATH));
        prog.push(insn(LD_ABS, 0, 0, 16 + 8 * 2)); // statx flags
        prog.push(insn(JSET, 1, 0, AT_EMPTY_PATH));
        prog.push(insn(RET, 0, 0, RET_USER_NOTIF));
        prog.push(insn(RET, 0, 0, RET_ALLOW));
        // Another call table is reported, which marks the record incomplete.
        prog[1].jf = (notify - 1 - 1) as u8;
        if x32 {
            prog[number + 1].jt = (notify - (number + 1) - 1) as u8;
        }
        assert!(prog.len() < 250, "jump offsets must fit a byte");
        prog
    }

    /// Room for one descriptor in a control message, aligned for its header.
    #[repr(C, align(8))]
    struct Control([u8; 64]);

    /// Install the filter in the child between fork and exec, and hand the
    /// listener to the parent. Only async-signal-safe calls are made.
    unsafe fn install(sock: RawFd, prog: &Program) -> io::Result<()> {
        unsafe {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut fd = libc::syscall(
                libc::SYS_seccomp,
                SECCOMP_SET_MODE_FILTER,
                FLAG_NEW_LISTENER | FLAG_WAIT_KILLABLE_RECV,
                prog as *const Program,
            );
            if fd < 0 && *libc::__errno_location() == libc::EINVAL {
                fd = libc::syscall(
                    libc::SYS_seccomp,
                    SECCOMP_SET_MODE_FILTER,
                    FLAG_NEW_LISTENER,
                    prog as *const Program,
                );
            }
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let fd = fd as RawFd;
            let mut space = Control([0; 64]);
            let mut byte = 0u8;
            let mut iov = libc::iovec {
                iov_base: (&raw mut byte).cast(),
                iov_len: 1,
            };
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &raw mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = space.0.as_mut_ptr().cast();
            msg.msg_controllen = libc::CMSG_SPACE(size_of::<RawFd>() as u32) as _;
            let header = libc::CMSG_FIRSTHDR(&raw const msg);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(size_of::<RawFd>() as u32) as _;
            std::ptr::copy_nonoverlapping(
                (&raw const fd).cast::<u8>(),
                libc::CMSG_DATA(header),
                size_of::<RawFd>(),
            );
            let sent = libc::sendmsg(sock, &raw const msg, libc::MSG_NOSIGNAL);
            let error = io::Error::last_os_error();
            libc::close(fd);
            libc::close(sock);
            if sent != 1 {
                return Err(error);
            }
            Ok(())
        }
    }

    fn receive_listener(sock: &OwnedFd) -> io::Result<OwnedFd> {
        let mut space = Control([0; 64]);
        let mut byte = 0u8;
        let mut iov = libc::iovec {
            iov_base: (&raw mut byte).cast(),
            iov_len: 1,
        };
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &raw mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = space.0.as_mut_ptr().cast();
        msg.msg_controllen = space.0.len() as _;
        loop {
            let n =
                unsafe { libc::recvmsg(sock.as_raw_fd(), &raw mut msg, libc::MSG_CMSG_CLOEXEC) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if n == 0 {
                return Err(io::Error::other("the command started without a filter"));
            }
            break;
        }
        unsafe {
            let header = libc::CMSG_FIRSTHDR(&raw const msg);
            if header.is_null()
                || (*header).cmsg_level != libc::SOL_SOCKET
                || (*header).cmsg_type != libc::SCM_RIGHTS
            {
                return Err(io::Error::other("no listener was passed"));
            }
            let mut fd: RawFd = -1;
            std::ptr::copy_nonoverlapping(
                libc::CMSG_DATA(header),
                (&raw mut fd).cast::<u8>(),
                size_of::<RawFd>(),
            );
            Ok(OwnedFd::from_raw_fd(fd))
        }
    }

    const MAX_PATHS: usize = 400_000;

    /// `name` as a path from the directory `base`, which is empty for a
    /// name that starts at the root. `.` and empty components are dropped.
    fn joined(base: &[u8], name: &[u8]) -> PathBuf {
        let mut cleaned = Vec::with_capacity(base.len() + name.len() + 1);
        cleaned.extend_from_slice(base);
        for part in name.split(|b| *b == b'/') {
            if part.is_empty() || part == b"." {
                continue;
            }
            if cleaned.last() != Some(&b'/') {
                cleaned.push(b'/');
            }
            cleaned.extend_from_slice(part);
        }
        if cleaned.is_empty() {
            cleaned.push(b'/');
        }
        PathBuf::from(OsString::from_vec(cleaned))
    }

    enum Use {
        Read,
        Probe,
        List,
        Execute,
        /// Opened for writing; `replace` when what was there is discarded.
        Write {
            replace: bool,
            read: bool,
        },
        Remove,
    }

    struct Tracker {
        report: Report,
        config: Config,
        buffer: Vec<u8>,
        /// The thread whose system call is being looked at, and how many
        /// calls the whole command has reported so far.
        caller: u32,
        now: u64,
        /// Whether that thread runs another program than the command's
        /// own.
        others: bool,
    }

    /// The command line of the program a thread belongs to, for telling
    /// someone which program it was.
    fn command_line(tid: u32) -> String {
        let Ok(line) = fs::read(format!("/proc/{tid}/cmdline")) else {
            return String::new();
        };
        let line = &line[..line.len().min(160)];
        String::from_utf8_lossy(line)
            .trim_end_matches('\0')
            .replace('\0', " ")
    }

    /// Whether a thread has ended or is ending. A program that exits takes
    /// its other threads with it, and one of them may have been waiting
    /// here for its call to be looked at. That call is never made, so
    /// nothing is missing from the record when it cannot be made out.
    fn gone(tid: u32) -> bool {
        let ended = || match fs::read(format!("/proc/{tid}/stat")) {
            Err(_) => true,
            // After the name, which may hold anything, comes the state.
            Ok(stat) => {
                let state = stat.rsplitn(2, |b| *b == b')').next();
                state.is_some_and(|rest| matches!(rest.get(1), Some(b'Z' | b'X')))
            }
        };
        // A thread that is ending takes a moment to be seen as ended.
        (0..20).any(|attempt| {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            ended()
        })
    }

    impl Tracker {
        fn incomplete(&mut self, why: &str) {
            self.report.incomplete.get_or_insert_with(|| why.to_owned());
        }

        fn remote(&mut self, pid: u32, address: u64, len: usize) -> Option<usize> {
            self.buffer.resize(len, 0);
            let local = libc::iovec {
                iov_base: self.buffer.as_mut_ptr().cast(),
                iov_len: len,
            };
            let remote = libc::iovec {
                iov_base: address as *mut libc::c_void,
                iov_len: len,
            };
            let n = unsafe { libc::process_vm_readv(pid as i32, &local, 1, &remote, 1, 0) };
            if n > 0 {
                return Some(n as usize);
            }
            if n < 0 {
                let error = io::Error::last_os_error();
                match error.raw_os_error() {
                    // A bad address fails in the caller as well and
                    // touches no file.
                    Some(libc::EFAULT) => (),
                    // The caller has ended: see `gone`.
                    Some(libc::ESRCH) => (),
                    _ if gone(pid) => (),
                    // Anything else means this process cannot see what
                    // the call names.
                    _ => self.incomplete(&format!(
                        "the memory of `{}` could not be read: {error}",
                        command_line(pid)
                    )),
                }
            }
            None
        }

        /// A call of a thread that is still there could not be made out.
        fn lost(&mut self, tid: u32, what: &str) {
            if !gone(tid) {
                self.incomplete(&format!("{what}, in `{}`", command_line(tid)));
            }
        }

        /// A NUL-terminated string in the caller's memory. Each read stays
        /// inside one page, so a string that ends just before unmapped
        /// memory is still read in full.
        fn string(&mut self, pid: u32, address: u64) -> Option<Vec<u8>> {
            if address == 0 {
                return None;
            }
            let mut out = Vec::new();
            let mut at = address;
            while out.len() <= libc::PATH_MAX as usize {
                let room = 4096 - (at & 4095) as usize;
                let n = self.remote(pid, at, room)?;
                let chunk = &self.buffer[..n];
                if let Some(end) = memchr::memchr(0, chunk) {
                    out.extend_from_slice(&chunk[..end]);
                    return Some(out);
                }
                out.extend_from_slice(chunk);
                at += n as u64;
            }
            None
        }

        /// The absolute path a call names. `.` and empty components are
        /// dropped; `..` stays, since only the kernel knows where it leads
        /// once symbolic links are involved.
        fn resolve(&mut self, tid: u32, dirfd: i32, name: &[u8]) -> Option<PathBuf> {
            // A path through the caller's own working directory is a path
            // relative to that directory, whatever else is under /proc.
            const OWN: &[u8] = b"/proc/self/cwd";
            const OPEN: &[u8] = b"/proc/self/fd/";
            let whole = |rest: &&[u8]| rest.is_empty() || rest[0] == b'/';
            let mut own = name
                .strip_prefix(OWN)
                .filter(whole)
                .map(|rest| (rest, libc::AT_FDCWD));
            // And one through a directory the caller holds open is a path
            // relative to that directory.
            if own.is_none()
                && let Some(rest) = name.strip_prefix(OPEN)
            {
                let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
                let number = std::str::from_utf8(&rest[..digits])
                    .ok()
                    .and_then(|n| n.parse::<i32>().ok());
                own = number
                    .map(|fd| (&rest[digits..], fd))
                    .filter(|(rest, _)| whole(rest));
            }
            let own = own.map(|(rest, fd)| (rest.strip_prefix(b"/").unwrap_or(rest), fd));
            let (name, dirfd) = match own {
                Some((rest, fd)) => (rest, fd),
                None => (name, dirfd),
            };
            let base = if own.is_none() && name.first() == Some(&b'/') {
                PathBuf::new()
            } else {
                let link = if dirfd == libc::AT_FDCWD {
                    format!("/proc/{tid}/cwd")
                } else {
                    format!("/proc/{tid}/fd/{dirfd}")
                };
                match fs::read_link(link) {
                    Ok(dir) if dir.is_absolute() => dir,
                    _ => return None,
                }
            };
            Some(joined(base.as_os_str().as_bytes(), name))
        }

        fn ignored(&self, path: &Path) -> bool {
            ephemeral(path) || self.config.ignore.iter().any(|p| path.starts_with(p))
        }

        fn touch(&mut self, path: PathBuf, how: Use) {
            if self.ignored(&path) {
                if let Some(dir) = &self.config.state
                    && let Ok(rest) = path.strip_prefix(dir)
                    && let Some(first) = rest.components().next()
                    && self.report.state.len() < 64
                {
                    self.report.state.insert(first.as_os_str().to_owned());
                }
                return;
            }
            // A directory is opened for reading in order to list it, and
            // the listing itself is not reported: it names no path. What
            // the directory held is taken down when it is opened.
            let how = match how {
                Use::Read
                    if match self.report.paths.get(&path) {
                        Some(known) => known.kind == Kind::Dir,
                        None => fs::metadata(&path).is_ok_and(|meta| meta.is_dir()),
                    } =>
                {
                    Use::List
                }
                how => how,
            };
            let listing = matches!(how, Use::List)
                && self
                    .report
                    .paths
                    .get(&path)
                    .is_none_or(|a| a.listing.is_none() && !a.created && a.kind == Kind::Dir)
                && !self.config.unlisted.contains(&path);
            let listing = listing.then(|| self.snapshot(&path)).flatten();
            if !self.report.paths.contains_key(&path) {
                if self.report.paths.len() >= MAX_PATHS {
                    self.incomplete("more paths than can be recorded");
                    return;
                }
                // lstat tells a link from what it leads to. Only links cost a
                // second call.
                let (kind, stamp, links, link) = match fs::symlink_metadata(&path) {
                    Ok(m) if m.file_type().is_symlink() => {
                        let link = fs::read_link(&path).ok();
                        match fs::metadata(&path) {
                            Ok(m) => (kind_of(&m), Some(Stamp::of(&m)), m.nlink(), link),
                            Err(_) => (Kind::Absent, None, 0, link),
                        }
                    }
                    Ok(m) => (kind_of(&m), Some(Stamp::of(&m)), m.nlink(), None),
                    Err(_) => (Kind::Absent, None, 0, None),
                };
                self.report.paths.insert(
                    path.clone(),
                    Access {
                        kind,
                        stamp,
                        links,
                        link,
                        read: false,
                        probed: false,
                        listing: None,
                        executed: false,
                        written: false,
                        created: false,
                        updated: false,
                        removed: false,
                        appeared: false,
                        origin: None,
                        writer: None,
                        others: false,
                        first: self.now,
                        made: 0,
                    },
                );
            }
            let access = self.report.paths.get_mut(&path).unwrap();
            access.others |= self.others;
            // A program that was run was run, whoever made it and whenever.
            access.executed |= matches!(how, Use::Execute);
            match how {
                // What the command removed is no longer there to be read.
                Use::Read | Use::Execute if access.removed => access.probed = true,
                Use::Read | Use::Execute if access.kind == Kind::Absent && !access.written => {
                    access.probed = true;
                    access.appeared |= fs::symlink_metadata(&path).is_ok();
                }
                Use::Read if !access.created => access.read = true,
                Use::Probe if !access.created => access.probed = true,
                Use::List if !access.created => {
                    access.probed = true;
                    if listing.is_some() {
                        access.listing = listing;
                    }
                }
                Use::Execute if !access.created => {
                    access.read = true;
                    access.executed = true;
                }
                Use::Read | Use::Probe | Use::List | Use::Execute => (),
                Use::Write { replace, read } => {
                    access.written = true;
                    if !access.created {
                        if replace || fs::symlink_metadata(&path).is_err() {
                            access.created = true;
                            access.made = self.now;
                        } else {
                            access.updated = true;
                            access.read |= read;
                        }
                    }
                    access.removed = false;
                    // A result may be turned down for this, and which
                    // program it was is the first thing anyone asks.
                    if (access.read || access.updated) && access.writer.is_none() {
                        access.writer = Some(command_line(self.caller).into());
                    }
                }
                Use::Remove => access.removed = true,
            }
        }

        fn snapshot(&mut self, dir: &Path) -> Option<Vec<OsString>> {
            let mut names = Vec::new();
            for entry in fs::read_dir(dir).ok()? {
                let name = entry.ok()?.file_name();
                if self
                    .report
                    .paths
                    .get(&dir.join(&name))
                    .is_some_and(|a| a.written)
                {
                    continue;
                }
                names.push(name);
            }
            Some(names)
        }

        fn at(&mut self, tid: u32, dirfd: i32, name: u64, how: Use) {
            let Some(name) = self.string(tid, name) else {
                return;
            };
            match self.resolve(tid, dirfd, &name) {
                Some(path) => self.touch(path, how),
                None => self.lost(tid, "a path could not be resolved"),
            }
        }

        fn open(&mut self, tid: u32, dirfd: i32, name: u64, flags: u64) {
            let flags = flags as i32;
            let mode = flags & libc::O_ACCMODE;
            let writes = mode != libc::O_RDONLY
                || flags & (libc::O_CREAT | libc::O_TRUNC) != 0
                || flags & libc::O_TMPFILE == libc::O_TMPFILE;
            if !writes {
                // Opened only to name the place, it cannot be read or
                // listed.
                let how = if flags & libc::O_PATH != 0 {
                    Use::Probe
                } else {
                    Use::Read
                };
                return self.at(tid, dirfd, name, how);
            }
            // An unnamed temporary file never appears in the directory.
            if flags & libc::O_TMPFILE == libc::O_TMPFILE {
                return;
            }
            let replace = flags & libc::O_TRUNC != 0
                || (flags & libc::O_CREAT != 0 && flags & libc::O_EXCL != 0);
            self.at(
                tid,
                dirfd,
                name,
                Use::Write {
                    replace,
                    read: mode == libc::O_RDWR && !replace,
                },
            );
        }

        fn handle(&mut self, data: &Data, tid: u32) {
            self.caller = tid;
            let a = &data.args;
            let call = data.nr as u32;
            *self.report.reported.entry(call).or_default() += 1;
            if data.arch != nr::ARCH || (cfg!(target_arch = "x86_64") && call >= 0x4000_0000) {
                return self.incomplete("a program used another system call table");
            }
            const CWD: i32 = libc::AT_FDCWD;
            let replace = Use::Write {
                replace: true,
                read: false,
            };
            match call {
                nr::OPENAT => self.open(tid, a[0] as i32, a[1], a[2]),
                nr::OPEN => self.open(tid, CWD, a[0], a[1]),
                nr::CREAT => self.at(tid, CWD, a[0], replace),
                nr::OPENAT2 => {
                    // struct open_how starts with the 64-bit flags.
                    let flags = match self.remote(tid, a[2], 8) {
                        Some(8) => u64::from_ne_bytes(self.buffer[..8].try_into().unwrap()),
                        _ => 0,
                    };
                    self.open(tid, a[0] as i32, a[1], flags)
                }
                nr::STAT | nr::LSTAT | nr::ACCESS => self.at(tid, CWD, a[0], Use::Probe),
                nr::NEWFSTATAT | nr::STATX | nr::FACCESSAT | nr::FACCESSAT2 => {
                    self.at(tid, a[0] as i32, a[1], Use::Probe)
                }
                nr::EXECVE => self.at(tid, CWD, a[0], Use::Execute),
                nr::EXECVEAT => {
                    let name = self.string(tid, a[1]).unwrap_or_default();
                    if name.is_empty() {
                        match fs::read_link(format!("/proc/{tid}/fd/{}", a[0] as i32)) {
                            Ok(path) if path.is_absolute() => self.touch(path, Use::Execute),
                            _ => self.lost(tid, "a program was run from a descriptor"),
                        }
                    } else if let Some(path) = self.resolve(tid, a[0] as i32, &name) {
                        self.touch(path, Use::Execute)
                    }
                }
                nr::GETDENTS64 | nr::GETDENTS => {
                    match fs::read_link(format!("/proc/{tid}/fd/{}", a[0] as i32)) {
                        Ok(path) if path.is_absolute() => self.touch(path, Use::List),
                        _ => self.lost(tid, "a listed directory could not be named"),
                    }
                }
                nr::RENAME => self.moved(tid, CWD, a[0], CWD, a[1]),
                nr::RENAMEAT | nr::RENAMEAT2 => {
                    self.moved(tid, a[0] as i32, a[1], a[2] as i32, a[3])
                }
                nr::LINK => {
                    self.at(tid, CWD, a[0], Use::Read);
                    self.at(tid, CWD, a[1], replace)
                }
                nr::LINKAT => {
                    self.at(tid, a[0] as i32, a[1], Use::Read);
                    self.at(tid, a[2] as i32, a[3], replace)
                }
                nr::SYMLINK => self.at(tid, CWD, a[1], replace),
                nr::SYMLINKAT => self.at(tid, a[1] as i32, a[2], replace),
                nr::MKDIR => self.make_directory(tid, CWD, a[0]),
                nr::MKDIRAT => self.make_directory(tid, a[0] as i32, a[1]),
                nr::TRUNCATE => self.at(tid, CWD, a[0], replace),
                nr::UNLINK | nr::RMDIR => self.at(tid, CWD, a[0], Use::Remove),
                nr::UNLINKAT => self.at(tid, a[0] as i32, a[1], Use::Remove),
                nr::CONNECT => {
                    let len = (a[2] as usize).clamp(2, 110);
                    let Some(n) = self.remote(tid, a[1], len) else {
                        return;
                    };
                    if n < 2 {
                        return;
                    }
                    let addr = self.buffer[..n].to_vec();
                    match u16::from_ne_bytes([addr[0], addr[1]]) as i32 {
                        libc::AF_UNIX => {
                            let name = &addr[2..];
                            let end = if name.first() == Some(&0) {
                                name.len()
                            } else {
                                memchr::memchr(0, name).unwrap_or(name.len())
                            };
                            let caller = command_line(tid);
                            if self.report.sockets.len() < 64 {
                                self.report.sockets.push((
                                    PathBuf::from(OsString::from_vec(name[..end].to_vec())),
                                    caller,
                                ));
                            }
                        }
                        libc::AF_UNSPEC => (),
                        _ => self.report.network = true,
                    }
                }
                nr::SECCOMP => {
                    // Another listener cannot be added beneath this one, so
                    // that program will fail where it otherwise would not.
                    if a[0] == SECCOMP_SET_MODE_FILTER && a[1] & FLAG_NEW_LISTENER != 0 {
                        self.incomplete("a program needs a system call listener of its own");
                    }
                }
                nr::IO_URING_SETUP | nr::NAME_TO_HANDLE_AT | nr::OPEN_BY_HANDLE_AT => {
                    self.incomplete("a program can open files without naming them")
                }
                nr::CHROOT | nr::MOUNT | nr::PIVOT_ROOT | nr::UNSHARE | nr::SETNS => {
                    self.incomplete("a program changed its view of the filesystem")
                }
                _ => (),
            }
        }

        /// Programs make sure of a directory by trying to create it and
        /// every directory above it. Where one is already there the call
        /// fails, having shown only that.
        fn make_directory(&mut self, tid: u32, dirfd: i32, name: u64) {
            let Some(name) = self.string(tid, name) else {
                return;
            };
            let Some(path) = self.resolve(tid, dirfd, &name) else {
                return self.lost(tid, "a path could not be resolved");
            };
            let how = if fs::symlink_metadata(&path).is_ok() {
                Use::Probe
            } else {
                Use::Write {
                    replace: false,
                    read: false,
                }
            };
            self.touch(path, how)
        }

        fn moved(&mut self, tid: u32, from_dir: i32, from: u64, to_dir: i32, to: u64) {
            let source = self
                .string(tid, from)
                .and_then(|name| self.resolve(tid, from_dir, &name));
            let Some(source) = source else {
                return self.lost(tid, "a moved file could not be named");
            };
            // Linkers try to move an earlier output aside. With nothing
            // there the call fails and only shows that nothing was there.
            if fs::symlink_metadata(&source).is_err() {
                return self.touch(source, Use::Probe);
            }
            self.touch(source.clone(), Use::Remove);
            let (ours, origin) = match self.report.paths.get(&source) {
                Some(a) if !a.created => (false, a.origin.clone().or(Some(source))),
                _ => (true, None),
            };
            let Some(dest) = self
                .string(tid, to)
                .and_then(|name| self.resolve(tid, to_dir, &name))
            else {
                return self.lost(tid, "a moved file could not be named");
            };
            // The destination now holds what the source held: the command's
            // own data, or contents from before it started.
            self.touch(
                dest.clone(),
                Use::Write {
                    replace: true,
                    read: false,
                },
            );
            if !ours && let Some(access) = self.report.paths.get_mut(&dest) {
                access.created = false;
                access.made = 0;
                access.updated = true;
                access.origin = origin;
            }
        }
    }

    /// Whether a command started by this process would find a directory
    /// already open on one of its descriptors, to list without opening.
    fn inherits_a_directory() -> bool {
        let Ok(open) = fs::read_dir("/proc/self/fd") else {
            return true;
        };
        open.flatten().any(|entry| {
            let Some(fd) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<RawFd>().ok())
            else {
                return false;
            };
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            let status = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            // One that is closed when a program starts is not passed on,
            // and one that only names a place cannot be read.
            flags >= 0
                && flags & libc::FD_CLOEXEC == 0
                && status & libc::O_PATH == 0
                && fs::metadata(entry.path()).is_ok_and(|meta| meta.is_dir())
        })
    }

    /// Wait for a report, for the command to end, or for a note. `notes`
    /// is -1 where there is no socket for notes, and is then left out.
    fn readable(fd: RawFd, wake: RawFd, notes: RawFd, block: bool) -> (bool, bool, bool, bool) {
        let entry = |fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let mut fds = [entry(fd), entry(wake), entry(notes)];
        loop {
            let n = unsafe { libc::poll(fds.as_mut_ptr(), 3, if block { -1 } else { 0 }) };
            if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        (
            fds[0].revents & libc::POLLIN != 0,
            fds[0].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0,
            fds[1].revents != 0,
            fds[2].revents & libc::POLLIN != 0,
        )
    }

    /// Everything recorded for one observed command: what the command and
    /// its children did, and separately what happened beneath each process
    /// that asked for a record of its own.
    struct State {
        whole: Tracker,
        scopes: HashMap<u32, Tracker>,
        /// The scope each thread's reports belong to, and when that was
        /// last worked out.
        owner: HashMap<u32, (Option<u32>, u64)>,
        events: u64,
        /// Where watched programs send notes of what they are about to
        /// read. See `fast/shim.rs`.
        notes: Option<OwnedFd>,
        /// The command's first process, which is the first to report.
        leader: Option<u32>,
        /// Whether each thread runs the program the leader runs, and when
        /// that was last worked out.
        program: HashMap<u32, (bool, u64)>,
    }

    /// The process a thread belongs to and that process's parent.
    fn ids(tid: u32) -> Option<(u32, u32)> {
        let status = fs::read_to_string(format!("/proc/{tid}/status")).ok()?;
        let field = |name: &str| {
            status
                .lines()
                .find_map(|line| line.strip_prefix(name))
                .and_then(|value| value.trim().parse::<u32>().ok())
        };
        Some((field("Tgid:")?, field("PPid:")?))
    }

    impl State {
        /// Take in every note that has arrived. A program writes its note
        /// before it makes the call, and a call that changes anything is
        /// held until this process has looked, so taking the notes in
        /// before each report is looked at keeps things in the order they
        /// happened.
        fn take_notes(&mut self) {
            let Some(socket) = self.notes.as_ref().map(AsRawFd::as_raw_fd) else {
                return;
            };
            let mut note = [0u8; abi::LONGEST];
            loop {
                let n = unsafe {
                    libc::recv(
                        socket,
                        note.as_mut_ptr().cast(),
                        note.len(),
                        libc::MSG_DONTWAIT,
                    )
                };
                if n <= 0 {
                    return;
                }
                self.noted(&note[..n as usize]);
            }
        }

        /// One note: a program is about to open a file for reading, or to
        /// ask about a path.
        fn noted(&mut self, note: &[u8]) {
            let field = |at: usize| u16::from_le_bytes([note[at], note[at + 1]]) as usize;
            if note.len() < abi::HEADER
                || field(0) != note.len()
                || abi::HEADER + field(8) > note.len()
                || note[2] > abi::ASKED
            {
                return self
                    .whole
                    .incomplete("a note from a watched program could not be read");
            }
            let process = u32::from_le_bytes([note[4], note[5], note[6], note[7]]);
            let (directory, name) = note[abi::HEADER..].split_at(field(8));
            let path = joined(directory, name);
            let how = || {
                if note[2] == abi::OPENED {
                    Use::Read
                } else {
                    Use::Probe
                }
            };
            self.events += 1;
            let now = self.events;
            // The program may be gone by now. What it belonged to is known
            // from its first report, which came before any note.
            let scope = match self.owner.get(&process) {
                Some((scope, _)) => Some(*scope),
                None if ids(process).is_some() => Some(self.scope_of(process)),
                None => None,
            };
            let others = !matches!(scope, Some(None)) || !self.own_program(process);
            let record = |tracker: &mut Tracker| {
                tracker.caller = process;
                tracker.now = now;
                tracker.others = others;
                tracker.report.noted += 1;
                tracker.touch(path.clone(), how());
            };
            match scope {
                Some(Some(root)) => match self.scopes.get_mut(&root) {
                    Some(tracker) => record(tracker),
                    None => record(&mut self.whole),
                },
                Some(None) => record(&mut self.whole),
                // Nothing says whose it was: then it was everyone's.
                None => {
                    record(&mut self.whole);
                    self.scopes.values_mut().for_each(record);
                }
            }
        }

        /// Whether a thread runs the program the command itself is: the
        /// one its first process runs now. Cargo is that program in the
        /// process a toolchain manager started it in, and again where one
        /// of its own subcommands starts it for the build.
        fn own_program(&mut self, tid: u32) -> bool {
            let Some(leader) = self.leader else {
                return false;
            };
            if tid == leader {
                return true;
            }
            // As long as an answer about whose scope a thread is in.
            if let Some((own, at)) = self.program.get(&tid)
                && self.events - at < 200_000
            {
                return *own;
            }
            let program = |id: u32| {
                let meta = fs::metadata(format!("/proc/{id}/exe")).ok()?;
                Some((meta.dev(), meta.ino()))
            };
            let own = program(leader).is_some_and(|theirs| program(tid) == Some(theirs));
            self.program.insert(tid, (own, self.events));
            own
        }

        /// A thread is about to become another program, if the call
        /// succeeds.
        fn becomes_another(&mut self, tid: u32) {
            match self.leader {
                // Every answer was a comparison with the leader's program.
                Some(leader) if ids(tid).is_none_or(|(process, _)| process == leader) => {
                    self.program.clear()
                }
                _ => drop(self.program.remove(&tid)),
            }
        }

        /// The scope whose root process is an ancestor of this thread.
        fn scope_of(&mut self, tid: u32) -> Option<u32> {
            if self.scopes.is_empty() {
                return None;
            }
            // A thread id is not given out again until the whole range has
            // been used, so an answer stays right for a while. It is looked
            // up again long before the range could wrap.
            if let Some((scope, at)) = self.owner.get(&tid)
                && self.events - at < 200_000
            {
                return *scope;
            }
            let mut found = None;
            if let Some((process, mut parent)) = ids(tid) {
                // A scope's root reports for itself, not for the scope.
                if !self.scopes.contains_key(&process) {
                    for _ in 0..64 {
                        if self.scopes.contains_key(&parent) {
                            found = Some(parent);
                            break;
                        }
                        match ids(parent) {
                            Some((_, next)) if parent > 1 => parent = next,
                            _ => break,
                        }
                    }
                }
            }
            self.owner.insert(tid, (found, self.events));
            found
        }
    }

    /// Ask the scheduler to run this thread as soon as it has something to
    /// do. Every reported call waits for it, and it needs the processor for
    /// a few microseconds at a time, so on a busy machine queueing it behind
    /// whole time slices of other work is what makes observation slow. A
    /// short slice of its own puts it at the front. Kernels before 6.12
    /// ignore the request.
    fn answer_promptly() {
        #[repr(C)]
        struct Attr {
            size: u32,
            policy: u32,
            flags: u64,
            nice: i32,
            priority: u32,
            runtime: u64,
            deadline: u64,
            period: u64,
        }
        let attr = Attr {
            size: std::mem::size_of::<Attr>() as u32,
            policy: libc::SCHED_OTHER as u32,
            flags: 0,
            // Unchanged: asking for a better one would be refused.
            nice: unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) },
            priority: 0,
            runtime: 100_000,
            deadline: 0,
            period: 0,
        };
        unsafe { libc::syscall(libc::SYS_sched_setattr, 0, &raw const attr, 0) };
    }

    /// Answer one pending report. Returns false when there was none.
    fn serve_one(listener: RawFd, state: Option<&Mutex<State>>) -> bool {
        let mut notif = Notif::default();
        if unsafe { libc::ioctl(listener, IOCTL_RECV as _, &raw mut notif) } != 0 {
            return false;
        }
        if let Some(state) = state {
            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
            state.take_notes();
            state.events += 1;
            let (data, tid) = (notif.data, notif.pid);
            let state = &mut *state;
            let now = state.events;
            state.leader.get_or_insert(tid);
            let scope = state.scope_of(tid);
            let others = scope.is_some() || !state.own_program(tid);
            let tracker = match scope {
                Some(scope) => state.scopes.get_mut(&scope).unwrap(),
                None => &mut state.whole,
            };
            tracker.now = now;
            tracker.others = others;
            // Recording must never keep the command from continuing.
            let recorded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                tracker.handle(&data, tid)
            }));
            if recorded.is_err() {
                tracker.incomplete("the observer failed");
            }
            if matches!(data.nr as u32, nr::EXECVE | nr::EXECVEAT) {
                state.becomes_another(tid);
            }
        }
        let resp = Resp {
            id: notif.id,
            val: 0,
            error: 0,
            flags: NOTIF_FLAG_CONTINUE,
        };
        unsafe { libc::ioctl(listener, IOCTL_SEND as _, &raw const resp) };
        true
    }

    /// Hands out records for parts of an observed command.
    #[derive(Clone)]
    pub struct Scopes(Arc<Mutex<State>>);

    impl Scopes {
        /// From now on, record what the descendants of `root` do apart
        /// from the rest of the command.
        pub fn begin(&self, root: u32, config: Config) {
            let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
            state.scopes.insert(
                root,
                Tracker {
                    report: Report::default(),
                    config,
                    buffer: Vec::new(),
                    caller: 0,
                    now: 0,
                    others: true,
                },
            );
        }

        /// Whether `process` is the root of a scope or runs inside one.
        pub fn inside(&self, process: u32) -> bool {
            let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
            state.scopes.contains_key(&process) || state.scope_of(process).is_some()
        }

        /// Count what a scope recorded as the doing of the command as a
        /// whole after all, as it would have been without the scope.
        pub fn absorb(&self, report: Report) {
            let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
            state.whole.report.absorb(report);
        }

        /// The record for `root`'s descendants, which have all exited.
        pub fn end(&self, root: u32) -> Option<Report> {
            let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
            state.take_notes();
            let tracker = state.scopes.remove(&root)?;
            state.owner.retain(|_, (scope, _)| *scope != Some(root));
            let mut report = tracker.report;
            report.events = state.events;
            Some(report)
        }
    }

    pub struct Session {
        thread: JoinHandle<Option<OwnedFd>>,
        wake: OwnedFd,
        state: Arc<Mutex<State>>,
    }

    fn pair(kind: libc::c_int) -> io::Result<(OwnedFd, OwnedFd)> {
        let mut fds = [0 as RawFd; 2];
        let made = if kind == 0 {
            unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }
        } else {
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    kind | libc::SOCK_CLOEXEC,
                    0,
                    fds.as_mut_ptr(),
                )
            }
        };
        if made != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
    }

    pub fn spawn(cmd: &mut Command, config: Config) -> io::Result<(Child, Session)> {
        static FILTERS: [std::sync::OnceLock<Vec<Insn>>; 2] =
            [std::sync::OnceLock::new(), std::sync::OnceLock::new()];
        let listing = inherits_a_directory();
        let insns: &'static [Insn] = FILTERS[usize::from(listing)].get_or_init(|| filter(listing));
        let (ours, theirs) = pair(libc::SOCK_SEQPACKET)?;
        let (wake_read, wake) = pair(0)?;
        let child_sock = theirs.as_raw_fd();
        // A socket for the command's notes of what it reads, and the
        // library that writes them, where there is one and the descriptor
        // for it is free.
        let free = unsafe { libc::fcntl(NOTES_FD, libc::F_GETFD) } == -1;
        let notes = config
            .fast
            .as_ref()
            .filter(|_| free)
            .and_then(|library| Some((library, pair(libc::SOCK_DGRAM).ok()?)));
        let (reading, writing) = match notes {
            Some((library, (reading, writing))) => {
                let inode = fs::File::from(writing.try_clone()?).metadata()?.ino();
                cmd.env(abi::SOCKET, format!("{NOTES_FD}:{inode}"));
                // Ahead of whatever else the command loads that way.
                let given = cmd
                    .get_envs()
                    .find(|(name, _)| *name == "LD_PRELOAD")
                    .map(|(_, value)| value.map(OsStr::to_owned))
                    .unwrap_or_else(|| std::env::var_os("LD_PRELOAD"));
                let mut load = library.as_os_str().to_owned();
                let others = given.unwrap_or_default();
                for other in others
                    .as_bytes()
                    .split(|b| *b == b':' || *b == b' ')
                    .filter(|other| !other.is_empty())
                    .filter(|other| !is_fast_library(Path::new(OsStr::from_bytes(other))))
                {
                    load.push(":");
                    load.push(OsStr::from_bytes(other));
                }
                cmd.env("LD_PRELOAD", load);
                (Some(reading), Some(writing))
            }
            None => (None, None),
        };
        let notes_to = writing.as_ref().map(AsRawFd::as_raw_fd);
        unsafe {
            cmd.pre_exec(move || {
                // The copy stays open in the command and all it starts.
                if let Some(socket) = notes_to
                    && libc::dup2(socket, NOTES_FD) < 0
                {
                    return Err(io::Error::last_os_error());
                }
                let prog = Program {
                    len: insns.len() as u16,
                    filter: insns.as_ptr(),
                };
                install(child_sock, &prog)
            });
        }
        // Programs the command starts can tell that their file accesses are
        // already being recorded.
        cmd.env("JATO_OBSERVED", "1");
        // The command's own exec is reported, and the spawn below waits for
        // that exec, so the listener has to be served from another thread
        // before the spawn returns.
        let state = Arc::new(Mutex::new(State {
            whole: Tracker {
                report: Report::default(),
                config,
                buffer: Vec::new(),
                caller: 0,
                now: 0,
                others: false,
            },
            scopes: HashMap::new(),
            owner: HashMap::new(),
            events: 0,
            notes: reading,
            leader: None,
            program: HashMap::new(),
        }));
        let noted = {
            let state = state.lock().unwrap_or_else(|e| e.into_inner());
            state.notes.as_ref().map_or(-1, AsRawFd::as_raw_fd)
        };
        let shared = state.clone();
        let thread = std::thread::Builder::new()
            .name("jato-observer".into())
            .spawn(move || {
                let state = &*shared;
                let fail = |why: String| {
                    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                    state.whole.report.incomplete.get_or_insert(why);
                };
                let listener = match receive_listener(&ours) {
                    Ok(fd) => fd,
                    Err(e) => {
                        fail(format!("no observer: {e}"));
                        return None;
                    }
                };
                drop(ours);
                let mut flags = FD_SYNC_WAKE_UP;
                unsafe { libc::ioctl(listener.as_raw_fd(), IOCTL_SET_FLAGS as _, &raw mut flags) };
                answer_promptly();
                let fd = listener.as_raw_fd();
                loop {
                    let (ready, closed, woken, notes) =
                        readable(fd, wake_read.as_raw_fd(), noted, true);
                    if ready {
                        serve_one(fd, Some(state));
                    } else if closed {
                        return None;
                    } else if woken {
                        // The command exited. Record what was already asked
                        // and leave any surviving program to a helper.
                        loop {
                            let (ready, closed, ..) =
                                readable(fd, wake_read.as_raw_fd(), -1, false);
                            if ready && serve_one(fd, Some(state)) {
                                continue;
                            }
                            return (!closed).then_some(listener);
                        }
                    } else if notes {
                        // Nothing to answer, and programs are reading:
                        // make room for more of what they say.
                        let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                        state.take_notes();
                    }
                }
            })?;
        let spawned = cmd.spawn();
        drop(theirs);
        drop(writing);
        match spawned {
            Ok(child) => Ok((
                child,
                Session {
                    thread,
                    wake,
                    state,
                },
            )),
            Err(e) => {
                let _ = thread.join();
                Err(e)
            }
        }
    }

    impl Session {
        pub fn scopes(&self) -> Scopes {
            Scopes(self.state.clone())
        }

        /// Call after the command has exited. Returns what was recorded
        /// outside every scope.
        pub fn finish(self) -> Report {
            let _ = unsafe { libc::write(self.wake.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
            let survivors = self.thread.join();
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.take_notes();
            // Whatever is still running finds nobody listening, and has
            // its calls reported again.
            state.notes = None;
            let mut report = std::mem::take(&mut state.whole.report);
            report.events = state.events;
            match survivors {
                Ok(Some(listener)) => {
                    report.survivors = true;
                    keep_answering(listener);
                }
                Ok(None) => (),
                Err(_) => {
                    report
                        .incomplete
                        .get_or_insert_with(|| "the observer failed".into());
                }
            }
            report
        }
    }

    /// A program the command left running still reports its calls, and each
    /// would fail once nobody answered. Hand the listener to a small process
    /// that allows them until the last such program exits.
    fn keep_answering(listener: OwnedFd) {
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let fd = listener.as_raw_fd();
        let mut cmd = Command::new(exe);
        cmd.arg("__answer")
            .arg(fd.to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .env_clear();
        unsafe {
            cmd.pre_exec(move || {
                libc::setsid();
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let _ = cmd.spawn();
    }

    pub fn answer(fd: RawFd) -> i32 {
        loop {
            let (ready, closed, ..) = readable(fd, -1, -1, true);
            if ready {
                serve_one(fd, None);
            } else if closed {
                return 0;
            }
        }
    }

    pub fn available() -> bool {
        use std::sync::OnceLock;
        static AVAILABLE: OnceLock<bool> = OnceLock::new();
        *AVAILABLE.get_or_init(|| {
            // A filter with a listener cannot be added beneath another one,
            // and whoever holds that one already sees these calls.
            if std::env::var_os("JATO_OBSERVE").is_some_and(|v| v == "0")
                || std::env::var_os("JATO_OBSERVED").is_some()
            {
                return false;
            }
            let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
            if unsafe { libc::uname(&raw mut uts) } != 0 {
                return false;
            }
            let release = unsafe { std::ffi::CStr::from_ptr(uts.release.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            let mut parts = release
                .split(['.', '-'])
                .map(|p| p.parse::<u32>().unwrap_or(0));
            // Letting a reported call continue needs Linux 5.5.
            (parts.next().unwrap_or(0), parts.next().unwrap_or(0)) >= (5, 5)
        })
    }
}

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
mod imp {
    use super::*;
    pub struct Session;
    #[derive(Clone)]
    pub struct Scopes;
    impl Scopes {
        pub fn begin(&self, _: u32, _: Config) {}
        pub fn inside(&self, _: u32) -> bool {
            false
        }
        pub fn absorb(&self, _: Report) {}
        pub fn end(&self, _: u32) -> Option<Report> {
            None
        }
    }
    impl Session {
        pub fn scopes(&self) -> Scopes {
            Scopes
        }
        pub fn finish(self) -> Report {
            Report::default()
        }
    }
    pub fn spawn(_: &mut Command, _: Config) -> io::Result<(Child, Session)> {
        Err(io::Error::other("observation is not supported here"))
    }
    pub fn available() -> bool {
        false
    }
    pub fn answer(_: RawFd) -> i32 {
        1
    }
}

pub use imp::{Scopes, Session, answer, available, spawn};

#[cfg(test)]
mod tests {
    use super::*;

    /// A path first used at `first`, found as a file or not there.
    fn seen(first: u64, there: bool) -> Access {
        Access {
            kind: if there { Kind::File } else { Kind::Absent },
            stamp: None,
            links: 1,
            link: None,
            read: false,
            probed: false,
            listing: None,
            executed: false,
            written: false,
            created: false,
            updated: false,
            removed: false,
            appeared: false,
            origin: None,
            writer: None,
            others: false,
            first,
            made: 0,
        }
    }

    fn made(first: u64, there: bool, at: u64) -> Access {
        Access {
            written: true,
            created: true,
            made: at,
            ..seen(first, there)
        }
    }

    fn read(first: u64) -> Access {
        Access {
            read: true,
            ..seen(first, true)
        }
    }

    #[test]
    fn a_part_s_record_is_taken_in_as_the_command_s_own_doing() {
        // The command made a file and a part of it read that file: the
        // command read its own work.
        let mut whole = made(3, false, 3);
        whole.absorb(read(7));
        assert!(whole.created && whole.written && !whole.read);
        assert_eq!(whole.kind, Kind::Absent);

        // The part made it and the command read it afterwards: the same.
        let mut whole = read(9);
        whole.absorb(made(4, false, 4));
        assert!(whole.created && whole.written && !whole.read);
        assert_eq!(whole.kind, Kind::Absent);

        // The command read what was there and the part wrote over it.
        let mut whole = read(2);
        whole.absorb(made(6, true, 6));
        assert!(whole.read && whole.created && whole.written);

        // The command only looked, the part read what was there, and the
        // command wrote over it later still: what the part read came from
        // outside.
        let mut whole = Access {
            probed: true,
            ..made(1, true, 9)
        };
        whole.absorb(read(5));
        assert!(whole.read && whole.created);

        // Both read it. It is what it was when the first of them looked.
        let mut whole = read(8);
        let mut part = read(2);
        part.links = 2;
        whole.absorb(part);
        assert!(whole.read && !whole.written);
        assert_eq!((whole.first, whole.links), (2, 2));
    }

    #[test]
    fn a_part_s_report_joins_the_command_s() {
        let mut whole = Report::default();
        whole.paths.insert("/nowhere/shared".into(), read(1));
        let mut part = Report {
            network: true,
            ..Report::default()
        };
        part.paths.insert("/nowhere/only-the-part".into(), read(4));
        part.paths.insert(
            "/nowhere/shared".into(),
            Access {
                removed: true,
                ..seen(5, true)
            },
        );
        whole.absorb(part);
        assert!(whole.network);
        assert!(whole.paths["/nowhere/only-the-part".as_ref() as &Path].read);
        // Nothing is there now, so the removal was the last word.
        let shared = &whole.paths["/nowhere/shared".as_ref() as &Path];
        assert!(shared.removed && shared.read);
    }
}
