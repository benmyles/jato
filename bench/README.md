# Jato measurements

The whole Olo workspace was measured on a build server with Jato 0.19.0, 0.18.0 and 0.17.0 and on a laptop with Jato 0.15.0, shared `jato rust-analyzer` with 0.20.0, what watching costs with 0.12.0 and 0.14.0, everything else with 0.12.0.

Benchmarks ran on an Intel Core Ultra 7 356H (16 logical processors, 15 available to the session, 62 GiB) with Fedora 45, Linux 7.3.4 and Btrfs with zstd compression. The machine was shared with other people's builds throughout: its load average is given with each set of numbers, and wall-clock times moved with it. Comparisons within one table were taken in one sitting, rotating the order of contenders where there was more than one run.

Each contender had its own empty store, its own daemon where it has one, its own copy of the project and its own target directories. The built program was run after every step and had to print the same thing for every contender. Times are wall-clock seconds.

## Olo

`cargo build -p olo-system` at Olo commit `645cce1e3aea`: 279 compilations on the pinned `nightly-2026-11-14` with `-Zthreads=8`, upstream mold 3.0.0, the project's dev profile and workspace feature unification. Each contender built in its checkout's own `target`. One run; load average 21 to 29, except during Jato's cold build, when it was 50.

| Seconds | Jato 0.12.0 | kache 2.3.0 | Bare Cargo |
| --- | ---: | ---: | ---: |
| Nothing stored, nothing built | 145.97 | 113.60 | 128.90 |
| The same build again | 0.09 | 0.60 | 0.55 |
| Target directory removed | 0.20 | 2.63 | a cold build |
| A target directory never used | 0.21 | 3.07 | a cold build |
| The sources copied to another path | 0.33 | 6.77 | a cold build |
| A function added to `olo-system` in that copy | 13.26 | 24.71 | |
| A function added in the restored target | 8.93 | 16.26 | 6.87 |
| Further such edits, median of three | 7.96 | 7.84 | 9.92 |
| The edits undone | 0.12 | 6.34 | 9.01 |

The cold builds used 769, 758 and 763 processor-seconds: watching every compiler and keeping what it wrote cost under 1% more than bare Cargo. Their wall-clock times follow the machine's load, not the contender: over five sittings Jato's cold build took 130 to 146 s and once 662 s, kache's 114 to 339 s, Cargo's 118 to 285 s.

In Jato's restores Cargo does not start; kache answers 279 compiler invocations one after another, and for a target directory it has not seen it fills in what it can before Cargo looks. The first edit in a checkout where nothing has been compiled used 53 processor-seconds with Jato, which handed rustc the state it had saved in the first checkout, and 125 with kache, where rustc compiled the crate from nothing. In the restored target the same took 21 against 47 processor-seconds; Cargo's own target, built in place, still had rustc's state and took 19. Before Jato handed saved state on, both of these edits cost it 108 to 127 processor-seconds.

After the cold build Jato's store held 1,212 MiB: the build's outputs (616 MiB) and the state rustc saved for the workspace's 15 crates. 12 MiB of that was not shared with the target directory it was cloned from. kache's store held 625 MiB with 11 MiB of its own; kache builds without incremental state. The target directory was 1,209 MiB under Jato and bare Cargo and 646 MiB under kache. A target Jato restores takes 1.3 MiB of its own until something in it is compiled. kache's daemon held 21 MiB of memory between builds; Jato has no daemon. [Samples, processor time and disk accounting](olo-0.12.json)

## The whole Olo workspace

`cargo check --workspace` at Olo commit `392b862f3`, in a clone of its git repository: 121 packages, 1,502 Rust compilations, and 113 build scripts that between them compile about 1,100 C, C++ and assembly files. Each contender handled C its own way: kache as `kache cc` in `CC`, `CXX`, `HOST_CC` and `HOST_CXX`, as its setup leaves them, and Jato through the names it puts on `PATH`. Jato 0.15.0 and kache 2.3.0 went through the steps at the same time, each in its own copy with its own store, so that both met the same machine. One run; the load average was 29 to 38 at the start of each step, and 52 at kache's last, which ran after Jato had finished.

| Seconds, processor-seconds | Jato 0.15.0 | | kache 2.3.0 | |
| --- | ---: | ---: | ---: | ---: |
| Nothing stored, nothing built | 913.60 | 2,319 | 938.01 | 2,297 |
| The same check again | 0.83 | 0.6 | 1.37 | 0.9 |
| Target directory removed | 2.21 | 1.7 | 18.00 | 55 |
| A target directory never used | 2.49 | 1.9 | 30.33 | 117 |
| The sources copied to another path | 3.78 | 2.4 | 13.22 | 42 |
| A function added to `olo-system` in that copy | 8.39 | 31 | 16.63 | 91 |
| A third copy, the function added before anything is built there | 14.76 | 39 | 32.84 | 126 |
| A function added in the restored target | 10.84 | 29 | 14.31 | 32 |
| Further such edits, median of three | 8.47 | 23 | 24.53 | 27 |
| The edits undone | 0.92 | 0.7 | 41.76 | 28 |

Putting a build back is one command's result for Jato and 2,600 for kache, of which the C compilations and the build scripts that start them are most of the processor time. `olo-system` has 54 dependents in the workspace, so each edit of it compiles 55 crates.

The third copy is what a new worktree with an edit in it is: sources no build was made from. Jato starts from the nearest recorded build, puts its files in place without what the compiler of `olo-system` wrote, and has Cargo compile that crate and its dependents; kache answers every compiler and runs every build script. Measured separately in git worktrees of the clone, at a load of 18 to 25: a first build with nothing changed 2.3 s, with one line changed in one program 7.2 s, with a comment added to a crate at the bottom of the workspace 5.9 s, with a function added to `olo-system` and committed 11.6 s and 40 processor-seconds, and the next worktree with any of those sources 1.8 to 2.2 s. Without starting from an earlier build (`JATO_NEAR=0`), as Jato did until 0.15, the one line took 22.6 s and 75 processor-seconds and the function 37.2 s and 107.

Clippy over the same workspace, in a worktree where the check had been put back, compiled the 121 packages with its own driver for 321 processor-seconds and was recorded as resting on the check, whose 13,088 files it holds with its own. `clippy --workspace` as the first command in the next new worktree was put back in 5.3 s, at a load of 61.

From nothing the two take the same time and the same processor time when they run side by side. One after the other on this machine they do not, because other people's builds come and go by the minute: over four such sittings Jato's check from nothing took 508, 1,490, 670 and 1,890 s and kache's 547, 642, 428 and 548 s, each time for 2,250 to 2,730 processor-seconds, and whichever ran while the machine was busiest took longest. The desktop this machine runs keeps compilers in a group that gets a hundredth of the processors' time whenever anything else wants them, so a build's time on the clock says more about what else ran than about the build. Comparisons of builds from nothing here are only meaningful side by side.

Before Jato remembered the C that build scripts compile (0.13), a copied checkout was its worst case: every Rust compilation was restored, the whole build was not, because two C object files with debugging information named the first checkout, and Cargo ran all 113 build scripts, which compiled everything again. That took 929 s and 630 processor-seconds at a load of 40 to 70, 545 of them in C compilers and assemblers.

After the check from nothing Jato's store held 3,100 MiB and kache's 1,608 MiB, the target directories 3,071 and 1,600 MiB: rustc's incremental state, which kache builds without and which is what makes the edits above cheap. Of Jato's target directory 16 MiB was its own; the rest is shared with the store, block for block, and a restored target directory shares all of it. kache's daemon held 42 MiB between builds. [Jato's samples](olo-workspace-0.15-jato.json), [kache's](olo-workspace-0.15-kache.json), and the runs with 0.13.0 ([first](olo-workspace-0.13-watched.json), [second](olo-workspace-0.13.json))

## The whole Olo workspace on a build server (0.19.0 vs 0.18.0)

Measured on the 32-processor AMD EPYC 9655 build server (125 GiB RAM, Ubuntu 26.04, Linux 7.0, ext4) on Olo (`d01bd55a`, 121 packages, 56 dev binaries, 20.9 GiB target, pinned `nightly-2026-11-14`, mold 2.40.4), single-contender runs (`--runs 1 --c-compilers own --cargo-args --workspace`), zero uncacheable units or builds in all runs. `0.18.0` ran on a quiet machine (load 0.1–21); `0.19.0` default (packed zstd `-5`, 40 GiB cache) and `0.19.0` with `JATO_MEMORY=64GiB` (80 GiB cache) ran under background server load 5.7–49.1.

`check --workspace` (69,220 inputs, 13,088 files, 1.57 GiB):

| Seconds (load) | Jato 0.18.0 | Jato 0.19.0 | Change |
| --- | ---: | ---: | ---: |
| Nothing stored, nothing built | 78.77 (0.1) | 86.68 (5.7) | +10.0% |
| The same check again | 0.125 (21) | **0.057** (20) | **-54.4%** |
| Target directory removed | 0.468 (21) | **0.371** (19) | **-20.7%** |
| A target directory never used | 0.477 (21) | **0.463** (19) | -2.9% |
| The sources copied to another path | 0.687 (21) | **0.382** (19) | **-44.4%** |
| A function added to `olo-system` in that copy | 3.337 (21) | 3.525 (19) | +5.6% |
| A third copy, edited before first build | 5.382 (21) | **5.081** (17) | -5.6% |
| A fourth copy, `Cargo.toml` edited first | 3.210 (19) | **3.010** (16) | -6.2% |
| First edit in the restored target | 3.395 (19) | **3.364** (15) | -0.9% |
| Further edits, median of three | 3.043 (20) | 3.178 (17) | +4.4% |
| The edits undone | 0.240 (19) | **0.183** (16) | **-23.8%** |

`build --workspace`, dev profile (72,527 inputs, 13,912 files, 20.9 GiB / 56 binaries):

| Seconds (load) | Jato 0.18.0 | Jato 0.19.0 (packed) | vs 0.18 | Jato 0.19.0 (`memory 64GiB`) | vs 0.18 |
| --- | ---: | ---: | ---: | ---: | ---: |
| Nothing stored, nothing built | 240.34 | 256.73 (15) | +6.8% | 245.69 (45) | +2.2% |
| The same build again | 0.146 | **0.071** (31) | **-51.4%** | **0.073** (37) | **-50.0%** |
| Target directory removed | 2.623 | **1.530** (28) | **-41.7%** | **1.271** (34) | **-51.5%** |
| A target directory never used | 3.175 | **1.254** (28) | **-60.5%** | **0.730** (34) | **-77.0%** |
| The sources copied to another path | 8.497 | **6.414** (29) | **-24.5%** | **5.592** (34) | **-34.2%** |
| A function added to `olo-system` in that copy | 32.163 | **31.935** (27) | -0.7% | 32.279 (32) | +0.4% |
| A third copy, edited before first build | 36.439 | 36.491 (33) | +0.1% | **36.230** (30) | -0.6% |
| A fourth copy, `Cargo.toml` edited first | 12.992 | **7.552** (32) | **-41.9%** | **8.133** (37) | **-37.4%** |
| First edit in the restored target | 14.748 | **10.622** (32) | **-28.0%** | **11.622** (32) | **-21.2%** |
| Further edits, median of three | 10.691 | **9.181** (35) | **-14.1%** | **8.739** (40) | **-18.3%** |
| The edits undone (11.8 GiB changed) | 8.937 | **0.693** (49) | **-92.2%** | **0.738** (47) | **-91.7%** |
| **8 cache-served steps sum** (excl. `copy_edit`/`copy_new`) | 61.809 | **37.317** | **-39.6%** | **36.898** | **-40.3%** |

Store size after cold `build --workspace`: 8.60 GB packed (`0.19.0`, zstd `-5`), 21.60 GB with `JATO_MEMORY=64GiB` (uncompressed hot tier in page cache). Restored `new_target` with `JATO_MEMORY=64GiB` places 13,912 files (20.9 GiB logical, ~10.5 GiB unique after hardlink deduplication of `deps/<bin>-<hash>` + `<bin>` pairs) verified with BLAKE3 in **0.730 s (28.7 GB/s effective)**. On quieter single-contender verification runs (`load` 1–32), `0.19.0` packed achieved `restore` **1.10 s** (-58%), `new_target` **0.97 s** (-69%), `edit_first` **9.55 s** (-35%), `edit` **7.90 s** (-26%), and `undo` **0.59 s** (-94%). Raw data: `bench/olo-server-0.19-{check,build,build-mem64}-jato.json`.

## The whole Olo workspace on a build server (0.18.0)

Jato 0.18.0 and kache 2.3.0 on the same 32-processor AMD EPYC 9655 server (125 GiB RAM, Ubuntu 26.04, Linux 7.0, ext4), started together one command each on Olo (`d01bd55a`, 121 packages, 56 binaries, 20.6 GiB dev target, pinned `nightly-2026-11-14`, mold 2.40.4). Zero uncacheable units or builds in either run.

`check --workspace`:

| Seconds, processor-seconds, load | Jato 0.18.0 | | | kache 2.3.0 | | | Speedup |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Nothing stored, nothing built | 153.88 | 2,249 | 7 | 160.55 | 2,302 | 7 | 1.04× |
| The same check again | 0.13 | 0.2 | 52 | 0.59 | 0.5 | 48 | 4.65× |
| Target directory removed | 1.52 | 2.9 | 48 | 11.85 | 62.6 | 45 | 7.82× |
| A target directory never used | 0.99 | 2.8 | 48 | 11.41 | 87.6 | 41 | 11.49× |
| The sources copied to another path | 0.52 | 2.6 | 48 | 10.37 | 112.3 | 39 | 20.02× |
| A function added to `olo-system` in that copy | 3.34 | 28.4 | 48 | 9.41 | 183.5 | 35 | 2.82× |
| A third copy, the function added before anything is built there | 6.14 | 33.6 | 45 | 20.99 | 156.2 | 33 | 3.42× |
| A fourth copy, a note added to `olo-system`'s manifest first | 3.80 | 8.4 | 42 | 10.72 | 124.4 | 23 | 2.82× |
| A function added in the restored target | 4.38 | 26.4 | 41 | 7.19 | 125.5 | 22 | 1.64× |
| Further such edits, median of three | 3.98 | 23.3 | 41 | 3.08 | 24.3 | 22 | 0.77× |
| The edits undone | 0.26 | 0.4 | 37 | 2.96 | 26.2 | 21 | 11.28× |

Store size: Jato 1,351 MiB (0 MiB idle RAM) vs kache 1,794 MiB (115 MiB daemon RSS).

`build --workspace`, dev profile, 56 programs (20.6 GiB):

| Seconds, processor-seconds, load | Jato 0.18.0 | | | kache 2.3.0 | | | Speedup |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Nothing stored, nothing built | 480.44 | 7,478 | 19 | 482.26 | 7,430 | 19 | 1.00× |
| The same build again | 0.15 | 0.2 | 62 | 1.84 | 0.7 | 62 | 12.25× |
| Target directory removed | 3.06 | 22.4 | 57 | 30.13 | 102.9 | 57 | 9.86× |
| A target directory never used | 5.19 | 26.3 | 56 | 138.71 | 2,022.6 | 50 | 26.71× |
| The sources copied to another path | 18.21 | 82.6 | 56 | 22.22 | 98.6 | 60 | 1.22× |
| A function added to `olo-system` in that copy | 54.62 | 672.7 | 50 | 78.67 | 1,331.0 | 53 | 1.44× |
| A third copy, the function added before anything is built there | 72.70 | 771.8 | 61 | 89.17 | 1,090.1 | 35 | 1.23× |
| A fourth copy, a note added to `olo-system`'s manifest first | 10.06 | 82.9 | 67 | 19.59 | 80.5 | 16 | 1.95× |
| A function added in the restored target | 18.24 | 136.5 | 60 | 35.98 | 620.9 | 15 | 1.97× |
| Further such edits, median of two | 13.14 | 62.1 | 66 | 11.74 | 60.6 | 22 | 0.89× |
| The edits undone | 15.27 | 14.6 | 60 | 11.40 | 66.4 | 40 | 0.75× |

Store size: Jato 6,155 MiB (zstd-1, 0 MiB idle RAM) vs kache 12,319 MiB (2× smaller disk). With `copy_manifest`, incremental `.rlib` session normalization eliminated the 40-binary relink cascade (10.06 s / 82.9 cpu vs 55.01 s / 710 cpu in 0.17.0, and 1.95× faster than kache despite running at load 67 vs kache's 16). Raw data: `bench/olo-server-0.18-{check,build}-{jato,kache}.json`.

With the configurable RAM hot tier (`jato memory 64GiB`) on ext4 (quiet machine, Olo `build --workspace`, 13,912 files, 20.6 GiB):
- Cold `build --workspace`: **239.81 s** (vs bare Cargo's 248.1 s).
- Fresh `build --workspace` (72,742 inputs checked in parallel across 8 threads): **0.13 s** (62 ms input verification).
- Full 20.6 GiB restore (single-pass SIMD BLAKE3 verified from page cache): **1.78 s** (1,456 ms write = 14.2 GB/s).
- Cross-command target seeding in brand-new worktrees: `jato test -p olo-system --no-run` **14.40 s** (seeded 13,909 files, pruned 11,486 unused files, 0 Cargo stale units on subsequent restore); `jato clippy -p olo-system` **11.37 s** (seeded in 1.02 s).

Observed test caching + parallel workspace test execution (`jato test --workspace` on `ripgrep`, ext4):
- Cold build + cold parallel tests: **7.17 s**; warm repeat with cached tests: **1.42 s**; brand-new worktree (restored build + cached tests): **1.68 s** — against **2.97 s** for bare `cargo test --workspace` in an already-built target.

Six open-source Rust workspaces (`check --workspace`, ext4, fresh git worktree `b` and edited worktree `c` with a function added to the most-depended library):

| Workspace | Cold | New worktree `b` (0.17 → 0.18) | Edited worktree `c` (compilers) | Cargo stale in `b` |
| --- | ---: | ---: | ---: | ---: |
| `ripgrep` | 2.8 s | 0.1 s | — | 0 |
| `rust-analyzer` (142 crates) | 26.1 s | 9.0 s → **0.4 s** (22.5×) | 7.2 s (31, was 274) | 0 |
| `cargo` (34 crates) | 16.5 s | 8.5 s → **0.7 s** (12×) | 3.2 s (12, was 703) | 0 |
| `tokio` | 4.5 s | **0.4 s** | 1.2 s (5) | 0 |
| `nushell` | 47.8 s | 20.9 s → **2.5 s** (8.4×) | 9.3 s (42) | 0 |
| `ruff` | 37.9 s | unrecorded → **9.1 s** (5 of 529 units) | 11.2 s (59) | 0 |

## The whole Olo workspace on a build server (0.17.0)

Jato 0.17.0 and kache 2.3.0 on a server with 32 processors (AMD EPYC 9655), 125 GiB of memory, Ubuntu 26.04, Linux 7.0 and ext4 on a disk that writes 7.8 GB a second, with nothing else running. Olo at commit `d01bd55a`, the pinned `nightly-2026-11-14`, mold 2.40.4. The two were started together, one command each, so each had the other's load beside it: Jato was done first both times, and kache's last steps ran on a quieter machine than Jato's. One run each. The load is the one-minute average when the step began.

`check --workspace`:

| Seconds, processor-seconds, load | Jato | | | kache | | |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Nothing stored, nothing built | 169.51 | 2,444 | 5 | 172.80 | 2,479 | 5 |
| The same check again | 0.25 | 0.3 | 50 | 0.44 | 0.4 | 46 |
| Target directory removed | 0.71 | 3 | 46 | 13.72 | 97 | 43 |
| A target directory never used | 0.88 | 3 | 46 | 12.98 | 94 | 38 |
| The sources copied to another path | 0.99 | 3 | 46 | 12.31 | 108 | 34 |
| A function added to `olo-system` in that copy | 4.02 | 29 | 46 | 10.43 | 199 | 29 |
| A third copy, the function added before anything is built there | 7.01 | 36 | 43 | 17.01 | 174 | 30 |
| A fourth copy, a note added to `olo-system`'s manifest first | 3.90 | 8 | 39 | 12.03 | 143 | 22 |
| A function added in the restored target | 4.35 | 27 | 38 | 8.17 | 153 | 22 |
| Further such edits, median of three | 3.88 | 23 | 34 | 3.14 | 27 | 23 |
| The edits undone | 0.46 | 0.5 | 31 | 3.06 | 26 | 20 |

`build --workspace`, the dev profile, 56 programs:

| Seconds, processor-seconds, load | Jato | | | kache | | |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Nothing stored, nothing built | 513.21 | 7,878 | 14 | 508.38 | 7,773 | 14 |
| The same build again | 0.23 | 0.2 | 62 | 0.76 | 0.5 | 67 |
| Target directory removed | 5.31 | 24 | 58 | 28.11 | 139 | 62 |
| A target directory never used | 7.17 | 22 | 56 | 134.60 | 2,128 | 48 |
| The sources copied to another path | 13.74 | 47 | 54 | 20.47 | 113 | 62 |
| A function added to `olo-system` in that copy | 61.76 | 703 | 48 | 111.19 | 1,449 | 56 |
| A third copy, the function added before anything is built there | 74.50 | 817 | 61 | 95.06 | 1,093 | 64 |
| A fourth copy, a note added to `olo-system`'s manifest first | 55.01 | 710 | 58 | 15.79 | 215 | 23 |
| A function added in the restored target | 34.98 | 164 | 57 | 38.18 | 650 | 22 |
| Further such edits, median of two | 15.06 | 65 | 60 | 13.47 | 64 | 25 |
| The edits undone | 11.50 | 12 | 82 | 9.81 | 66 | 20 |

From nothing the two are the same: 2% apart either way, with 1% between their processor times. Everything that puts a build back is Jato's by a wide margin, a second where kache takes thirteen for the check, and five to seven where kache takes 28 and 135 for the build.

kache is ahead in three places. Further edits in a target directory that is built take both about as long, and kache's were measured at half the load: 3.1 s against 3.9 for the check, 13.5 against 15.1 for the build. Alone on the quiet machine the same edits of the check took Jato 2.9 to 3.2 s, of which Cargo and rustc took 2.8 to 3.0. Undoing the edits of the build took Jato 11.5 s at a load of 82, 12 processor-seconds of it its own, to put the programs back that the edits had linked again; kache took 9.8 s and 66 processor-seconds.

The third is the note in a manifest, for the build: 55 s against 16. A procedural macro `olo-system` uses reads the crate's own `Cargo.toml`. Jato sees that and has the crate compiled again; Cargo and kache know nothing of it and do not. For the check that costs one compilation, since what the crates above read of `olo-system` comes out the same byte for byte and they are answered from the cache. For the build the library comes out as other bytes, as it does every time rustc compiles it (the object files in it are named with a string made up for each compilation), and the 40 programs that link it are linked again. Run alone on a quiet machine the same step took 34 s. With a manifest nothing compiles from, the build starts from the recorded one and Cargo finds nothing to do: 2.1 s for the check.

After the build from nothing Jato's store held 5,703 MiB and kache's 12,319 MiB: on a filesystem that cannot clone, Jato keeps what it stores compressed. The target directories were 20,543 and 12,360 MiB, the difference being rustc's incremental state. For the check: 1,281 and 1,716 MiB stored, 3,073 and 1,602 MiB of target directory.

The same check of real commits, each in a new worktree against a record of the newest commit, Jato alone on the quiet machine: one commit back, where the lock file and a manifest differ and one build script has to run again, 3.1 s (10.2 s when Cargo carried the build out, before builds started across manifests); five commits back 9.4 s with 88 compilers run; forty commits back, 181 inputs differing, 20 s with 255 compilers run and 2,035 files of the earlier build taken away. Each was then compared with a check of the same commit from nothing with a cache of its own: the same files, but for 41 object files that the `cc` crate names after the path of its output directory. And every compilation was asked for one at a time into another target directory: all answered by records that match, but for the eight crates downstream of a build script that writes its source differently every time.

Reproduce with one command for each contender, started together:

```sh
python3 bench/compare.py --offline --project /path/to/olo --command check --c-compilers own \
    --edit crates/system/src/lib.rs --tools jato --jato /path/to/jato --kache /path/to/kache \
    --runs 1 --cache-gib 60 --scratch /path/on/disk --cargo-args --workspace
```

The data is in `bench/olo-server-0.17-{check,build}-{jato,kache}.json`.

`bench/same.py A/target B/target` compares two target directories file by file, with where each is taken out of what the files say: for a build that started from another against one of the same sources made from nothing, in a second worktree with a cache of its own.

## ripgrep

ripgrep 14.1.1 on Rust 1.100, default dev profile, median of three rotating runs; load average 21 to 30. `jato` is `jato build`; `jato wrapper` is plain Cargo with Jato as `RUSTC_WRAPPER`, which reuses single compilations only.

| Seconds | `jato` | `jato` wrapper | kache 2.3.0 | sccache 0.19.0 | Bare Cargo |
| --- | ---: | ---: | ---: | ---: | ---: |
| Nothing stored, nothing built | 10.93 | 16.77 | 7.33 | 8.53 | 7.83 |
| The same build again | 0.01 | 0.04 | 0.03 | 0.03 | 0.03 |
| Target directory removed | 0.07 | 0.18 | 0.38 | 4.86 | 9.53 |
| A target directory never used | 0.34 | 0.17 | 0.61 | 8.09 | 9.12 |
| The sources copied to another path | 0.08 | 0.19 | 0.70 | 7.54 | 11.32 |
| A function added in that copy | 2.13 | 3.23 | 2.94 | 0.50 | 0.68 |
| A function added in the restored target | 1.06 | 0.91 | 2.07 | 0.47 | 0.77 |
| Further such edits, median of three | 0.52 | 0.51 | 0.53 | 0.47 | 0.50 |
| The edits undone | 0.05 | 0.05 | 0.16 | 0.51 | 0.69 |

The cold builds used 25.8 to 27.7 processor-seconds with `jato`, 25.6 to 28.3 as a wrapper, 23.2 to 23.6 with kache, 26.7 to 28.4 with sccache and 24.4 to 25.6 with bare Cargo: on a project this small, watching the compilers and keeping rustc's state for nine crates costs 5 to 8%. The cold wall-clock samples of one contender spread from 8 to 22 s as the machine's load moved; a build of the same kind alone at a load of 15 took 5.7 s. In those runs each wrapper kept rustc's state before telling Cargo it was done; under `jato build` that now happens after.

For sccache and bare Cargo the copy is a cold build, which leaves rustc's own state behind for the edit that follows; sccache does not cache the workspace's incrementally compiled crates at all. For a crate this small the state Jato hands on is worth little over compiling from nothing: 2.1 s against kache's 2.9 s in the copy. [Samples and disk accounting](ripgrep-0.12.json)

## Builds started together

Eight builds of ripgrep started at the same moment, each in its own copy with a compiler setting of its own so that none could use another's results. `jato` has its default of 15 compilers at once; `jato, no limit` is `JATO_JOBS=0`. Two runs, taken while the machine's load average before each start was between 27 and 97, which is why the times of one contender differ by as much as they do; the order was rotated.

| | `jato` | `jato`, no limit | kache 2.3.0 | Bare Cargo |
| --- | ---: | ---: | ---: | ---: |
| Most compilers running at once | 15 | 119, 120 | 16 | 117, 112 |
| Most memory resident at once | 4.1, 3.9 GiB | 12.7, 12.9 GiB | 3.8, 3.8 GiB | 14.3, 13.9 GiB |
| Processor-seconds | 225, 242 | 244, 281 | 225, 290 | 223, 235 |
| Seconds until the last build finished | 75, 103 | 95, 226 | 78, 390 | 47, 116 |
| Load average just before | 77, 70 | 68, 63 | 77, 74 | 27, 97 |

Without the limit eight builds ran 119 compilers on 15 processors and held three times the memory. With it they used 8 to 14% less processor time and finished sooner in both runs. kache's daemon limits compilers in the same way. Bare Cargo's first run happened to start on a much quieter machine than the others'. Memory is the sum of resident sizes, which counts pages shared between compilers once for each. [Samples](concurrent-ripgrep-0.12.json)

## Checks on save

`jato watch` on a workspace of two crates, seven saves each way, while the machine's load average was above 50. A save that breaks the code reached the compiler's error in a median 137 ms, and the save that repairs it reached a clean check of the workspace in 154 ms. Four further runs alternated the cache on and off (`JATO_DISABLE=1`) at a load of 56 to 58: 129 and 150 ms to the error with it on against 160 and 179 ms with it off, and 153 and 173 ms to the clean check against 259 and 338 ms, since the repair returns to a state that is already recorded. The idle watcher held 4.5 MiB. [Samples of the first run](feedback-0.12.json)

## What observation costs

Measured with 0.12, where the kernel reported every file call: the same incremental compilation of `olo-system` (1.1 to 1.7 s unwatched) was run alternately with and without the observer. With the machine's load average at 22 the two were indistinguishable: 1.07 to 1.44 s unwatched, 1.00 to 1.69 s watched and recorded. With it between 35 and 50 the watched runs took 2.1 to 2.4 s: each of the reported system calls waits for the observer to be given a processor, and the compiler for its next turn after that. Checking `olo-system` and its 54 dependents after an edit took 17 to 25 s watched and 7 to 14 s unwatched at a load of 45 to 55.

With 0.14 programs tell of their own reads. For one check of `olo-system`, three runs each way in alternation at a load of 23:

| | Unwatched | Watched, kernel alone | Watched, with notes |
| --- | ---: | ---: | ---: |
| Calls the kernel reported | | 2,600 before, 796 without listings | 158 |
| Times the command gave up the processor | 3,500 to 5,400 | 20,900 to 22,200 | 6,950 to 7,980 |
| Seconds | 1.86 to 2.02 | 1.99 to 3.12 | 1.81 to 2.81 |

A build from nothing has slack that hides those waits: two cold checks of `olo-system` run side by side at a load of 50, one watched each way, took 397.88 and 397.91 s for 429 and 430 processor-seconds. ripgrep from nothing at a load of 31, three runs in alternation: bare Cargo 8.2 to 9.5 s and 25.0 to 26.5 processor-seconds, Jato with the kernel alone 9.9 to 11.5 s and 27.0 to 28.2, Jato with notes 9.7 to 10.8 s and 26.9 to 28.5.

## Shared `rust-analyzer` across worktrees (0.20.0)

Measured on a 64-core Linux 7.3.4 build server (Btrfs, live background load 43–79, isolated `JATO_CACHE_DIR`) against `shiftup-v2` (main checkout + 11 sibling git worktrees) and `olo` (pinned `nightly-2026-11-14`):

- **Standalone unshared `rust-analyzer`**: 4.2–5.7 GiB RSS per session (~118 GiB across 21 active clients).
- **Idle / lazy sessions**: 9 sessions across `shiftup-v2` worktrees and `olo` that only sent `initialize` consumed **0 B RAM** (0 backends spawned), with **1.5 ms** warm lazy `initialize` via stat-validated toolchain caching (down from 72.6 ms).
- **Multi-worktree sharing & query latency**:
  - 15 active clients across 12 `shiftup-v2` worktrees shared **1 backend** at **2.44 GiB RSS** (~35× reduction vs ~86 GiB unshared), with 15/15 resolving `hover` and `definition` strictly inside their own worktree.
  - 21 active clients across 12 `shiftup-v2` worktrees + `olo` ran in **2 backends** (**9.23 GiB** total RSS even after full `workspace/symbol` indexing vs ~118 GiB unshared), with 11/11 sibling worktrees isolated.
  - Warm 2nd-client attach + `hover` on `shiftup-v2` (`openai.rs`, 39 KB): **3.20 ms** (vs **47.7 s** cold first query; ~15,000× faster); on `olo`: **1.35 ms**.
  - Cached single-file `documentSymbol`: **0.56 ms** min / **0.75 ms** median (vs **7.76 ms** uncached).
  - 8 concurrent identical `workspace/symbol` queries coalesced into 1 backend query in **36.3 ms**.
  - Live `SIGKILL` backend crash recovery on `shiftup-v2`: reaped full process group (`-pgrp`) with 0 orphan `proc-macro-srv`/`cargo` children and recovered in **9.97 s** (vs 44.5 s cold, 4.5× faster via shared `cargo.targetDir`).

## Reproduction

Use prefetched dependencies and a scratch directory on disk. `--no-rebuilds cargo` leaves out the steps in which bare Cargo would only repeat its cold build.

```sh
RUSTUP_TOOLCHAIN=stable python3 bench/compare.py --offline --project /path/to/ripgrep-14.1.1 \
    --edit crates/core/main.rs --verify "rg --version" \
    --tools cargo jato jato-wrapper kache sccache --runs 3
python3 bench/compare.py --offline --project /path/to/olo --package olo-system \
    --edit crates/system/src/lib.rs --verify "nm-fake --help" \
    --tools cargo jato kache --no-rebuilds cargo --runs 1
# Side by side: one command for each contender, started together, each with a scratch directory of its own.
for tool in jato kache; do
    python3 bench/compare.py --offline --project /path/to/olo --command check --c-compilers own \
        --edit crates/system/src/lib.rs --tools $tool --runs 1 --cache-gib 40 \
        --scratch /var/tmp/compare-$tool --cargo-args --workspace > olo-workspace-$tool.json &
done; wait
RUSTUP_TOOLCHAIN=stable python3 bench/concurrent.py --offline --project /path/to/ripgrep-14.1.1 \
    --verify "rg --version" --builds 8 --tools cargo jato jato-unlimited kache
python3 bench/feedback.py --jato /path/to/jato
```

`compare.py` takes each contender through a cold build, the same build again, the target directory removed, a new target directory, a copy of the sources at another path, a second copy that is edited before anything is built in it, edits to one file and their undoing. Contenders named in one command run one after the other. On a machine other people are building on, start one command for each contender at the same moment instead, as above: their builds from nothing then meet the same load. By default build scripts are given the C compilers by their paths and no contender caches C; `--c-compilers own` gives kache and sccache their wrappers and leaves Jato the names it puts on `PATH`. `jato` is Jato run as `jato build`; `jato-wrapper` is Jato named as `RUSTC_WRAPPER` under plain Cargo, which reuses single compilations only. `concurrent.py` starts several builds at once that share nothing, to show how they share the machine. `feedback.py` measures a save to the compiler's diagnostic under `jato watch`.
