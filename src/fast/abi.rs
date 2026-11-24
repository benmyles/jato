// What the library loaded into watched programs and Jato agree on.

/// Where the library puts the two instructions it makes its quiet system
/// calls from. Aligned to 64 KiB so it is page-aligned on 4K, 16K, and 64K
/// kernels. The filter lets a call that only reads through when it comes
/// from here, and reports every other.
pub const STUB: usize = 0x6a61_746f_0000;

/// Size of the mapped stub page (64 KiB covers 4K, 16K, and 64K kernels).
pub const STUB_SIZE: usize = 65_536;

/// The address a system call made from there returns to (`syscall` is 2 bytes
/// on x86_64; `svc #0` is 4 bytes on aarch64).
#[cfg(target_arch = "x86_64")]
pub const STUB_RETURN: u64 = STUB as u64 + 2;
#[cfg(target_arch = "aarch64")]
pub const STUB_RETURN: u64 = STUB as u64 + 4;

/// Names the socket to Jato in a watched program's environment, as
/// `descriptor:inode`.
pub const SOCKET: &str = "JATO_NOTES_TO";

/// Kinds of note.
pub const OPENED: u8 = 0;
pub const ASKED: u8 = 1;

/// A note is this header and then the directory the name is relative to,
/// which is empty for a name that says everything, and the name.
///
/// ```text
/// u16 length of the whole note   u8 kind   u8 zero   u32 process
/// u16 length of the directory    the directory       the name
/// ```
pub const HEADER: usize = 10;

/// The longest note. Each is sent as one message.
pub const LONGEST: usize = 4096;
