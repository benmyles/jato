# Jato

Jato speeds up repeat Rust builds and gives you compiler feedback on save. It remembers individual Cargo compiler results across cleans and branch switches. In a workspace, it checks the package you edited first, then checks the whole workspace.

Jato 0.8 reduces the cost of changing a dependency and retaining large artifact revisions. Across eight builds that inserted bytes into a 16 MiB embedded asset, automatic storage retained **49.3 MiB** of files, compared with **385.1 MiB** for Jato 0.7, **384.2 MiB** for kache and **384.1 MiB** for sccache. In a four-consumer Cargo workspace, a compiler error after a dependency edit reached Cargo in a median **307 ms**, down from **427 ms** with 0.7. The same check took 494 ms with kache and 736 ms with sccache. [Measurements, controls and reproduction commands](bench/README.md)

## Get started

Build Jato on Linux with Rust 1.89 or later and a C toolchain. SQLite and Zstandard are bundled. Install your distribution's `strace` package to enable verified native link caching on x86-64 GNU Linux. From this repository, run

```sh
cargo install --locked --path .
```

From a Rust project, use the same arguments you would give Cargo.

```sh
jato dev run
jato check --all-targets
jato build --release
jato test -p parser -- --nocapture
jato watch
jato stats
```

The first compilation populates the cache. Cargo continues to skip fresh units and maintains its incremental data in the usual target directory. A restored unit can save an entire compiler invocation. An edit that produces new inputs still needs compilation.

### Use Jato with coding agents

From the Jato checkout, install the [Jato skill](skills/jato/SKILL.md) for every project in your account. One command installs it for all three clients and requires only Python 3.

```sh
./scripts/install-skill
```

Open the agent in your Rust project and ask it to use Jato, or invoke the skill explicitly in a new session.

| Client | Example prompt | Default skill directory |
| --- | --- | --- |
| Claude Code | `/jato run the workspace tests` | `~/.claude/skills/jato/` |
| Codex | `$jato run the workspace tests` | `~/.agents/skills/jato/` |
| Antigravity CLI (`agy`) | `/jato run the workspace tests` | `~/.gemini/antigravity-cli/skills/jato/` |

The installer honors `CLAUDE_CONFIG_DIR`. Each copy works after moving the checkout. A repeat installation preserves matching files. To update copies that differ, review them and rerun `./scripts/install-skill --force`. In a running agy session, `/skills reload` refreshes the commands. [Claude Code skills](https://code.claude.com/docs/en/skills) · [Codex skills](https://developers.openai.com/codex/skills) · [Antigravity skills](https://antigravity.google/docs/skills)

### Fast development builds

Use `jato dev` to build, `jato dev run` to start the application, or `jato dev test` to run its tests. `jato dev watch` uses the same profile for focused and workspace checks. For separate checkouts, use `jato dev worktree --root . run`.

These commands select the Cargo profile `jato-dev`, which inherits the project’s `dev` profile. It uses 16 codegen units, emits line tables for workspace members, and requests no debug information for dependencies outside the workspace. Workspace backtraces retain source locations. Inspecting local variables in a debugger calls for a profile with full debug information, such as an ordinary `jato build` with Cargo’s defaults. Cargo writes the new profile to `target/jato-dev` and keeps its own incremental state there. Keeping both profiles retains both sets of Cargo outputs. Use `jato dev cargo clean` to clear the fast profile. Explicit `--profile` and `--release` options take precedence. [Cargo profiles and package overrides](https://doc.rust-lang.org/cargo/reference/profiles.html)

If `cc` on your PATH is a compiler launcher, choose the system C driver for a project that supports it. The option applies to this Cargo process.

```sh
jato --system-linker build --release
```

## Reuse builds across worktrees

On Linux, install the distribution’s `bubblewrap` package and use the same private `JATO_CACHE_DIR` across builds. For registry dependencies shared by different projects, run `jato worktree build --release`. Source paths keep their usual meaning. For multiple copies or Git worktrees of the same workspace, run the following commands from each checkout.

```sh
jato worktree --root . build --release
jato worktree --root . watch
jato worktree --root . paths
jato worktree --root . cargo clean
```

Each invocation starts Cargo in a private mount namespace with the invoking user’s normal filesystem access. Cargo sees the same target and build paths across runs. With `--root`, it also sees the source at a stable path. Choose a root containing the workspace and any inputs reached via relative paths. Jato checks ancestor configuration before moving the source. The cache directory must be outside the projected source. The host must permit bubblewrap to create its mount namespace. [Bubblewrap](https://github.com/containers/bubblewrap) · [Cargo configuration discovery](https://doc.rust-lang.org/cargo/reference/config.html)

Writable outputs live in a checkout-specific `.jato-worktree` subdirectory under Cargo’s configured target and build directories. This keeps Cargo’s freshness and incremental state independent, including when multiple checkouts share a global `CARGO_TARGET_DIR`. Using both ordinary and projected builds retains both sets of Cargo outputs until cleaned. `paths` prints the physical directories as JSON and accepts the same `--target-dir`, `--manifest-path`, and `--config` options as the build. `jato worktree … cargo clean` clears that view’s build outputs. Ordinary `cargo clean` can clear the entire configured target directory.

Compiler diagnostics, Cargo JSON, debug information and path-valued macros such as `env!("CARGO_MANIFEST_DIR")` and `env!("OUT_DIR")` can contain view paths. Use `jato worktree --root . run` or `jato worktree --root . exec PROGRAM [ARGS...]` when a program or debugger needs to open those paths. Programs that need the host checkout path embedded in a shipped binary can use ordinary `jato build`. Separate caches, toolchains, features and differing inputs still affect reuse.

On Linux, simultaneous cache misses with matching compiler settings can share one compilation. This also works for ordinary Cargo wrappers whose inputs and output paths align. The owner holds a short-lived local socket while compiling. A peer checks its own source files, tracked environment, dependency artifacts and native inputs before restoring verified outputs into its own target. A failed compiler or an ineligible result sends the peer through its own compiler. Warm hits proceed independently.

Jato uses known input fingerprints to distinguish edits when a manifest exists. On a first compile, the crate root’s first 4 KiB helps group likely matches. A peer waits up to 60 seconds, and proceeds independently if coordination is unavailable. The processes must share a local socket namespace and private cache to coordinate. `JATO_COALESCE=0` disables waiting for that invocation. [Linux abstract socket lifetime](https://man7.org/linux/man-pages/man7/unix.7.html)

## Work on save

| Command | Behavior |
| --- | --- |
| `jato watch` | Run an initial workspace check. After a member edit, check its package and then the workspace, including dependents and unified features. |
| `jato watch --focus-only` | Give feedback on the edited members. The initial run and shared configuration changes still check the workspace. |
| `jato watch -- test --workspace` | Run these Cargo arguments on every save. |
| `jato watch --watch ../schemas -- check --all-targets` | Watch an additional file or directory for changes. |
| `jato watch --debounce 40` | Use a fixed 40 ms quiet window. By default, an idle watcher can start immediately on a completed single Rust-file save. Other small edits get 16 ms and bursts or configuration changes get 100 ms. |

The watcher uses native filesystem events and Cargo metadata. It subscribes to workspace directories, member directories, and direct local path dependencies. Use `--watch` for other inputs such as an external generator's templates. It honors directory ignore rules and excludes Cargo outputs and Jato's cache. New saves supersede an active job, including its compiler and test processes. Ctrl-C stops the watcher and its active job. Set `JATO_LOG=debug` to see which paths scheduled a check.

On Linux the watcher polls inotify, Cargo completion and cancellation in the same loop. A completed save can start a check immediately. New directories acquire watches in place when Cargo’s workspace roots stay the same. A configuration change, moved watched directory or event overflow triggers a rescan. Ignore rules apply to directories moved into the workspace. [Watch measurements](bench/README.md#save-to-compiler-feedback)

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
| `JATO_MAX_SIZE=512MiB` | Set the budget for packed frames, shared sealed bases, revision patches and record overhead. The default is 1 GiB. |
| `JATO_COALESCE=0` | Run concurrent misses independently. Each build still uses the cache. |
| `JATO_STORAGE=packed` | Store all new artifacts in deduplicated, compressed chunks. The default `auto` can also share file extents on compressed Btrfs. |
| `jato stats --json` | Show logical artifact bytes, packed and sealed payloads, patch sizes, SQLite file sizes, compiler hits and bypasses. |
| `jato gc 256MiB` | Immediately evict least-recently-used entries to this size. `jato gc` uses the configured budget. `jato gc 0` removes all managed entries. |
| `JATO_LOG=debug jato check` | Print per-crate decisions, including unavailable inputs and unsupported compiler options, when Cargo invokes rustc. |
| `JATO_LOG=timings jato build --release` | Show per-hit validation and restore times, and the compile, validation and storage costs of a recorded miss, in microseconds. |
| `JATO_LINK_CACHE=0 jato build` | Use the original toolchain for final links and units requiring native input observation. |
| `JATO_DISABLE=1 jato build` | Delegate every invocation directly to the compiler for this run. |

The build cache has no daemon. Packed storage streams artifacts through a 256 KiB input window. Rust archive member and metadata boundaries preserve sharing with separate `.rmeta` files. Within long regions, FastCDC chooses boundaries from the data so inserts and deletions can reuse the surrounding bytes. Repeated chunks within a compiler invocation are compressed once. On compressed Btrfs with fs-verity and reflinks, automatic mode can seal files of 256 KiB or larger and restore independently writable clones. It uses packed storage for smaller files, cross-device writes, unsupported filesystems or limited headroom. [FastCDC research](https://www.usenix.org/conference/atc16/technical-sessions/presentation/xia) · [Rust implementation](https://github.com/nlfiedler/fastcdc-rs)

SQLite publishes each complete artifact set in a transaction. Hits verify the reconstructed file digest or the kernel's fs-verity measurement before publishing files and replaying diagnostics. Storage failures fall back to compilation. Database pages, the write-ahead log, temporary staging and Cargo's target directory have additional filesystem costs. The cache contains local artifacts and diagnostics, which may include values embedded by `env!`.

The compiler counters cover invocations that reach the wrapper after Cargo's freshness check. A `coalesced_hit` is a verified hit after observing another compiler on the same scheduling hint. A `live_incremental_hit` restores an eligible native output during an existing incremental session. Both count toward `hit`. `reused_compiler_ms` sums earlier compiler durations, which can overlap during parallel builds. `probe_hit` counts replayed compiler version and target queries separately.

Packed writes hash frames before compression and can reuse previously compressed frames across revisions. `packed_reused_bytes` in `jato stats --json` counts raw bytes whose compression was skipped by successful publications. Ordered frame descriptors share the bounded staging spool with compressed payloads. For a new incompressible frame of at least 16 KiB, the descriptor points back to the compiler output. Publication rereads and hashes those exact bytes. Frames whose encoded payload is at least 64 KiB go into sharded files under `frames-v1`. Smaller frames stay in SQLite. Large payloads bypass the database’s write-ahead log and checkpoint writes. The database publishes their references together and reclaims a frame when its final user is evicted. Long zero runs in a restored file can remain sparse. Distinct digest sets beyond 1,024 entries spill to a private temporary SQLite index. [SQLite temporary databases](https://www.sqlite.org/inmemorydb.html#temporary_databases) · [Write-ahead logging](https://www.sqlite.org/wal.html)

When a compiler emits an identical file, Jato can reuse a sealed object after checking its full digest and kernel measurement. The `sealed_reused_bytes` counter records bytes matched this way. For outputs of at least 4 MiB, Jato also looks for a sealed base from the same compiler recipe and output slot with a length within 64 KiB. A small overwrite, insertion or deletion can be recorded as changed bytes and references to matching ranges. Each patch refers directly to a complete base and is capped at 128 KiB. Evicting the original build keeps the base alive for its remaining users.

For a small in-place change, Jato can clone the authenticated base, rehash each modified region, and recompute the standard BLAKE3 root for the entire result. When much of a file shifted, it can reconstruct and hash the bytes in one pass. All paths check the complete result before publication. `patched_files` and `patch_bytes` report the number and size of revision patches. `frame_file_bytes` is the file-backed portion of `compressed_bytes`.

Jato 0.8 upgrades earlier local caches in place. To move a cache, copy its entire directory when builds are idle so the SQLite files, frames and sealed objects travel together. Old fixed-size chunks and new content-defined chunks can coexist. Reusing a chunk leaves its compressed payload untouched. Worktree builds populate their own compiler recipes. New variants share checked input manifests, and `jato gc` reclaims their storage after the last reference is evicted.

## What can be reused

Jato caches stable rustc metadata checks and eligible Rust libraries. Keys include the working directory, compiler arguments, toolchain and sysroot identities, compiler environment, source and included-file contents, rustc's tracked environment inputs, and dependency artifact hashes. A new source edit can go directly to rustc before Jato reads its compiled dependencies. A hit requires the complete key. Changes to module search directories trigger fresh resolution.

When rustc tracks a source input or direct compiled dependency of at least 4 MiB, Jato can rule out a new variant using bounded samples. This lets a consumer skip an unsuccessful full read of a large rmeta or rlib when its producer changes. A matching sample proceeds through whole-file validation. For large inputs Jato can borrow up to three immediately available job slots from Cargo. It streams independent files or aligned BLAKE3 subtrees in parallel and falls back to a single reader when the slots are occupied. Oversized UTF-8 inputs that need a complete assembly or native-link syntax scan take the compiler path.

On x86-64 GNU Linux, Jato can also reuse binaries, test executables and compiled procedural-macro libraries. On a miss an indexed artifact-size check can rule out an unverified dependency before tracing. Publication requires full content digests and a match to certified producer artifacts using the input identities observed before compilation. Jato observes the original compiler and stock linker with `/usr/bin/strace`. It records executable identities, files read, directory searches and failed lookups. Replacing an archive, adding a higher-priority library or changing the linker can invalidate the result. A restricted tracer, custom linker launcher, unsupported flags or unverified inputs send the work to the original toolchain.

This also covers Rust libraries using a native-only search path or an ordinary `-l` static or dynamic library declaration. When rustc bundles a static archive into an rlib, Jato validates the archive it actually opened and the search paths it tried. Cargo continues to run build scripts, including any C compilation they request.

Cargo's default release and dev profiles can reuse eligible native outputs. Jato records an incremental native build when it observes rustc creating fresh state for that crate. Reverting an edit can restore that executable even while the incremental session is live. Jato leaves the session in place for rustc to compile the next new edit. For small source directories it also records the names and types of directory entries and the identities of untracked entries. An editor can replace a tracked file by rename and still restore its old executable after full content verification. A different layout, symlinks or a directory set beyond the bounded scan can require fresh rustc module resolution. Cargo controls the incremental setting.

Cargo versions with a shared `target/*/deps` directory can have concurrent jobs changing a directory as the linker searches it. Jato requires the observed search to validate and lets rustc finish such a link when the directory has changed.

Ordinary builds preserve absolute compiler paths, including output paths. A different checkout or target location can therefore need a separate entry. Worktree mode gives Cargo repeatable paths before compilation. When a copied checkout has new directory identities, Jato can ask rustc to resolve its inputs again and revalidate the recorded native inputs before restoring an executable. An entry can remember the original directory layout and three additional layouts after successful checks. The extra layouts cost 192 charged bytes per entry and are added when the cache budget permits.

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

Run `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, and `cargo test --locked`. The integration suite builds disposable Cargo projects and exercises compiler invalidation, exact artifact restoration, process cancellation, and concurrent cache access. Run `python3 tests/install_skill.py` to check the global skill installer in temporary user homes. Project conventions, the release checklist, and the work journal live alongside the code. [AGENTS.md](AGENTS.md) · [TODO.md](TODO.md) · [JOURNAL.md](JOURNAL.md)
