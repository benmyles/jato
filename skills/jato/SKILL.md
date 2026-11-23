---
name: jato
description: Use Jato in Rust Cargo projects to restore builds that were made before (in this checkout, another worktree or after a clean), keep several builds from overloading the machine, check edits on save, or find out why something was compiled again. Use when setting up Jato or when building, testing or running a Cargo project where Jato is available.
---

# Use Jato in a Cargo project

Start in the directory where the project runs Cargo. Check `jato --version` when first setting up. To install Jato on Linux, run `cargo install --locked --path /path/to/jato` with the path to the **Jato source checkout**. Building it requires Rust 1.89+ and a C toolchain. If Jato is unavailable and the task is to check or test code, run the project's original Cargo command.

## Run the project's command

Keep the project's toolchain, features, target, profile, package selections, lockfile flags and the arguments after `--`. Replace the leading `cargo` with `jato` for build, check, clippy, test, bench and run, or with `jato cargo` for anything else, including a `+toolchain` selector.

| Project command | Jato invocation |
| --- | --- |
| `cargo check --workspace --locked` | `jato check --workspace --locked` |
| `cargo test -p parser parses_numbers -- --nocapture` | `jato test -p parser parses_numbers -- --nocapture` |
| `cargo clippy --all-targets -- -D warnings` | `jato clippy --all-targets -- -D warnings` |
| `cargo +stable build --release` | `jato cargo +stable build --release` |

Run through `jato`, a build that was made before with the same sources, settings and toolchain is put back whole without starting Cargo: in a new worktree, in another target directory, or after `cargo clean`. A build that has to be made is recorded as it runs. Use the same command each time; `jato build -p a` and `jato build -p b` are different builds.

For a tool that starts Cargo itself, prefix its command with `RUSTC_WRAPPER="$(command -v jato)"`. Single compilations are then reused, which is slower for a large restored build than `jato build` but needs no other change. If the task calls for persistent project integration, merge `rustc-wrapper = "jato"` into `[build]` in `.cargo/config.toml`. Check the role of any existing wrapper before replacing it. Every developer and CI runner using that config needs `jato` on `PATH`.

## What to expect

- A project's first build with Jato has to be made in an empty target directory to be of use anywhere else: run `cargo clean` first, or build in a new worktree. In a target directory that Cargo or another cache filled, Cargo finds everything up to date and Jato has nothing to keep; `jato stats` then says that the last build is only put back where it was made.
- The first build of a state compiles and records. Later builds of that state, anywhere on the machine that shares the cache, are restored: in another git worktree or clone, on another branch, after a commit that left the built files alone.
- In a new worktree, build with `jato` before or after editing, as suits the task. A first build of sources that differ from ones built before only in Rust source starts from that earlier build: Cargo compiles what changed and what depends on it. `jato stats` counts these as `build_near`. A changed manifest, lock file or build script input has Cargo carry out the whole build, with each compiler answered from the cache.
- After editing, Cargo and rustc compile what changed at their usual speed. Undoing an edit returns to a recorded build at once.
- In a target that was restored, the first edit of a crate is compiled incrementally all the same: Jato hands rustc the state it saved where the crate was last compiled under watch. `jato stats` counts this as `state_given`.
- C, C++ and assembly that build scripts compile are remembered like Rust, through the names `cc`, `gcc`, `clang`, `nasm` and their relatives at the head of `PATH` for the Cargo commands Jato runs. `jato stats` counts them as `tool_hit` and `tool_miss`. `JATO_TOOLS=0` turns this off.
- A program that embeds where it was built, with `env!("CARGO_MANIFEST_DIR")` for one, is compiled again in each checkout. The rest of the build is put back around it.
- A `build` after a `check`, or the reverse, takes the C and assembly that build scripts compiled from the other.
- A checkout with a different crate beside it (`path = "../shared"`), or with Cargo settings in a directory above it, gets a build of its own.
- Builds started at the same time through Jato take turns at the processors. A build that seems to wait is queued behind others; `jato stats` shows `queued`.
- Jato records on Linux (x86-64 and aarch64, kernel 5.5 or later). Elsewhere its commands run Cargo unchanged.
- Programs in a recorded build run with `LD_PRELOAD` naming a small library of Jato's, through which they say which files they read. If a program misbehaves under it, `JATO_NOTES=0` leaves it out.
- With every processor in use, compilations in a target that is already built are not recorded (`continued` in the stats). A build made from nothing always is.

## On a build server

- Keep the cache and the checkouts on a filesystem that clones files, Btrfs or XFS: a build is then put back without copying anything, and target directories share their room with the cache. On ext4 every file is copied: 21 GB and 3 to 4 s for a debug build of a large workspace, against 1.3 s and no room. `jato stats` says which the cache directory is on.
- Free memory already serves as the cache of the disk. Keeping the cache or a target directory in memory by hand makes nothing quicker on a disk that is fast.

## Choose an edit loop

- For a quick local executable or test, use `jato dev run` or `jato dev test`. These select a separate `jato-dev` profile with workspace line tables and its own target outputs. Use the project's normal profile for CI parity or to inspect local variables in a debugger.
- For feedback on save, run `jato watch`. It checks the workspace, then after each save the edited package first and the workspace after it. To run package tests on every save, use `jato watch -- test -p parser`. Stop the watcher you started with Ctrl-C when the session is done.

## Explain cache behavior

Compare `jato stats --json` before and after a command: `build_hit` counts builds put back whole, `hit` single compilations reused, `miss` compilations run and recorded, `continued` compilations left to rustc during a run of edits, `uncacheable` those that could not be stored. `JATO_LOG=debug jato build` says for each command whether it was reused and, if not, which file, variable or setting had changed, and for a build that was not recorded which program did what. Give `JATO_LOG` an absolute path to collect this in a file.

When a command was compiled that should have been found, run both builds with `JATO_RECIPES=/some/dir/a` and `JATO_RECIPES=/some/dir/b` (two empty directories) and compare them: each file says what one command is known by, a line for each thing, and the lines that differ are why.

A procedural macro that reads an environment variable needs it named: `JATO_KEY_ENV=NAME,OTHER`. Files and directories a macro or build script reads are seen without help. A compilation that uses the network is never stored.

To inspect the cache budget, run `jato limit`; to set it for every project using that cache, `jato limit 32GiB`. `jato gc 10GiB` evicts least-recently-used results down to a size.
