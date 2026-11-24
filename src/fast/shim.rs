//! Loaded into every program Jato watches, ahead of the C library.
//!
//! Jato learns what a program examines from the kernel: a filter reports
//! each system call that names a file, and the program waits until Jato
//! has looked. On a machine with every processor in use that wait is the
//! cost of watching, since the program gives up its turn each time.
//!
//! Most of those calls only read: opening a file to read it, asking
//! whether a path exists. For those nothing has to be looked at before the
//! call goes ahead. This library stands in for the C library's functions
//! that make them. It sends a note of the path down a socket to Jato and
//! then makes the call from one fixed address, which the filter lets
//! through. Every other call, from here or anywhere else in the program,
//! is reported as before, so a program that does not load this library, or
//! reaches the kernel some other way, is watched exactly as it was.
//!
//! Nothing here allocates, takes a lock or needs more of the C library
//! than `errno`.
#![no_std]
#![allow(clippy::missing_safety_doc)]

use core::arch::asm;
use core::ffi::{c_char, c_int, c_uint, c_void};
use core::sync::atomic::{AtomicI32, AtomicU64, Ordering};

include!("abi.rs");

unsafe extern "C" {
    fn __errno_location() -> *mut c_int;
    fn fdopendir(fd: c_int) -> *mut c_void;
    static environ: *const *const c_char;
}

const ENOSYS: isize = 38;
const AT_SYMLINK_NOFOLLOW: c_int = 0x100;
const AT_EACCESS: c_int = 0x200;

// The parts of the compiler's own library that are linked in name this
// for unwinding, which nothing here does. Defined out of sight, so that it
// is not taken for the one a Rust program brings.
#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".hidden rust_eh_personality",
    ".globl rust_eh_personality",
    ".type rust_eh_personality, @function",
    "rust_eh_personality:",
    "ud2",
);

#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    ".hidden rust_eh_personality",
    ".globl rust_eh_personality",
    ".type rust_eh_personality, @function",
    "rust_eh_personality:",
    "brk #0",
);

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    unsafe { call(nr::EXIT_GROUP, [134, 0, 0, 0, 0, 0]) };
    loop {}
}

#[cfg(target_arch = "x86_64")]
mod nr {
    pub const CLOSE: usize = 3;
    pub const STAT: usize = 4;
    pub const FSTAT: usize = 5;
    pub const LSTAT: usize = 6;
    pub const MMAP: usize = 9;
    pub const MPROTECT: usize = 10;
    pub const ACCESS: usize = 21;
    pub const GETPID: usize = 39;
    pub const SENDTO: usize = 44;
    pub const GETCWD: usize = 79;
    pub const EXIT_GROUP: usize = 231;
    pub const OPENAT: usize = 257;
    pub const NEWFSTATAT: usize = 262;
    pub const READLINKAT: usize = 267;
    pub const FACCESSAT: usize = 269;
    pub const STATX: usize = 332;
    pub const FACCESSAT2: usize = 439;
}

#[cfg(target_arch = "aarch64")]
mod nr {
    pub const GETCWD: usize = 17;
    pub const FACCESSAT: usize = 48;
    pub const OPENAT: usize = 56;
    pub const CLOSE: usize = 57;
    pub const READLINKAT: usize = 78;
    pub const NEWFSTATAT: usize = 79;
    pub const FSTAT: usize = 80;
    pub const EXIT_GROUP: usize = 94;
    pub const GETPID: usize = 172;
    pub const SENDTO: usize = 206;
    pub const MMAP: usize = 222;
    pub const MPROTECT: usize = 226;
    pub const STATX: usize = 291;
    pub const FACCESSAT2: usize = 439;
}

const AT_FDCWD: c_int = -100;
const AT_EMPTY_PATH: c_int = 0x1000;
const O_ACCMODE: c_int = 3;
const O_CREAT: c_int = 0o100;
const O_TRUNC: c_int = 0o1000;
const O_TMPFILE: c_int = 0o20200000;
const O_PATH: c_int = 0o10000000;
const O_NONBLOCK: c_int = 0o4000;
const O_DIRECTORY: c_int = 0o200000;
const O_CLOEXEC: c_int = 0o2000000;
const S_IFMT: u32 = 0o170000;
const S_IFSOCK: u32 = 0o140000;
const MSG_NOSIGNAL: usize = 0x4000;

/// The socket to Jato, or -1 while there is none, and the socket's inode.
static SOCKET_FD: AtomicI32 = AtomicI32::new(-1);
static SOCKET_INODE: AtomicU64 = AtomicU64::new(0);

/// A system call made here, where the filter reports it if it is one of
/// those it reports.
#[cfg(target_arch = "x86_64")]
unsafe fn call(number: usize, a: [usize; 6]) -> isize {
    let result: isize;
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            in("rdi") a[0], in("rsi") a[1], in("rdx") a[2],
            in("r10") a[3], in("r8") a[4], in("r9") a[5],
            lateout("rcx") _, lateout("r11") _,
            options(nostack),
        );
    }
    result
}

#[cfg(target_arch = "aarch64")]
unsafe fn call(number: usize, a: [usize; 6]) -> isize {
    let result: isize;
    unsafe {
        asm!(
            "svc #0",
            in("x8") number,
            inlateout("x0") a[0] as isize => result,
            in("x1") a[1], in("x2") a[2],
            in("x3") a[3], in("x4") a[4], in("x5") a[5],
            options(nostack),
        );
    }
    result
}

/// The same call made from the fixed address, which the filter lets
/// through for calls that only read.
#[cfg(target_arch = "x86_64")]
unsafe fn quiet(number: usize, a: [usize; 6]) -> isize {
    let result: isize;
    unsafe {
        asm!(
            "call {stub}",
            stub = in(reg) STUB,
            inlateout("rax") number as isize => result,
            in("rdi") a[0], in("rsi") a[1], in("rdx") a[2],
            in("r10") a[3], in("r8") a[4], in("r9") a[5],
            lateout("rcx") _, lateout("r11") _,
        );
    }
    result
}

#[cfg(target_arch = "aarch64")]
unsafe fn quiet(number: usize, a: [usize; 6]) -> isize {
    let result: isize;
    unsafe {
        asm!(
            "blr {stub}",
            stub = in(reg) STUB,
            in("x8") number,
            inlateout("x0") a[0] as isize => result,
            in("x1") a[1], in("x2") a[2],
            in("x3") a[3], in("x4") a[4], in("x5") a[5],
            lateout("x30") _,
            options(nostack),
        );
    }
    result
}

/// What the C library's functions return: the result, or -1 with the
/// reason in `errno`.
unsafe fn answer(result: isize) -> c_int {
    if result < 0 {
        unsafe { *__errno_location() = -result as c_int };
        -1
    } else {
        result as c_int
    }
}

unsafe fn length(text: *const c_char) -> usize {
    let mut n = 0;
    while unsafe { *text.add(n) } != 0 {
        n += 1;
    }
    n
}

/// The kernel's `struct stat`, as far as the mode and inode.
#[cfg(target_arch = "x86_64")]
#[repr(C)]
struct Stat {
    dev: u64,
    ino: u64,
    nlink: u64,
    mode: u32,
    rest: [u8; 116],
}

#[cfg(target_arch = "aarch64")]
#[repr(C)]
struct Stat {
    dev: u64,
    ino: u64,
    mode: u32,
    nlink: u32,
    rest: [u8; 104],
}

/// Whether descriptor `fd` is still the socket this library was told of. A
/// program may have closed it and opened something of its own there.
unsafe fn still_the_socket(fd: c_int) -> bool {
    let mut stat = core::mem::MaybeUninit::<Stat>::uninit();
    let found = unsafe { call(nr::FSTAT, [fd as usize, stat.as_mut_ptr() as usize, 0, 0, 0, 0]) };
    if found != 0 {
        return false;
    }
    let stat = unsafe { stat.assume_init() };
    stat.mode & S_IFMT == S_IFSOCK && stat.ino == SOCKET_INODE.load(Ordering::Relaxed)
}

/// Tell Jato that this program is about to open `name` for reading, or to
/// ask about it. False if it could not be said, and the call has to be
/// made where Jato sees it.
unsafe fn note(kind: u8, dirfd: c_int, name: *const c_char) -> bool {
    let fd = SOCKET_FD.load(Ordering::Relaxed);
    if fd < 0 || name.is_null() {
        return false;
    }
    let name_length = unsafe { length(name) };
    let name_bytes = unsafe { core::slice::from_raw_parts(name as *const u8, name_length) };
    // Names under /proc/self mean something else to whoever reads them
    // later, and an empty name means the descriptor itself.
    if name_length == 0 || name_bytes.starts_with(b"/proc/") {
        return false;
    }
    let mut buffer = [0u8; LONGEST];
    let mut used = HEADER;
    if name_bytes[0] != b'/' {
        let room = LONGEST - HEADER;
        let found = if dirfd == AT_FDCWD {
            // The length it reports counts the byte that ends the text.
            let n = unsafe {
                call(nr::GETCWD, [buffer.as_mut_ptr().add(HEADER) as usize, room, 0, 0, 0, 0])
            };
            n - 1
        } else {
            let mut link = *b"/proc/self/fd/\0\0\0\0\0\0\0\0\0\0\0\0";
            let mut digits = [0u8; 11];
            let mut value = dirfd as u32;
            let mut count = 0;
            if dirfd < 0 {
                return false;
            }
            loop {
                digits[count] = b'0' + (value % 10) as u8;
                count += 1;
                value /= 10;
                if value == 0 {
                    break;
                }
            }
            for i in 0..count {
                link[14 + i] = digits[count - 1 - i];
            }
            unsafe {
                call(
                    nr::READLINKAT,
                    [
                        AT_FDCWD as usize,
                        link.as_ptr() as usize,
                        buffer.as_mut_ptr().add(HEADER) as usize,
                        room,
                        0,
                        0,
                    ],
                )
            }
        };
        // A directory that fills the room may have been cut short.
        if found <= 0 || found as usize >= room || buffer[HEADER] != b'/' {
            return false;
        }
        used += found as usize;
    }
    let directory = used - HEADER;
    if used + name_length > LONGEST {
        return false;
    }
    buffer[used..used + name_length].copy_from_slice(name_bytes);
    used += name_length;
    let process = unsafe { call(nr::GETPID, [0; 6]) } as u32;
    buffer[0..2].copy_from_slice(&(used as u16).to_le_bytes());
    buffer[2] = kind;
    buffer[3] = 0;
    buffer[4..8].copy_from_slice(&process.to_le_bytes());
    buffer[8..10].copy_from_slice(&(directory as u16).to_le_bytes());
    if !unsafe { still_the_socket(fd) } {
        SOCKET_FD.store(-1, Ordering::Relaxed);
        return false;
    }
    // One message for each note. With nobody listening any more this
    // fails, without the signal a pipe would send.
    let sent = unsafe {
        call(nr::SENDTO, [fd as usize, buffer.as_ptr() as usize, used, MSG_NOSIGNAL, 0, 0])
    };
    if sent != used as isize {
        SOCKET_FD.store(-1, Ordering::Relaxed);
        return false;
    }
    true
}

/// A system call that names a path it only reads or asks about: noted and
/// made quietly where that can be done, and made in the open otherwise.
unsafe fn reading(kind: u8, dirfd: c_int, name: *const c_char, number: usize, a: [usize; 6]) -> isize {
    if unsafe { note(kind, dirfd, name) } {
        unsafe { quiet(number, a) }
    } else {
        unsafe { call(number, a) }
    }
}

fn only_reads(flags: c_int) -> bool {
    flags & O_ACCMODE == 0 && flags & (O_CREAT | O_TRUNC) == 0 && flags & O_TMPFILE != O_TMPFILE
}

unsafe fn open_at(dirfd: c_int, name: *const c_char, flags: c_int, mode: c_uint) -> c_int {
    let a = [dirfd as usize, name as usize, flags as usize, mode as usize, 0, 0];
    let result = if only_reads(flags) {
        // Opened only to name the place, it cannot be read or listed.
        let kind = if flags & O_PATH != 0 { ASKED } else { OPENED };
        unsafe { reading(kind, dirfd, name, nr::OPENAT, a) }
    } else {
        unsafe { call(nr::OPENAT, a) }
    };
    unsafe { answer(result) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn open(name: *const c_char, flags: c_int, mode: c_uint) -> c_int {
    unsafe { open_at(AT_FDCWD, name, flags, mode) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn open64(name: *const c_char, flags: c_int, mode: c_uint) -> c_int {
    unsafe { open_at(AT_FDCWD, name, flags, mode) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn openat(dirfd: c_int, name: *const c_char, flags: c_int, mode: c_uint) -> c_int {
    unsafe { open_at(dirfd, name, flags, mode) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn openat64(dirfd: c_int, name: *const c_char, flags: c_int, mode: c_uint) -> c_int {
    unsafe { open_at(dirfd, name, flags, mode) }
}

// The forms the C library's headers substitute when a program is compiled
// with its checks on. They take no mode.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __open_2(name: *const c_char, flags: c_int) -> c_int {
    unsafe { open_at(AT_FDCWD, name, flags, 0) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __open64_2(name: *const c_char, flags: c_int) -> c_int {
    unsafe { open_at(AT_FDCWD, name, flags, 0) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __openat_2(dirfd: c_int, name: *const c_char, flags: c_int) -> c_int {
    unsafe { open_at(dirfd, name, flags, 0) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __openat64_2(dirfd: c_int, name: *const c_char, flags: c_int) -> c_int {
    unsafe { open_at(dirfd, name, flags, 0) }
}

/// Opening a directory to list it. The C library's own way of doing it
/// makes the call from inside itself, where it would be reported. The
/// entries are read by the C library as ever.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn opendir(name: *const c_char) -> *mut c_void {
    let fd = unsafe { open_at(AT_FDCWD, name, O_NONBLOCK | O_DIRECTORY | O_CLOEXEC, 0) };
    if fd < 0 {
        return core::ptr::null_mut();
    }
    let listing = unsafe { fdopendir(fd) };
    if listing.is_null() {
        unsafe { call(nr::CLOSE, [fd as usize, 0, 0, 0, 0, 0]) };
    }
    listing
}

unsafe fn stat_at(dirfd: c_int, name: *const c_char, into: *mut c_void, flags: c_int) -> c_int {
    let a = [dirfd as usize, name as usize, into as usize, flags as usize, 0, 0];
    let result = if flags & AT_EMPTY_PATH == 0 {
        unsafe { reading(ASKED, dirfd, name, nr::NEWFSTATAT, a) }
    } else {
        unsafe { call(nr::NEWFSTATAT, a) }
    };
    unsafe { answer(result) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn stat(name: *const c_char, into: *mut c_void) -> c_int {
    #[cfg(target_arch = "x86_64")]
    {
        let a = [name as usize, into as usize, 0, 0, 0, 0];
        unsafe { answer(reading(ASKED, AT_FDCWD, name, nr::STAT, a)) }
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        stat_at(AT_FDCWD, name, into, 0)
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn stat64(name: *const c_char, into: *mut c_void) -> c_int {
    unsafe { stat(name, into) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lstat(name: *const c_char, into: *mut c_void) -> c_int {
    #[cfg(target_arch = "x86_64")]
    {
        let a = [name as usize, into as usize, 0, 0, 0, 0];
        unsafe { answer(reading(ASKED, AT_FDCWD, name, nr::LSTAT, a)) }
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        stat_at(AT_FDCWD, name, into, AT_SYMLINK_NOFOLLOW)
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lstat64(name: *const c_char, into: *mut c_void) -> c_int {
    unsafe { lstat(name, into) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fstatat(dirfd: c_int, name: *const c_char, into: *mut c_void, flags: c_int) -> c_int {
    unsafe { stat_at(dirfd, name, into, flags) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fstatat64(dirfd: c_int, name: *const c_char, into: *mut c_void, flags: c_int) -> c_int {
    unsafe { stat_at(dirfd, name, into, flags) }
}

// What programs built against an older C library call for the same.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __xstat(_version: c_int, name: *const c_char, into: *mut c_void) -> c_int {
    unsafe { stat(name, into) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __xstat64(_version: c_int, name: *const c_char, into: *mut c_void) -> c_int {
    unsafe { stat(name, into) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __lxstat(_version: c_int, name: *const c_char, into: *mut c_void) -> c_int {
    unsafe { lstat(name, into) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __lxstat64(_version: c_int, name: *const c_char, into: *mut c_void) -> c_int {
    unsafe { lstat(name, into) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __fxstatat(
    _version: c_int,
    dirfd: c_int,
    name: *const c_char,
    into: *mut c_void,
    flags: c_int,
) -> c_int {
    unsafe { stat_at(dirfd, name, into, flags) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn __fxstatat64(
    _version: c_int,
    dirfd: c_int,
    name: *const c_char,
    into: *mut c_void,
    flags: c_int,
) -> c_int {
    unsafe { stat_at(dirfd, name, into, flags) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn statx(
    dirfd: c_int,
    name: *const c_char,
    flags: c_int,
    mask: c_uint,
    into: *mut c_void,
) -> c_int {
    let a = [dirfd as usize, name as usize, flags as usize, mask as usize, into as usize, 0];
    let result = if flags & AT_EMPTY_PATH == 0 {
        unsafe { reading(ASKED, dirfd, name, nr::STATX, a) }
    } else {
        unsafe { call(nr::STATX, a) }
    };
    unsafe { answer(result) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn access(name: *const c_char, mode: c_int) -> c_int {
    #[cfg(target_arch = "x86_64")]
    {
        let a = [name as usize, mode as usize, 0, 0, 0, 0];
        unsafe { answer(reading(ASKED, AT_FDCWD, name, nr::ACCESS, a)) }
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        faccessat(AT_FDCWD, name, mode, 0)
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn faccessat(dirfd: c_int, name: *const c_char, mode: c_int, flags: c_int) -> c_int {
    // Without flags the older call does, which every kernel has.
    let (number, a) = if flags == 0 {
        (nr::FACCESSAT, [dirfd as usize, name as usize, mode as usize, 0, 0, 0])
    } else {
        (nr::FACCESSAT2, [dirfd as usize, name as usize, mode as usize, flags as usize, 0, 0])
    };
    let mut result = if flags & AT_EMPTY_PATH == 0 {
        unsafe { reading(ASKED, dirfd, name, number, a) }
    } else {
        unsafe { call(number, a) }
    };
    // A kernel without the newer call. Asking as the effective user is
    // the same question for a program that has not changed users, which
    // is how the C library answers it there.
    if result == -ENOSYS && flags == AT_EACCESS {
        let a = [dirfd as usize, name as usize, mode as usize, 0, 0, 0];
        result = unsafe { call(nr::FACCESSAT, a) };
    }
    unsafe { answer(result) }
}

/// The value of `JATO_NOTES_TO` in this program's environment: the socket's
/// descriptor and inode. The environment is the C library's once it has
/// one, and before that the one the loader hands to whatever it starts.
unsafe fn told(handed: *const *const c_char) -> Option<(c_int, u64)> {
    let mut entry = unsafe { environ };
    if entry.is_null() {
        entry = handed;
    }
    if entry.is_null() {
        return None;
    }
    loop {
        let text = unsafe { *entry };
        if text.is_null() {
            return None;
        }
        let bytes = unsafe { core::slice::from_raw_parts(text as *const u8, length(text)) };
        if let Some(value) = bytes.strip_prefix(b"JATO_NOTES_TO=") {
            let mut parts = value.split(|b| *b == b':');
            let number = |digits: &[u8]| -> Option<u64> {
                if digits.is_empty() || digits.len() > 19 {
                    return None;
                }
                let mut n = 0u64;
                for d in digits {
                    if !d.is_ascii_digit() {
                        return None;
                    }
                    n = n * 10 + (d - b'0') as u64;
                }
                Some(n)
            };
            let fd = number(parts.next()?)?;
            let inode = number(parts.next()?)?;
            return (fd < 1 << 20).then_some((fd as c_int, inode));
        }
        entry = unsafe { entry.add(1) };
    }
}

/// Run when the library is loaded: put the two instructions at the fixed
/// address and find the socket. If either cannot be done, every call is
/// made in the open, as without this library.
extern "C" fn start(_: c_int, _: *const *const c_char, handed: *const *const c_char) {
    const PROT_READ: usize = 1;
    const PROT_WRITE: usize = 2;
    const PROT_EXEC: usize = 4;
    const MAP_PRIVATE: usize = 2;
    const MAP_ANONYMOUS: usize = 0x20;
    const MAP_FIXED_NOREPLACE: usize = 0x100000;
    unsafe {
        let Some((fd, inode)) = told(handed) else {
            return;
        };
        let page = call(
            nr::MMAP,
            [
                STUB,
                STUB_SIZE,
                PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE,
                usize::MAX,
                0,
            ],
        );
        if page as usize != STUB {
            return;
        }
        #[cfg(target_arch = "x86_64")]
        {
            // syscall; ret
            let code = STUB as *mut u8;
            code.write(0x0f);
            code.add(1).write(0x05);
            code.add(2).write(0xc3);
            if call(
                nr::MPROTECT,
                [STUB, STUB_SIZE, PROT_READ | PROT_EXEC, 0, 0, 0],
            ) != 0
            {
                return;
            }
        }
        #[cfg(target_arch = "aarch64")]
        {
            // svc #0; ret
            let code = STUB as *mut u32;
            code.write(0xd400_0001);
            code.add(1).write(0xd65f_03c0);
            if call(
                nr::MPROTECT,
                [STUB, STUB_SIZE, PROT_READ | PROT_EXEC, 0, 0, 0],
            ) != 0
            {
                return;
            }
            asm!(
                "dc cvau, {p}",
                "dsb ish",
                "ic ivau, {p}",
                "dsb ish",
                "isb",
                p = in(reg) STUB,
                options(nostack),
            );
        }
        SOCKET_INODE.store(inode, Ordering::Relaxed);
        if still_the_socket(fd) {
            SOCKET_FD.store(fd, Ordering::Relaxed);
        }
    }
}

#[used]
#[unsafe(link_section = ".init_array")]
static START: extern "C" fn(c_int, *const *const c_char, *const *const c_char) = start;
