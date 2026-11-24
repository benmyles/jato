//! Minimal preload library that hands Cargo build-script executions to
//! `jato __script` for observed caching.
//!
//! Exports zero C library symbols so it cannot interfere with any program it is
//! loaded into. Its `.init_array` constructor checks whether the current
//! process is a Cargo build script under `JATO_SCRIPT_TARGET` (with `OUT_DIR`
//! and `JATO_SESSION` set and `JATO_SCRIPT_DIRECT` unset) and, if so, replaces
//! the process with `jato __script <exe> [args...]`. Everything else returns
//! immediately with no allocations, no locks, and no libc calls.
#![no_std]
#![allow(clippy::missing_safety_doc)]

use core::arch::asm;
use core::ffi::{c_char, c_int};

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
    pub const EXECVE: usize = 59;
    pub const EXIT_GROUP: usize = 231;
    pub const READLINKAT: usize = 267;
}

#[cfg(target_arch = "aarch64")]
mod nr {
    pub const READLINKAT: usize = 78;
    pub const EXIT_GROUP: usize = 94;
    pub const EXECVE: usize = 221;
}

const AT_FDCWD: usize = (-100isize) as usize;

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

unsafe fn cstr_len(ptr: *const c_char) -> usize {
    let mut n = 0;
    while unsafe { *ptr.add(n) } != 0 {
        n += 1;
    }
    n
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    let mut i = 0;
    let end = haystack.len() - needle.len();
    while i <= end {
        if &haystack[i..i + needle.len()] == needle {
            return true;
        }
        i += 1;
    }
    false
}

extern "C" fn start(argc: c_int, argv: *const *const c_char, envp: *const *const c_char) {
    if argc < 1 || argc > 64 || argv.is_null() || envp.is_null() {
        return;
    }
    let mut jato_bin: *const c_char = core::ptr::null();
    let mut target: &[u8] = &[];
    let mut has_out_dir = false;
    let mut has_session = false;
    unsafe {
        let mut entry = envp;
        loop {
            let text = *entry;
            if text.is_null() {
                break;
            }
            let len = cstr_len(text);
            let bytes = core::slice::from_raw_parts(text as *const u8, len);
            if bytes.starts_with(b"JATO_SCRIPT_DIRECT=") {
                return;
            } else if bytes.starts_with(b"JATO_SCRIPT_BIN=") && len > 16 {
                jato_bin = text.add(16);
            } else if let Some(rest) = bytes.strip_prefix(b"JATO_SCRIPT_TARGET=") {
                target = rest;
            } else if bytes.starts_with(b"OUT_DIR=") && len > 8 {
                has_out_dir = true;
            } else if bytes.starts_with(b"JATO_SESSION=") && len > 13 {
                has_session = true;
            }
            entry = entry.add(1);
        }
        if jato_bin.is_null() || target.is_empty() || !has_out_dir || !has_session {
            return;
        }
        while target.last() == Some(&b'/') {
            target = &target[..target.len() - 1];
        }
        if target.is_empty() {
            return;
        }
        let mut exe_buf = [0u8; 4096];
        let n = call(
            nr::READLINKAT,
            [
                AT_FDCWD,
                c"/proc/self/exe".as_ptr() as usize,
                exe_buf.as_mut_ptr() as usize,
                exe_buf.len() - 1,
                0,
                0,
            ],
        );
        if n <= 0 || (n as usize) >= exe_buf.len() {
            return;
        }
        let n = n as usize;
        exe_buf[n] = 0;
        let exe = &exe_buf[..n];
        if exe.len() <= target.len() + 1 || &exe[..target.len()] != target || exe[target.len()] != b'/' {
            return;
        }
        let rel = &exe[target.len() + 1..];
        if !(rel.starts_with(b"build/") || contains_subslice(rel, b"/build/")) {
            return;
        }
        let mut slash = rel.len();
        while slash > 0 {
            slash -= 1;
            if rel[slash] == b'/' {
                slash += 1;
                break;
            }
        }
        let file_name = &rel[slash..];
        if !(file_name.starts_with(b"build-script-") || file_name.starts_with(b"build_script_")) {
            return;
        }
        let mut new_argv: [*const c_char; 68] = [core::ptr::null(); 68];
        new_argv[0] = jato_bin;
        new_argv[1] = c"__script".as_ptr();
        new_argv[2] = exe_buf.as_ptr() as *const c_char;
        let count = argc as usize;
        let mut i = 1;
        while i < count {
            new_argv[2 + i] = *argv.add(i);
            i += 1;
        }
        new_argv[2 + count] = core::ptr::null();
        let _ = call(
            nr::EXECVE,
            [
                jato_bin as usize,
                new_argv.as_ptr() as usize,
                envp as usize,
                0,
                0,
                0,
            ],
        );
    }
}

#[used]
#[unsafe(link_section = ".init_array")]
static START: extern "C" fn(c_int, *const *const c_char, *const *const c_char) = start;
