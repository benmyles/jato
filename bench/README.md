# Jato 0.3.0 measurements

The comparison uses the installed Jato 0.3.0, Jato 0.2.0, the machine's kache 2.1.1, upstream kache 2.2.0, upstream sccache 0.19.0 and plain Cargo. All timed builds use the same Rust 1.100.0 toolchain, four Cargo jobs and default profile settings on an Intel Core Ultra 7 356H running Fedora 45. The local Btrfs volume uses Zstandard level 1 compression. The workstation was shared with other workloads. Full per-run timings and counters accompany the medians. [0.2 report](0.2.md) · [0.1 report](0.1.md)

## ripgrep 14.1.1

| Workload | Cargo | Jato 0.2 | Jato 0.3 | Installed kache 2.1.1 | sccache 0.19 |
| --- | ---: | ---: | ---: | ---: | ---: |
| First check | 3.37 s | 3.44 s | 6.52 s | 3.81 s | 4.14 s |
| Clean-target check, warm store | 3.69 s | 157 ms | 168 ms | 311 ms | 1.19 s |
| Unchanged check, existing target | 62 ms | 38 ms | 40 ms | 76 ms | 63 ms |
| First release build | 30.02 s | 36.06 s | 27.56 s | 28.40 s | 16.23 s |
| Clean-target release rebuild, warm store | 25.71 s | 289 ms | **198 ms** | 251 ms | 5.52 s |
| Unchanged release build, existing target | 60 ms | 48 ms | 41 ms | 90 ms | 73 ms |

All three warm release trials favored Jato 0.3 over both Jato 0.2 and the installed kache. Cold checks were slower in this run. The numbers are wall-clock medians from three trials. [Interleaved build times and memory samples](ripgrep-0.3.json)

A separate 21-pair experiment compared only the two Jato versions with prefilled stores. The warm check medians were 161 ms for 0.3 and 156 ms for 0.2. The median difference **within each adjacent pair** was 0.17 ms, with 0.3 first in ten pairs and 0.2 first in eleven. [All matched check samples](paired-0.3.json)

### Upstream kache 2.2.0

| ripgrep, separate three-trial comparison | Jato 0.3 | kache 2.2.0 |
| --- | ---: | ---: |
| First check | 3.17 s | 3.23 s |
| Clean-target check, warm store | **148 ms** | 353 ms |
| First release build | 18.96 s | 19.53 s |
| Clean-target release rebuild, warm store | **173 ms** | 305 ms |
| Unchanged release build | 37 ms | 88 ms |

The two tools ran against one another with separate private local stores. Jato's warm release times ranged from 164 to 182 ms and kache's from 304 to 327 ms. The private kache 2.2 server used 18.6 MiB RSS after the release builds. [Upstream kache and Jato samples](ripgrep-kache-2.2-0.3.json)

### Packed storage

| ripgrep release | Store file lengths | Unique compressed chunks | Warm restore |
| --- | ---: | ---: | ---: |
| Jato 0.2 packed | 48.8 MiB | 46.0 MiB | 325 ms |
| Jato 0.3 packed | **37.3 MiB** | **34.8 MiB** | **294 ms** |

Aligning packed chunks with archive members and embedded metadata reduced the store's apparent size by 24%. The automatic Btrfs mode used 160.4 MiB of file lengths, including shared extents, and had the faster 198 ms restore. Its largest process used 85.2 MiB on that restore. The Jato cache used zero idle server memory. The private kache 2.1.1 and sccache servers used 26.4 MiB and 18.8 MiB respectively. [Storage and RSS samples](ripgrep-0.3.json)

### Retaining source revisions

| Packed library history | One revision | Four revisions | Eight revisions |
| --- | ---: | ---: | ---: |
| Jato 0.2 unique payload | 23.4 MiB | 93.7 MiB | 187.3 MiB |
| Jato 0.3 unique payload | **13.7 MiB** | **51.1 MiB** | **90.9 MiB** |

Both stores represented 280.0 MiB of compiler outputs after eight revisions. Jato 0.3 used **51% less compressed payload**, and its SQLite files occupied 93.0 MiB compared with 190.2 MiB for 0.2. Both restored the first library byte-for-byte. This workload moves archive members between revisions and retains the same embedded table. [Revision sizes and restored archive digests](history-0.3.json)

## Small builds and edits

| Regex application | Cargo | Jato 0.2 | Jato 0.3 | kache 2.1.1 | kache 2.2.0 | sccache |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| First check | 1.76 s | 1.87 s | 1.79 s | 2.07 s | 2.24 s | 2.25 s |
| Clean-target check, warm store | 1.78 s | 67 ms | **64 ms** | 130 ms | 155 ms | 407 ms |
| Check after a new edit | 63 ms | 51 ms | **51 ms** | 129 ms | 130 ms | 84 ms |
| Check after undo | 63 ms | 27 ms | 28 ms | 93 ms | 72 ms | 76 ms |
| First release build | 6.62 s | 8.18 s | 6.89 s | 7.27 s | 6.85 s | 4.85 s |
| Clean-target release rebuild, warm store | 6.55 s | 98 ms | **69 ms** | 127 ms | 150 ms | 637 ms |

Packed Jato restored this release in 120 ms from 8.1 MiB of store files. The automatic mode used 34.4 MiB of file lengths, with reflink sharing on the measured volume. [Regex times, RSS and extents](regex-0.3.json)

## Native libraries and embedded data

| Warm clean-target workload | Cargo | Jato 0.2 | Jato 0.3 | kache 2.1.1 | kache 2.2.0 | sccache |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| System zlib, check | 1.25 s | 379 ms | **159 ms** | 182 ms | 198 ms | 481 ms |
| System zlib, release | 1.96 s | 896 ms | **149 ms** | 181 ms | 205 ms | 806 ms |
| 12 MiB embedded asset, check | 118 ms | 40 ms | **35 ms** | 99 ms | 119 ms | 129 ms |
| 12 MiB embedded asset, release | 465 ms | 467 ms | **38 ms** | 124 ms | 131 ms | 209 ms |

The zlib program compressed and decompressed its message after each release build. On this host, libz-sys passed native library declarations through Cargo. The embedded program read a runtime-selected byte of the generated data. Their first Jato release builds took 2.56 s and 616 ms, compared with 1.73 s and 470 ms for plain Cargo. [Zlib samples](zlib-0.3.json) · [Embedded data samples](embedded-0.3.json)

## Save to compiler feedback

| Watch defaults | Save to first rustc error | Fix to workspace success | Idle watcher RSS |
| --- | ---: | ---: | ---: |
| Jato 0.2 | 65.3 ms | 66.6 ms | 4.3 MiB |
| Jato 0.3 | **48.3 ms** | **48.8 ms** | 4.3 MiB |

Each version ran two eleven-edit sessions on the same two-member workload. Session order was 0.2, 0.3, 0.3, 0.2. With 0.3, the first diagnostic arrived in 45.6–63.9 ms across the 22 saves. [Raw watch sessions](feedback-0.3.json)

## Reproduce

Install Jato from this repository. Save a separate 0.2.0 binary to repeat the version comparison. The harness resolves the stock toolchain, uses private stores and peer daemon sockets, and exercises the normal Cargo compiler-wrapper interface. It uses the same absolute target path for each contender. Each tool, profile and trial starts with an empty store. The first build fills it, the warm build starts from a second empty target, and the fresh build retains that target. Tool order rotates and profile order alternates between trials.

```sh
python3 bench/compare.py --project /path/to/ripgrep-14.1.1 --verify-bin rg --previous-jato /path/to/jato-0.2.0 --include-packed --previous-packed --runs 3 --jobs 4
python3 bench/compare.py --fixture regex --previous-jato /path/to/jato-0.2.0 --include-packed --runs 3 --jobs 4
python3 bench/compare.py --fixture zlib --previous-jato /path/to/jato-0.2.0 --runs 3 --jobs 4
python3 bench/compare.py --fixture embedded --previous-jato /path/to/jato-0.2.0 --runs 3 --jobs 4
python3 bench/compare.py --project /path/to/ripgrep-14.1.1 --workloads check --previous-jato /path/to/jato-0.2.0 --paired-restores 21 --jobs 4
python3 bench/compare.py --tools jato kache --kache /path/to/kache-2.2.0/kache --project /path/to/ripgrep-14.1.1 --verify-bin rg --runs 3 --jobs 4
python3 bench/history.py --jato jato --previous-jato /path/to/jato-0.2.0 --variants 8
python3 bench/feedback.py --jato jato --runs 11
```

Use `--scratch` to select a filesystem. The comparison accepts `--offline` when the locked dependencies have already been fetched. Benchmark setup, target deletion, executable checks and daemon startup or shutdown are outside the timer. Clean refers to the target directory. The OS page cache stays enabled.

- The external project is the published ripgrep 14.1.1 crate with SHA-256 `f77b8032dc584527975f34aa5a897d0ef5a785573fda778771a614ff9da501d9`. The harness compares the release executable's version output after every build. [ripgrep source](https://crates.io/crates/ripgrep/14.1.1)
- The regex fixture checks a short expression and counts three matches. The zlib fixture uses flate2 and libz-sys to compress and decompress a message with the system library. Install zlib's development files and pkg-config to reproduce the system-library path. [Regex fixture](regex-app/) · [Zlib fixture](zlib-app/)
- The embedded-data fixture generates 12 MiB plus 17 bytes of deterministic data before timing. Its compiled program reads a runtime-selected byte. Each built-in fixture changes one line and undoes the edit after an unchanged check. [Embedded fixture](embedded-app/)
- The storage-history workload changes an exported symbol in a Rust library across eight revisions while retaining a 4 MiB data table. It builds the library in release mode, records packed storage after each revision, and verifies the SHA-256 of the restored first archive. [History harness](history.py)
- The feedback workload times a real compiler error after editing a workspace member, then times the successful workspace follow-up after fixing it. It also samples the idle watcher's resident memory. [Feedback harness](feedback.py)

Store bytes are apparent file lengths counted once per inode. Btrfs can compress data and share extents with the target directory. The harness flushes each private store after timing for the separate extent measurements. GNU time reports the largest process in the timed command tree. Peer servers and the compiler children they launch can run outside that tree. The comparison also records the private listening server's idle RSS. Jato's build cache exits after each build.

The released Jato executable's SHA-256 is `75ae82f142ef93b6a73d4ca6e464ed50c1ace3ac873106a62ae8c3e1dcb10711`. The comparison used the same kache, kache-engine, sccache and previous Jato executables recorded in the [0.2 measurements](0.2.md). The kernel was 7.3.4 and pkg-config reported system zlib `1.3.1.zlib-ng`.

The additional kache 2.2.0 comparison uses the published x86-64 Linux musl archive with SHA-256 `eeb6e72b9f55d0e9d61cae0789ffa8f3ab8c33bef166b7af655cc0806447ebac`, including its matching engine. Use `--kache` to select it and `--local-kache` to include another installed kache in the same fixture comparison. [kache 2.2.0 release](https://github.com/kunobi-ninja/kache/releases/tag/v2.2.0)
