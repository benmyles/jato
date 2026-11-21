# Jato

Jato speeds up repeat Rust builds and gives you compiler feedback on save. It remembers individual Cargo compiler results across cleans and branch switches. In a workspace, it checks the package you edited first, then checks the whole workspace.

Jato can reuse linked executables, libraries containing native archives, and crates with large embedded data. The head-to-head comparison covers ripgrep, ordinary edits, a system-zlib application and a 12 MiB asset, with first builds, warm rebuilds, storage and memory measured separately. [Benchmark report](bench/README.md)

## Get started

Build Jato on Linux with Rust 1.89 or later and a C toolchain. SQLite and Zstandard are bundled. Install your distribution's `strace` package to enable verified native link caching on x86-64 GNU Linux. From this repository, run

```sh
cargo install --locked --path .
```

From a Rust project, use the same arguments you would give Cargo.

```sh
jato check --all-targets
jato build --release
jato test -p parser -- --nocapture
jato watch
jato stats
```

The first compilation populates the cache. Cargo continues to skip fresh units and maintains its incremental data in the usual target directory. A restored unit can save an entire compiler invocation. An edit that produces new inputs still needs compilation.

If `cc` on your PATH is a compiler launcher, choose the system C driver for a project that supports it. The option applies to this Cargo process.

```sh
jato --system-linker build --release
```

## Work on save

| Command | Behavior |
| --- | --- |
| `jato watch` | Run an initial workspace check. After a member edit, check its package and then the workspace, including dependents and unified features. |
| `jato watch --focus-only` | Give feedback on the edited members. The initial run and shared configuration changes still check the workspace. |
| `jato watch -- test --workspace` | Run these Cargo arguments on every save. |
| `jato watch --watch ../schemas -- check --all-targets` | Watch an additional file or directory for changes. |
| `jato watch --debounce 40` | Use a fixed 40 ms quiet window. By default, an idle watcher can start immediately on a completed single Rust-file save. Other small edits get 16 ms and bursts or configuration changes get 100 ms. |

The watcher uses native filesystem events and Cargo metadata. It subscribes to workspace directories, member directories, and direct local path dependencies. Use `--watch` for other inputs such as an external generator's templates. It honors directory ignore rules and excludes Cargo outputs and Jato's cache. New saves supersede an active job, including its compiler and test processes. Ctrl-C stops the watcher and its active job. Set `JATO_LOG=debug` to see which paths scheduled a check.

On Linux a write-close event can complete a pending edit. A rename into place can also complete a save. The watcher times feedback through the first rustc error and through the successful workspace check after a fix. [Watch measurements](bench/README.md#save-to-compiler-feedback)

## Use with existing tools

`jato cargo` accepts arbitrary Cargo arguments, including toolchain selectors, aliases, and installed subcommands.

```sh
jato cargo +stable check --workspace
jato cargo nextest run
```

To use the cache with Cargo directly or with tools that invoke Cargo, set the wrapper for that process.

```sh
RUSTC_WRAPPER="$(command -v jato)" cargo build --release
```

For persistent use in a project, add the following to its `.cargo/config.toml` and put Jato on `PATH`.

```toml
[build]
rustc-wrapper = "jato"
```

Jato's Cargo commands select Jato as `RUSTC_WRAPPER` for the child process. Cargo can still invoke a nested workspace wrapper such as Clippy. Cargo runs build scripts and test programs as usual. Eligible compiler and linker outputs can be restored together.

## Storage and diagnostics

| Setting or command | Purpose |
| --- | --- |
| `JATO_CACHE_DIR` | Private directory on a local filesystem with locking. Defaults to `$XDG_CACHE_HOME/jato`, or `~/.cache/jato`. |
| `JATO_MAX_SIZE=512MiB` | Set the budget for packed payloads, the full length of sealed files and record overhead. The default is 1 GiB. |
| `JATO_STORAGE=packed` | Store all new artifacts in deduplicated, compressed chunks. The default `auto` can also share file extents on compressed Btrfs. |
| `jato stats --json` | Show this cache's logical artifact bytes, sealed and compressed payload, SQLite file sizes, compiler hits, probe hits, pending entries and bypasses. |
| `jato gc 256MiB` | Immediately evict least-recently-used entries to this size. `jato gc` uses the configured budget. `jato gc 0` removes all managed entries. |
| `JATO_LOG=debug jato check` | Print per-crate decisions, including unavailable inputs and unsupported compiler options, when Cargo invokes rustc. |
| `JATO_LOG=timings jato build --release` | Show per-hit validation and restore times in microseconds. |
| `JATO_LINK_CACHE=0 jato build` | Use the original toolchain for final links and units requiring native input observation. |
| `JATO_DISABLE=1 jato build` | Delegate every invocation directly to the compiler for this run. |

The build cache has no daemon. Packed storage streams artifacts in chunks of up to 256 KiB. It aligns chunks to large Rust archive members and to metadata that also exists in a separate `.rmeta` file. Identical content shares storage across files and revisions. Repeated blocks within a compiler invocation are compressed once. On compressed Btrfs with fs-verity and reflinks, automatic mode can seal files of 256 KiB or larger and restore independently writable clones. It uses packed storage for smaller files, cross-device writes, unsupported filesystems or limited headroom.

SQLite publishes each complete artifact set in a transaction. Hits verify the reconstructed file digest or the kernel's fs-verity measurement before publishing files and replaying diagnostics. Storage failures fall back to compilation. Database pages, the write-ahead log, temporary staging and Cargo's target directory have additional filesystem costs. The cache contains local artifacts and diagnostics, which may include values embedded by `env!`.

The compiler counters cover invocations that reach the wrapper after Cargo's freshness check. `reused_compiler_ms` sums earlier compiler durations, which can overlap during parallel builds. `probe_hit` counts replayed compiler version and target queries separately.

Jato 0.3 migrates earlier caches in place. Compatible Rust records remain available. Native recipes populate under the new verification key. New variants share checked input manifests, and `jato gc` reclaims their storage after the last reference is evicted.

## What can be reused

Jato caches stable rustc metadata checks and eligible Rust libraries. Keys include the working directory, compiler arguments, toolchain and sysroot identities, compiler environment, source and included-file contents, rustc's tracked environment inputs, and dependency artifact hashes. A new source edit can go directly to rustc before Jato reads its compiled dependencies. A hit requires the complete key. Changes to module search directories trigger fresh resolution.

For large inputs Jato can borrow up to three immediately available job slots from Cargo. It streams independent files or aligned BLAKE3 subtrees in parallel and falls back to a single reader when the slots are occupied. Large binary assets can be verified with bounded memory. Oversized UTF-8 inputs that need a complete assembly or native-link syntax scan take the compiler path.

On x86-64 GNU Linux, Jato can also reuse binaries, test executables and compiled procedural-macro libraries. On a miss it observes the original compiler and stock linker with `/usr/bin/strace`. It records executable identities, files read, directory searches and failed lookups. Replacing an archive, adding a higher-priority library or changing the linker can invalidate the result. A restricted tracer, custom linker launcher, unsupported flags or unverified inputs send the work to the original toolchain.

This also covers Rust libraries using a native-only search path or an ordinary `-l` static or dynamic library declaration. When rustc bundles a static archive into an rlib, Jato validates the archive it actually opened and the search paths it tried. Cargo continues to run build scripts, including any C compilation they request.

Cargo's default release profile can reuse eligible native outputs. Its default dev profile can also reuse an executable after a clean. Jato only records an incremental native build when it observes rustc creating fresh state for that crate. Once a live incremental session exists, rustc handles edits directly. Jato leaves the incremental setting under Cargo's control.

Cargo versions with a shared `target/*/deps` directory can have concurrent jobs changing a directory as the linker searches it. Jato requires the observed search to validate and lets rustc finish such a link when the directory has changed.

Jato preserves absolute paths, including output paths. Reuse works with a stable checkout and target location across cleans, edits, and branch switches. Separate worktrees and relocated target directories generally produce separate entries. The artifact store still deduplicates identical chunks.

Procedural-macro expansion can inspect arbitrary external state. A crate using such a macro, including through a re-export, takes the compiler path. Inline assembly and opaque code-generation inputs also take that path. Metadata-only results can certify other checks. Code generation requires its own evidence. Cargo can pipeline work normally, and Jato waits for matching producer evidence before serving a dependent entry. An existing incremental directory remains available. After a clean, rustc starts rebuilding incremental state on the next miss.

## Design references

- Cargo's freshness checks and rustc's incremental compiler already shorten ordinary edits. Jato keeps them enabled and spends its storage budget on reusable compiler outputs. [Cargo build cache](https://doc.rust-lang.org/cargo/reference/build-cache.html)
- Cargo's jobserver controls available parallelism. Jato uses nonblocking token acquisition for extra readers and BLAKE3's subtree interface to produce the same digest at every job count. [Jobserver client](https://docs.rs/jobserver/0.1.35/jobserver/struct.Client.html#method.try_acquire) · [BLAKE3 subtree hashing](https://docs.rs/blake3/1.8.7/blake3/hazmat/index.html)
- sccache's Rust notes identify system linking and files read by procedural macros as difficult cases. Jato tests archive replacement, previously absent search candidates and untracked macro reads through real Cargo builds. [sccache 0.19 Rust support](https://github.com/mozilla/sccache/blob/v0.19.0/docs/Rust.md) · [rustc search paths](https://doc.rust-lang.org/rustc/command-line-arguments.html#-l-add-a-directory-to-the-library-search-path)
- kache already shares output blocks and reuses eligible executables. We measure it directly with clean targets, an existing target and a live edit. Jato uses independently writable restores and a packed mode for machines with small caches. [kache](https://github.com/kunobi-ninja/kache) · [ccache storage options](https://ccache.dev/manual/latest.html) · [Head-to-head workloads](bench/README.md)
- CI archives and container layers reuse builds at a different granularity. Jato records individual successful units, so a feature change in one crate can leave many other entries usable. [rust-cache](https://github.com/Swatinem/rust-cache) · [cargo-chef](https://github.com/LukeMathWalker/cargo-chef)
- Watch tools make checks continuous. Jato adds package-first feedback, a workspace follow-up, and cancellation to its compiler cache. Cargo keeps control of linkers and test runners. [Bacon](https://dystroy.org/bacon/) · [Cargo Watch](https://github.com/watchexec/cargo-watch) · [cargo-nextest](https://nexte.st/docs/design/how-it-works/) · [mold](https://github.com/rui314/mold)
- Linux fs-verity authenticates the contents of an immutable cache inode. Reflinks let each restored target share extents until a writer changes its own file. [Kernel fs-verity documentation](https://docs.kernel.org/filesystems/fsverity.html) · [FICLONE](https://man7.org/linux/man-pages/man2/ioctl_ficlonerange.2.html)

## Development

Run `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, and `cargo test --locked`. The integration suite builds disposable Cargo projects and exercises compiler invalidation, exact artifact restoration, process cancellation, and concurrent cache access. Project conventions, the release checklist, and the work journal live alongside the code. [AGENTS.md](AGENTS.md) · [TODO.md](TODO.md) · [JOURNAL.md](JOURNAL.md)
