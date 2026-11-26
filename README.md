# Jato

**Fast, syscall-observed build and test caching for Cargo on Linux.**

Jato remembers whole Cargo builds, individual `rustc` / C / C++ / assembly compilations, build scripts, `rustc` incremental state, and passing test runs — without replacing Cargo, without a background daemon, and without guessing dependencies from compiler flags.

- **Instant whole-build restores**: On a hit, Cargo never starts. Clean targets, new git worktrees, and branch switches restore in under a second (or ~0.7 s for a 21 GiB workspace with `jato memory`).
- **Near-build deltas & cross-command seeding**: Starting from an edited commit, a `Cargo.toml` change, or a new command (`jato test`, `jato clippy`, `jato build` after `jato check`) reuses the closest cached build and only compiles what changed.
- **Soundness by observation**: Uses Linux `seccomp_unotify` + a minimal read-note preload shim to observe every file, directory, symlink, and subprocess actually read — catching procedural macros, build scripts, linkers, and C headers automatically.
- **Incremental `rustc` state across checkouts**: Saves and relocates `rustc` incremental state so your *first* edit in a brand-new worktree compiles incrementally instead of from scratch.
- **Cached build scripts, C/C++, and parallel tests**: Caches hermetic `build.rs` executions (`OUT_DIR` + directives), C/C++/asm compilations started by build scripts, and passing hermetic test binaries, while running workspace test binaries in parallel.
- **Built for multi-worktree & coding agents**: Zero idle daemon RAM, shared machine-wide jobserver FIFO so parallel builds don't thrash cores, Btrfs/XFS reflinks, optional RAM hot tier on ext4, a zero-dependency `file://` / `http://` remote cache, and a shared `jato rust-analyzer` LSP multiplexer & transparent shim so dozens of agent sessions across worktrees share one `rust-analyzer` per repo instead of consuming 100+ GiB RAM.

> **Platform**: Linux (`x86_64` and `aarch64`, kernel 5.5+). On macOS and Windows, `jato` passes commands straight through to Cargo.

---

## Install & quick start

Requires Rust 1.89+ and a C toolchain (SQLite, Zstandard, and the preload shims are bundled).

```sh
cargo install --locked --git https://github.com/benmyles/jato
# or from a local checkout:
cargo install --locked --path .
```

Run `jato` anywhere you would run `cargo`:

```sh
jato build --release
jato check --all-targets
jato clippy -- -D warnings
jato test -p parser -- --nocapture
jato run
jato cargo +nightly build   # any other Cargo subcommand
jato stats                  # hit rates, cache size, remote stats
jato why                    # explain why the last build missed or stayed local
```

> **Tip**: Make a project's first Jato build in an empty `target/` (after `cargo clean` or in a fresh worktree). If Cargo finds everything already up to date, Jato only records the build for that local directory.

### Coding agents (Claude Code, Codex, Antigravity)

Coding agents working in separate git worktrees share one cache and one CPU budget instead of duplicating gigabytes of `target/` data or oversubscribing cores. Install the bundled [Jato skill](skills/jato/SKILL.md) for all three clients:

```sh
./scripts/install-skill     # pass --force to overwrite modified copies
```

Installs to `~/.claude/skills/jato/` (honors `CLAUDE_CONFIG_DIR`), `~/.agents/skills/jato/`, and `~/.gemini/antigravity-cli/skills/jato/`.

### Shared `rust-analyzer` across sessions & worktrees

Running many coding-agent sessions or editors across worktrees normally spawns one 4–6 GiB `rust-analyzer` process per session (>100 GiB across 20+ worktrees). `jato rust-analyzer` multiplexes LSP clients into one shared, tuned `rust-analyzer` backend per repository (sharing `sysroot`, dependencies, proc-macro server, and a per-repo `cargo.targetDir` across sibling git worktrees), starts lazily (`0 B` until the first `.rs` file open or query), filters worktree URIs per client, hibernates idle backends transparently (auto-waking with unsaved buffers intact), and enforces a total RSS budget with LRU eviction:

```sh
jato rust-analyzer shim install    # transparently intercept `rust-analyzer` on PATH
jato rust-analyzer status          # inspect shared backends, worktrees, and RSS
jato rust-analyzer stop            # stop the shared daemon and all backends
jato rust-analyzer shim uninstall  # remove shim and restore any previous rustup link
```

Non-LSP invocations (`rust-analyzer --version`, `analysis-stats`, `parse`, etc.) pass straight through to the real binary. You can also point an editor/LSP client directly at `jato rust-analyzer` without installing the shim.

### Plain Cargo / `RUSTC_WRAPPER`

For tools that invoke Cargo directly, Jato also works as a compiler wrapper (caches single compilations only; use `jato <cmd>` for whole builds, build scripts, and tests):

```sh
RUSTC_WRAPPER="$(command -v jato)" cargo build --release
```

When you run `jato <cmd>`, Jato automatically steps around `kache`, `sccache`, or `ccache` wrappers (`RUSTC_WRAPPER`, `PATH` symlinks, `CC`/`CXX`) for that command alone without changing your shell environment.

---

## Performance

Checking and building all 121 packages of **Olo** (1,502 Rust compilations, 113 build scripts compiling ~1,100 C/C++/asm files, 56 dev binaries, 20.6–20.9 GiB target, `nightly-2026-11-14`, mold) on a 32-core AMD EPYC 9655 build server (125 GiB RAM, ext4):

| Step (seconds, ext4) | `check`: Jato `0.18` *(side by side)* | `check`: kache `2.3` *(side by side)* | `build`: Jato `0.18` *(side by side)* | `build`: kache `2.3` *(side by side)* | `build`: Jato `0.19` *(solo / `mem64`)* |
| --- | ---: | ---: | ---: | ---: | ---: |
| Cold build (empty cache & target) | **153.9** | 160.6 | **480.4** | 482.3 | 256.7 / **245.7** *(Cargo: 248)* |
| Same command again (72k inputs) | **0.13** | 0.59 | **0.15** | 1.84 | **0.07** / **0.07** |
| Target removed (`cargo clean`) | **1.52** | 11.85 | **3.06** | 30.13 | 1.53 / **1.27** |
| Fresh target directory | **0.99** | 11.41 | **5.19** | 138.71 | 1.25 / **0.73** *(28.7 GB/s)* |
| Copied checkout / new worktree | **0.52** | 10.37 | **18.21** | 22.22 | 6.41 / **5.59** |
| New worktree + `Cargo.toml` edit | **3.80** | 10.72 | **10.06** | 19.59 | **7.55** / 8.13 |
| First edit in restored target | **4.38** | 7.19 | **18.24** | 35.98 | **10.62** / 11.62 |
| Subsequent edits (in-place) | 3.98 | **3.08** | 13.14 | **11.74** | 9.18 / **8.74** |
| Undo edits | **0.26** | 2.96 | 15.27 | 11.40 | **0.69** / 0.74 |
| Disk store / idle daemon RAM | **1.3 GiB / 0** | 1.8 GiB / 115 MiB | **6.2 GiB / 0** | 12.3 GiB / 115 MiB | 8.6 GiB / 21.6 GiB *(0 RAM)* |

*Conditions: `0.18` vs `kache 2.3` started simultaneously, 1 run each, load 7–67; `0.19` packed and `jato memory 64GiB` ran solo on the same server, load 15–49. `0.19` `check` solo: `fresh` 0.06 s, `restore` 0.37 s, `copy` 0.38 s, `undo` 0.18 s.*

- **Open-source workspaces (`check --workspace` in a fresh git worktree, ext4)**: `ripgrep` **0.1 s**, `rust-analyzer` **0.4 s**, `tokio` **0.4 s**, `cargo` **0.7 s**, `nushell` **2.5 s**, `ruff` **9.1 s**.
- **Cached & parallel tests (`ripgrep`, ext4)**: `jato test --workspace` in a brand-new worktree takes **1.68 s** (restored build + cached tests) vs **2.97 s** for bare `cargo test --workspace` in an already-built target.
- **Full measurements, conditions, `ripgrep` vs sccache/kache, Btrfs numbers, and reproduction scripts**: [bench/README.md](bench/README.md).

---

## How it works

1. **Syscall observation instead of flag parsing**: Every watched Cargo, `rustc`, `build.rs`, C/C++/asm, and test process runs under a Linux `seccomp_unotify` filter. To keep observation overhead near zero (~1% on cold builds), watched programs preload a tiny zero-allocation shim (`libjato_shim.so`) that sends read notes directly from a fixed allowlisted instruction address while writes, renames, unlinks, execs, and unshimmed binaries are still trapped by the kernel (`JATO_NOTES=0` routes everything through the kernel).
2. **Two-phase manifest lookup**: A command's key hashes its normalized arguments and relevant environment variables. On lookup, Jato checks the recorded input manifest (files by content with stat fast-path, symlinks, directory listings, and env vars) in parallel. If everything matches, outputs and diagnostics are restored transactionally and Cargo never runs.
3. **Whole builds, near builds, seeding, and edits**:
   - **Whole builds** (`build`, `check`, `clippy`, `test --no-run`, `bench --no-run`) restore the entire `target/` in one shot, preserving terminal/non-terminal diagnostic color rules.
   - **Near builds** start from the closest cached build when only compiler inputs, manifests/lockfiles, or specific build scripts changed: stale outputs are omitted, Cargo compiles only the delta, and obsolete outputs are pruned.
   - **Cross-command seeding** seeds an empty `target/` (e.g., `jato test -p foo` or `jato clippy` after `jato check`) from a compatible workspace build and prunes unused outputs afterward.
   - **In-place edit recording** records edited states in already-built targets once you pause editing (`JATO_EDIT_WINDOW`) and machine load allows (`JATO_BUSY`), while handing on relocated `rustc` incremental state across checkouts (using `--remap-path-prefix`, descriptor paths, or an optional unprivileged mount namespace).
4. **Build scripts, C/C++, and tests**:
   - Hermetic `build.rs` executions are cached via a separate preload shim (`libjato_script.so`) that replays `OUT_DIR` files and relocated Cargo directives.
   - C/C++/asm compilers invoked by build scripts (`cc`, `c++`, `gcc`, `g++`, `clang`, `clang++`, `nasm`, `yasm`) are transparently cached via `PATH` wrappers with `-ffile-prefix-map`.
   - `jato test` intercepts test binaries via `CARGO_TARGET_<TRIPLE>_RUNNER`, caches passing, network-free, side-effect-free test runs (including observed fixture files), and executes workspace test binaries concurrently.
5. **Storage & concurrency**:
   - Content-addressed BLAKE3 store with an SQLite index and transactional commits.
   - **Btrfs / XFS**: zero-copy reflinks (`FICLONE`) — multiple worktrees share the exact same disk blocks.
   - **ext4 / non-cloning filesystems**: Zstandard compression, hardlink deduplication for identical large binaries, atomic fresh-target renames, and an optional uncompressed hot tier (`jato memory 64GiB`) served at page-cache speed.
   - **Shared jobserver & coalescing**: File-locked compile slots + a shared machine-wide named-FIFO jobserver keep concurrent builds from oversubscribing cores; simultaneous identical compilations coalesce and run once.

---

## Comparing Jato, kache, sccache, and Bazel (Blaze)

These four tools solve overlapping problems at different layers: **kache** and **sccache** wrap individual compiler calls under Cargo; **Bazel** (the open-source counterpart to Google's **Blaze**) replaces Cargo with a declarative polyglot build graph and optional Remote Build Execution (RBE); **Jato** keeps Cargo in charge and uses syscall observation to cache whole builds, scripts, incremental state, and tests.

### Capabilities

| | Jato | kache | sccache | Bazel / Blaze |
| --- | --- | --- | --- | --- |
| **Build system** | Keeps Cargo (`jato <cmd>` or `RUSTC_WRAPPER`) | Keeps Cargo (`RUSTC_WRAPPER` + C/C++/CUDA) | Keeps Cargo (`RUSTC_WRAPPER` + C/C++/CUDA) | Replaces Cargo (`BUILD.bazel` + `rules_rust`) |
| **Input discovery** | Runtime Linux `seccomp_unotify` + read-note shim | Parsed compiler flags & compiler-reported inputs | Parsed compiler flags, preprocessor & dep-info | Explicitly declared in `BUILD` files; enforced by sandbox |
| **Unit of caching** | Whole builds (skips Cargo), near-builds, seeding, `rustc`/C/asm units, `build.rs`, tests | Single compiler invocations (`rustc`, C/C++, CUDA) | Single non-incremental compiler invocations | Every action in a polyglot build & test graph |
| **Proc macros & hidden reads** | Observed automatically (`JATO_KEY_ENV` for env vars) | Relies on `rustc` dep-info; untracked reads can miss | Relies on `rustc` dep-info; untracked reads can miss | Enforced by sandbox: undeclared reads fail or are hidden |
| **Build scripts & C/C++** | Caches both `build.rs` runs and nested C/C++/asm | Caches C/C++/CUDA; Cargo still runs `build.rs` | Caches C/C++/CUDA; Cargo still runs `build.rs` | First-class sandboxed `cargo_build_script` & C/C++ rules |
| **Incremental `rustc` state** | Saved & handed on across cleans and worktrees | Disabled during cached builds (cold first edit) | Skips caching incremental crates (dev crates miss unless `CARGO_INCREMENTAL=0`) | Non-incremental `rustc` actions by default |
| **Test caching** | Observed test-binary caching + parallel runner | None | None | First-class sandboxed test caching, sharding, & RBE |
| **Daemon & idle RAM** | None (`0 MiB`); shared FIFO jobserver | Background daemon (`21–115 MiB` RSS measured) | Local client-server daemon | Persistent JVM server (hundreds of MiB to multi-GiB) |
| **Local storage** | SQLite + BLAKE3 CAS; reflinks, ext4 zstd + RAM hot tier | SQLite + BLAKE3 CAS; reflinks or hardlinks/copies | Flat disk cache (no reflink target dedup) | Output base + content-addressed disk cache |
| **Remote cache / RBE** | Built-in `file://` and HTTP/1.1 CAS; no S3 SDK or RBE | Native S3-compatible (S3/R2/MinIO) + GHA; no RBE | S3, GCS, Azure, Redis, Memcached, WebDAV, GHA + `sccache-dist` | Industry-standard REAPI: remote cache + RBE worker farms |
| **Platforms** | Linux (`x86_64`, `aarch64`, 5.5+); passthrough elsewhere | Linux, macOS, Windows | Linux, macOS, Windows | Linux, macOS, Windows |

### Relative performance summary

| Workload | Jato | kache `2.3` | sccache `0.19` | Bare Cargo |
| --- | ---: | ---: | ---: | ---: |
| Olo `build --workspace` restore / new target *(ext4, EPYC 32c)* | **0.73–3.06 s** | 30.1–138.7 s | — | ~248 s (cold) |
| Olo `build --workspace` first edit in restored target *(ext4)* | **10.6–18.2 s** | 36.0 s | — | — |
| Olo `build --workspace` undo *(ext4)* | **0.69 s** | 11.4 s | — | — |
| `ripgrep 14.1.1` dev restore / new target / copy *(Btrfs, median of 3, load 21–30)* | **0.07–0.34 s** | 0.38–0.70 s | 4.86–8.09 s | 9.12–11.32 s |
| `ripgrep 14.1.1` dev subsequent edits / undo *(Btrfs)* | 0.52 s / **0.05 s** | 0.53 s / 0.16 s | 0.47 s / 0.51 s | 0.50 s / 0.69 s |

**Where Bazel / Blaze fits** *(not run in `bench/compare.py`, since porting a Cargo project to `BUILD.bazel` changes the graph itself)*: with **RBE**, Bazel/Blaze can beat any single-machine tool on cold builds by fanning compile and test actions across a worker farm, and warm remote/local graph hits are fast once the server is hot. Locally, cold JVM startup, multi-GiB server RAM, and non-incremental `rustc` compiles make single-crate edit loops heavier unless the codebase is split into many small targets.

### When to pick each

- **Jato** — *Right fit*: Linux workstations, shared Linux build servers, and multi-worktree coding agents on standard Cargo workspaces (especially large ones with build scripts, C/C++, proc macros, or frequent worktree/branch switching). *Not the right fit*: macOS/Windows hosts, teams requiring native S3/GCS/Azure SDKs or RBE clusters, or polyglot non-Cargo monorepos.
- **kache** — *Right fit*: Cross-platform (Linux/macOS/Windows) Rust + C/C++/CUDA teams wanting a fast drop-in `RUSTC_WRAPPER` with reflinks and native S3/R2/MinIO + GitHub Actions remote caching. *Not the right fit*: Large Cargo workspaces where Cargo startup, `build.rs` execution, hidden proc-macro inputs, or cold first-edit compiles in new worktrees dominate.
- **sccache** — *Right fit*: Cross-platform CI pipelines (especially with `CARGO_INCREMENTAL=0`) needing mature enterprise remote backends (S3, GCS, Azure, Redis, GHA) or `sccache-dist`. *Not the right fit*: Local `dev`-profile Cargo workflows and multi-worktree editing (does not cache incremental crates, whole builds, build scripts, or tests).
- **Bazel / Blaze** — *Right fit*: Large polyglot organizations (Rust + C++ + Go + Protobuf + Python/Java) needing strict hermetic reproducibility, cross-language graphs, test sharding, and RBE clusters. *Not the right fit*: Cargo-first Rust teams that want zero migration cost, standard `Cargo.toml`/`rust-analyzer`/crates.io tooling, low RAM use, and `rustc` incremental compilation.

---

## Watch mode, dev profile, storage & remote caches

### Watch on save & fast dev profile

```sh
jato watch                                  # check edited package first, then workspace
jato watch --focus-only                     # edited packages only
jato watch -- test --workspace              # run custom Cargo args on save
jato dev build                              # fast `jato-dev` profile (16 CGUs, line tables only, no dep debuginfo)
```

`jato watch` routes runs through `jato` (so undo/edits/tests hit the cache) and cancels stale runs immediately on a new save.

### Cache sizing, memory acceleration & remote sharing

```sh
jato limit 64GiB                            # disk budget (default: 10% of FS, 10–100 GiB)
jato memory 64GiB                           # RAM hot tier on ext4 (uncompressed page-cache objects + larger SQLite mmap)
jato gc                                     # prune down to budget (`jato gc 0` clears cache)
jato remote file:///mnt/shared/jato-cache   # shared directory / NFS remote CAS
jato remote http://cache.internal:8080/jato # HTTP/1.1 PUT/GET/HEAD CAS (set JATO_REMOTE_TOKEN for Bearer auth)
```

Remote fetches verify manifest keys, local inputs *before* downloading blobs, and streaming BLAKE3 digests on every object; transport or integrity errors fall back safely without failing a build. On ext4 servers without a Btrfs/XFS partition, a loopback Btrfs image gives zero-copy reflinks (`truncate -s 300G /srv/builds.img && mkfs.btrfs /srv/builds.img`).

---

## Configuration

| Command or variable | Default / Purpose |
| --- | --- |
| `JATO_CACHE_DIR` | Cache directory (`$XDG_CACHE_HOME/jato` or `~/.cache/jato`). Builds sharing it share results and CPU slots. |
| `JATO_JOBS` | Max concurrent compilers / default parallel test binaries across builds sharing the cache (default: CPU count; `0` disables). |
| `JATO_PARALLEL_TESTS` | Max test binaries `jato test` runs at once (`0` or `1` runs serially; overrides `Cargo.toml`). |
| `JATO_TEST_THREADS` | Default `RUST_TEST_THREADS` per test binary (`0` unsets `Cargo.toml`'s value). |
| `JATO_KEY_ENV=A,B` | Extra environment variables read by procedural macros via `std::env::var`. |
| `JATO_REMOTE` / `JATO_REMOTE_READONLY=1` / `JATO_REMOTE_TOKEN` | Override configured remote cache URL (`0` disables), pull-only mode, or HTTP Bearer token. |
| `JATO_MEMORY` / `JATO_MAX_SIZE` | Per-process override for `jato memory` / `jato limit`. |
| `JATO_LOG=debug` | Print hit/miss reasons, changed inputs, and restore timings (or pass an absolute file path to append). |
| `JATO_DISABLE=1` | Bypass Jato completely and invoke Cargo / compilers directly. |

Per-project test concurrency (for suites sharing a database or external service) can be set in `[workspace.metadata.jato]` (or `[package.metadata.jato]`) in `Cargo.toml`:

```toml
[workspace.metadata.jato]
parallel-tests = 4   # max test binaries at once (0 or 1 = serial; capped by JATO_JOBS/CPUs)
test-threads = 4     # default RUST_TEST_THREADS per binary (overridden by CLI --test-threads, JATO_TEST_THREADS, or RUST_TEST_THREADS)
```

<details>
<summary>All feature toggles & diagnostic environment variables</summary>

| Variable | Effect |
| --- | --- |
| `JATO_WHOLE_BUILDS=0` | Cache single compilations only; disable whole-build restore/recording. |
| `JATO_NEAR=0` | Disable starting from a nearby recorded build when sources differ. |
| `JATO_RECORD_EDITS=0` | Skip in-place whole-build recording after edits in an already-built target. |
| `JATO_SEED=0` | Disable cross-command seeding of empty target directories. |
| `JATO_CACHE_SCRIPTS=0` | Disable `build.rs` execution caching. |
| `JATO_TOOLS=0` | Disable caching C/C++/asm compilers invoked by build scripts. |
| `JATO_CACHE_TESTS=0` | Disable `jato test` binary result caching. |
| `JATO_PARALLEL_TESTS=<N>` | Cap concurrent workspace test binaries at `N` (`0` or `1` runs serially; overrides `[workspace.metadata.jato] parallel-tests`). |
| `JATO_TEST_THREADS=<N>` | Set default `RUST_TEST_THREADS=N` per test binary (`0` unsets `[workspace.metadata.jato] test-threads`). |
| `JATO_SAVED_STATE=0` | Do not save or hand on `rustc` incremental state across checkouts. |
| `JATO_MOUNT=0` | Disable optional unprivileged mount-namespace target canonicalization. |
| `JATO_REMAP=0` | Keep real workspace/target paths in compiler outputs (results stay local to checkout). |
| `JATO_JOBSERVER=0` / `1` | `0`: let Cargo create its own pipe; `1`: force named FIFO without checking `make` version. |
| `JATO_COALESCE=0` | Disable coalescing identical concurrent compilations. |
| `JATO_ASYNC_RECORD=0` | Wait for linked compiler output staging before waking Cargo. |
| `JATO_EDIT_WINDOW=<sec>` | Seconds before re-recording an edited crate (default `600`; `0` records every edit). |
| `JATO_BUSY=<load>` | Load average above which edits in a built target run unwatched (default: CPU count; `0` always watches). |
| `JATO_NOTES=0` | Disable preload read notes; route every file syscall through the kernel (`seccomp_unotify`). |
| `JATO_STORAGE=packed` | Store zstd-compressed blobs even on reflink-capable filesystems. |
| `JATO_VERIFY=1` | Verify BLAKE3 digests on restore even on checksumming filesystems (Btrfs). |
| `JATO_RECIPES=/dir` | Dump normalized command recipes per unit to diff why two runs missed. |
| `JATO_RA_MEMORY` | Total RSS budget across shared `rust-analyzer` backends (default: 25% RAM, 4–32 GiB; `0` disables). |
| `JATO_RA_MAX_BACKENDS` | Max concurrent shared `rust-analyzer` backends (default `8`; `0` unlimited). |
| `JATO_RA_IDLE_SECS` | Idle seconds before hibernating a worktree/backend (default `600`; `0` disables). |
| `JATO_RA_SHARE_WORKTREES` | `1` (default) shares one backend across git worktrees of a repo; `0` isolates each worktree. |
| `JATO_RA_LAZY` | `1` (default) defers spawning `rust-analyzer` until first file open/query; `0` spawns on `initialize`. |
| `JATO_RA_THREADS` / `JATO_RA_PRIME` / `JATO_RA_PROC_MACROS` / `JATO_RA_LRU` | Tune per-backend worker threads (`4–8`), eager cache priming (`0`), `proc-macro-srv` count (`1`), and syntax-tree LRU (`64`). |
| `JATO_RA_TARGET_DIR` / `JATO_RA_CHECK_ON_SAVE` / `JATO_RA_PASSTHROUGH=1` | Override shared per-repo `cargo.targetDir` (`1`/`0`/path), `checkOnSave` (`0`/`1`), or bypass multiplexing. |

</details>

---

## What is not cached

- Commands that exit non-zero (errors and exit status always come from a fresh run).
- Commands that touch the network, talk to unrecognized sockets, mutate an input file in place (beyond standard compiler/linker output rewrites), read source from stdin, or perform unobservable operations (`jato stats` and `jato why` report the exact program and path/socket).
- Non-filesystem nondeterminism such as wall-clock time (`__TIME__`, `SystemTime::now()`) or RNG values: those replay the first recorded result unless invalidated by a tracked input.
- Environment variables read directly by procedural macros via `std::env::var` (name them in `JATO_KEY_ENV`; `env!` macros and `build.rs` env directives are tracked automatically).

---

## Development & license

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
python3 tests/install_skill.py
```

See [AGENTS.md](AGENTS.md) for invariants and full release qualification (`JATO_NOTES=0`, `JATO_STORAGE=packed`, `JATO_TEST_MOLD`, `JATO_TEST_NIGHTLY`), [bench/README.md](bench/README.md) for all benchmarks, [TODO.md](TODO.md) for open work, and [JOURNAL.md](JOURNAL.md) for engineering notes.

Licensed under the [MIT License](LICENSE).
