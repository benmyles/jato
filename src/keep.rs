//! Cargo owns intermediate state and publishes to the requested target. On
//! Btrfs a subvolume boundary gives each public target independently writable
//! copies while allowing the kernel to share their data extents.
use std::env;
use std::process::Command;

pub fn configure(cmd: &mut Command) {
    if env::var_os("CARGO_BUILD_BUILD_DIR").is_some() {
        return;
    }
    #[cfg(target_os = "linux")]
    prepare_subvolume();
    cmd.env(
        "CARGO_BUILD_BUILD_DIR",
        "{cargo-cache-home}/jato-build/v1/{workspace-path-hash}",
    );
}

#[cfg(target_os = "linux")]
fn prepare_subvolume() {
    use std::fs::{self, File};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::DirBuilderExt;
    use std::path::PathBuf;

    let home = env::var_os("CARGO_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| env::home_dir().map(|p| p.join(".cargo")));
    let Some(home) = home.filter(|p| p.is_absolute()) else {
        return;
    };
    let base = home.join("jato-build");
    let root = base.join("v1");
    if fs::symlink_metadata(root).is_ok() {
        return;
    }
    if fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&base)
        .is_err()
    {
        return;
    }
    let Ok(parent) = File::open(base) else { return };
    // linux/btrfs.h: btrfs_ioctl_vol_args is a signed 64-bit fd followed by
    // 4,088 bytes for the NUL-terminated subvolume name. CREATE is atomic
    // and cannot replace an existing entry, including a concurrent creation.
    #[repr(C)]
    struct VolArgs {
        fd: i64,
        name: [u8; 4088],
    }
    let mut args = VolArgs {
        fd: 0,
        name: [0; 4088],
    };
    args.name[..2].copy_from_slice(b"v1");
    let request = libc::_IOW::<VolArgs>(0x94, 14);
    // Unsupported filesystems leave directory creation and layout to Cargo.
    unsafe {
        libc::ioctl(parent.as_raw_fd(), request as _, &args);
    }
}
