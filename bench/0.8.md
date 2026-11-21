# Jato 0.8 measurements

Jato 0.8.0, Jato 0.7.0, kache 2.2.0, sccache 0.19.0 and bare Cargo ran on a shared Intel Core Ultra 7 356H with Fedora 45, Linux 7.3.4, Rust 1.100.0 and compressed Btrfs. Trials used four Cargo jobs unless stated otherwise. Tool binaries, daemon sockets, configuration, stores and targets were isolated. Dependencies were prefetched. The filesystem page cache and host load were shared. [0.7 measurements](0.7.md) · [kache release](https://github.com/kunobi-ninja/kache/releases/tag/v2.2.0) · [sccache release](https://github.com/mozilla/sccache/releases/tag/v0.19.0)

## Keeping revisions of a large artifact

A Rust library embeds a deterministic, incompressible 16 MiB file. Seven edits insert bytes near its beginning. A separate Rust program links every revision and checks each embedded byte. After removing the target, the harness restores the original archive and then the final edited archive. Both match their original SHA-256 and both downstream programs run again. The table uses automatic storage for Jato. Timings are single observations per revision or restore and exclude downstream checks.

| Eight insertion revisions | Jato 0.8 | Jato 0.7 | kache 2.2 | sccache 0.19 |
| --- | ---: | ---: | ---: | ---: |
| Store file lengths | 49.3 MiB | 385.1 MiB | 384.2 MiB | 384.1 MiB |
| Btrfs exclusive extents | 49.3 MiB | 337.1 MiB | 336.3 MiB | 384.1 MiB |
| Time for all eight Cargo builds | 4.94 s | 4.82 s | 7.18 s | 6.48 s |
| Restore original library | 45 ms | 52 ms | 206 ms | 160 ms |
| Restore edited library | 86 ms | 40 ms | 213 ms | 174 ms |

Jato retained one complete authenticated base per large output and 14 patches totalling 90,695 bytes. All four tools reported a hit for both archive restores. A full sealed copy in 0.7 gave the fastest edited restore on this insertion workload. Btrfs extent measurements were taken with the most recently restored Cargo target still present. [Builds, restores, extents and checksums](auto-insert-0.8.json)

With a one-byte overwrite in place per revision, 0.8 retained 49.2 MiB and 0.7 retained 385.1 MiB. A separate syscall trace of the last 0.8 restore counted 2,097,152 bytes read from staged outputs to verify the changed BLAKE3 stripes. The last edited archive restored in 37 ms with 0.8 and 38 ms with 0.7. [In-place revision history and staged reads](auto-edit-0.8.json)

The base/patch representation is available on supported compressed Btrfs mounts with kernel fs-verity. In portable packed mode the same eight insertions retained 22.7 MiB of store files with 0.8. Its final edited archive also passed the downstream check. [Packed insertions](insert-history-0.8.json) · [Packed overwrites](edit-history-0.8.json) · [Packed symbol changes](symbol-history-0.8.json)

## Edits across a dependency graph

The producer exports an 8 MiB embedded constant. Four consumers depend on its metadata. Each of seven rounds changes a producer constant, makes a second change that must fail in a consumer, and reverts to the original revision. The consumer source stays fixed. Tool order rotates. Outside the timer, rustc checks an expected type from every successful consumer. Each failure must include its expected compiler diagnostic. Cold checks are single observations.

| Producer and four consumers | Jato 0.8 | Jato 0.7 | kache 2.2 | sccache 0.19 | Bare Cargo |
| --- | ---: | ---: | ---: | ---: | ---: |
| Initial check | 405 ms | 457 ms | 533 ms | 1.02 s | 327 ms |
| New successful edit | 377 ms | 451 ms | 525 ms | 805 ms | 326 ms |
| Edit producing a Rust error | 307 ms | 427 ms | 494 ms | 736 ms | 294 ms |
| Undo to first revision | 84 ms | 82 ms | 275 ms | 329 ms | 289 ms |

Separate one-job traces followed one Jato consumer wrapper. For a successful change it read 32,516,654 producer metadata bytes with 0.8 and 65,021,020 with 0.7. On the failing change those counts were 3,072 and 32,510,510. These counts exclude rustc’s own input reads. A successful new variant still required a complete dependency digest before admission. [Every check, compiler hit and read count](dependency-edits-0.8.json)

## Packed publication and restoration

One release library embeds a 32 MiB incompressible asset. A second uses the same length with sparse data and a distinct marker per MiB. A separate syscall trace follows only the wrapper to count bytes written. Nine untraced clean targets per tool alternate order, and the harness compares each restored archive to its original SHA-256.

| 32 MiB embedded-asset fixture | Jato 0.8 | Jato 0.7 |
| --- | ---: | ---: |
| SQLite database and WAL writes, random asset | 168,920 B | 70,680,008 B |
| New frame-file writes, random asset | 34,048,316 B | 0 B |
| Warm rebuild, random asset | 128 ms | 196 ms |
| Target writes on sparse restore | 400,886 B | 100,670,993 B |
| Warm rebuild, sparse asset | 102 ms | 127 ms |

The 0.8 sparse artifact set contained 100,670,966 logical bytes. Kernels and filesystems can compress, coalesce, or share those writes. The byte counts come from the wrapper’s write calls. Both fixtures also made nine comment-only revisions of a native executable, ran the program against the actual asset and restored the original. [Incompressible input](packed-io-0.8.json) · [Sparse input](sparse-restore-0.8.json)

## Ordinary development on ripgrep

Each tool built the published ripgrep 14.1.1 workspace with the same jato-dev Cargo profile. Three independent cold stores per contender supplied the cold, clean-rebuild and fresh-target medians. Every built executable ran. A separate live incremental session timed seven distinct atomic edits and seven undos, and checked a revision-specific value after every build.

| ripgrep with jato-dev | Jato 0.8 | Jato 0.7 | kache 2.2 | sccache 0.19 | Bare Cargo |
| --- | ---: | ---: | ---: | ---: | ---: |
| Cold build | 10.66 s | 10.70 s | 7.29 s | 8.68 s | 5.69 s |
| Clean-target rebuild | 215 ms | 192 ms | 587 ms | 2.72 s | 7.53 s |
| Already-fresh target | 48 ms | 39 ms | 101 ms | 70 ms | 61 ms |
| New edit | 507 ms | 490 ms | 2.08 s | 524 ms | 534 ms |
| Undo | 81 ms | 72 ms | 187 ms | 513 ms | 565 ms |
| Store file lengths | 108.4 MiB | 108.4 MiB | 109.3 MiB | 23.6 MiB | — |

Host load varied. Jato’s three cold observations spanned 6.16 to 12.56 seconds. Bare Cargo finished the cold ripgrep build fastest. Among the caches, kache led cold and sccache retained the smallest store. The store-length row includes sealed files that can share physical extents with Cargo targets. [Cold, warm and fresh controls](dev-profiles-ripgrep-0.8.json) · [Live edits and undos](dev-ripgrep-0.8.json)

In seven alternating physical-path ripgrep restores, an ordinary check took 147 ms with 0.8 and 140 ms with 0.7. Release restores took 207 ms and 217 ms respectively. The native zlib and embedded-data fixtures ran their restored release binaries in automatic and packed mode. [Paired controls](paired-0.8.json) · [zlib](zlib-0.8.json) · [Embedded data](embedded-0.8.json)

## Concurrent builds and worktrees

Three fast-profile regex trials alternated two physical checkouts and empty targets. Returning to the other checkout took 83 ms with 0.8, 104 ms with 0.7, 143 ms with kache and 2.93 s with sccache configured with both base directories. [Worktree trials](fast-worktrees-0.8.json)

Two trials started two release builds of the regex application together from independent empty checkouts and one empty store. CPU and sampled proportional memory include private daemons and their descendants. Both executables ran after every pair. [Concurrent samples](concurrent-regex-0.8.json)

| Two simultaneous cold regex checkouts | Jato 0.8 | Jato 0.7 | kache 2.2 | sccache 0.19 |
| --- | ---: | ---: | ---: | ---: |
| Time to both release executables | 9.72 s | 9.42 s | 9.02 s | 9.06 s |
| Total CPU-seconds | 24.93 | 23.48 | 24.18 | 51.93 |
| Peak sampled process-tree PSS | 531.3 MiB | 436.0 MiB | 470.2 MiB | 960.6 MiB |
| Store file lengths | 34.5 MiB | 34.4 MiB | 33.9 MiB | 20.0 MiB |

## Save to compiler feedback

Across nine two-package break/fix cycles, `jato dev watch` reported the first Rust error in a median 51.9 ms and a successful workspace check in 51.0 ms. The idle process tree used 2,174 KiB PSS. [Watch samples](feedback-0.8.json)

## Release qualification

Rust 1.89.0 and 1.100.0 each passed 64 tests with native hits, sealed storage and projected worktrees required. The separate 272 MiB packed stress test restored the original after two recorded revisions and peaked at 8,848 KiB RSS in the unoptimized test process. [Stress-test resource usage](packed-spill-0.8.json)

An upgraded 0.7 packed release cache and a new automatic-storage development cache each restored all 39 ripgrep compiler units in another checkout. The latter restored an edited executable recorded as a patch. Each binary passed all 299 upstream integration tests. On a private ext4 image mounted with fuse2fs, automatic storage used file-backed packed frames, restored the byte-identical rlib across filesystems and reclaimed all charged bytes. [Upstream hashes and storage checks](upstream-0.8.json)

## Reproduce

Use an unpacked copy of the published ripgrep workspace and prefetched locked dependencies. Put the upstream tools on PATH or pass their paths explicitly. Native tests use strace. Projected worktrees use bubblewrap. The harnesses accept a disk-backed `--scratch` directory. [ripgrep 14.1.1](https://crates.io/crates/ripgrep/14.1.1)

```sh
python3 bench/history.py --previous-jato /path/to/jato-0.7.0 --storage auto --variants 8 \
    --insert-asset --trace-edited --kache kache --sccache sccache
python3 bench/history.py --previous-jato /path/to/jato-0.7.0 --storage auto --variants 8 --edit-asset --trace-edited
python3 bench/dependency_edits.py --previous-jato /path/to/jato-0.7.0 --mib 8 --clients 4 --runs 7
python3 bench/capture.py --previous-jato /path/to/jato-0.7.0 --mib 32 --runs 9
python3 bench/capture.py --previous-jato /path/to/jato-0.7.0 --mib 32 --runs 9 --sparse
python3 bench/compare.py --offline --previous-jato /path/to/jato-0.7.0 \
    --project /path/to/ripgrep-14.1.1 --verify-bin rg --workloads jato-dev --runs 3
python3 bench/devloop.py --previous-jato /path/to/jato-0.7.0 --profile jato-dev \
    --project /path/to/ripgrep-14.1.1 --source crates/core/main.rs --verify-bin rg --runs 7
python3 bench/compare.py --offline --previous-jato /path/to/jato-0.7.0 \
    --project /path/to/ripgrep-14.1.1 --verify-bin rg --paired-restores 7
python3 bench/worktrees.py --mode jato-dev --tools worktree kache sccache_basedirs \
    --previous-worktree /path/to/jato-0.7.0 --revisit --runs 3
python3 bench/concurrent.py --previous-jato /path/to/jato-0.7.0 --copies 2 --runs 2
python3 bench/feedback.py --dev --runs 9
```
