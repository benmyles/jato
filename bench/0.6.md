# Jato 0.6 measurements

These tests ran the locally installed Jato 0.6.0 and compared it with Jato 0.5.0, upstream kache 2.2.0, upstream sccache 0.19.0 and Cargo without a wrapper. The machine was a shared Intel Core Ultra 7 356H running Fedora 45, Linux 7.3.4, Rust 1.100.0 and compressed Btrfs. Times include Cargo. Each tool had a private store and target, and the peer daemons had private sockets. Runs used four Cargo jobs unless indicated. [Previous release and concurrent-build results](0.5.md) · [kache release](https://github.com/kunobi-ninja/kache/releases/tag/v2.2.0) · [sccache release](https://github.com/mozilla/sccache/releases/tag/v0.19.0)

## Edits in a live incremental session

The harness builds revision A, runs Cargo again with the target fresh, then makes seven distinct edits and seven undos to A. It saves the source by rename, keeps each target and its incremental state, and rotates contender order each round. A branch in the application prints a different value for each revision. The harness executes every binary and checks the value outside the timer. The timings below are medians for edits and undos. Cold and fresh builds are single observations per contender.

| ripgrep 14.1.1, dev profile | Jato 0.6 | Jato 0.5 | kache | sccache | Bare Cargo |
| --- | ---: | ---: | ---: | ---: | ---: |
| Cold build | 11.05 s | 9.65 s | 6.74 s | 7.14 s | 8.83 s |
| Fresh target | 39 ms | 43 ms | 83 ms | 64 ms | 62 ms |
| New edit | 553 ms | 688 ms | 2.84 s | 566 ms | 578 ms |
| Undo | **161 ms** | 591 ms | 270 ms | 604 ms | 606 ms |

Jato restored all seven undos from the executable recorded during the first build. rustc compiled each new edit with the live incremental state. kache also reported seven undo hits. The cold build includes recording and validating the first outputs. [ripgrep samples and hit counters](dev-ripgrep-0.6.json)

| Locked regex application, dev profile | Jato 0.6 | Jato 0.5 | kache | sccache | Bare Cargo |
| --- | ---: | ---: | ---: | ---: | ---: |
| Cold build | 9.12 s | 7.04 s | 3.29 s | 4.70 s | 4.63 s |
| Fresh target | 26 ms | 35 ms | 66 ms | 48 ms | 44 ms |
| New edit | 213 ms | 212 ms | 629 ms | 266 ms | 238 ms |
| Undo | **74 ms** | 209 ms | 193 ms | 252 ms | 221 ms |

Both Jato releases had similar new-edit latency on the smaller application. On this machine the 0.6 executable restore took 38% of kache’s undo time and 29% of sccache’s. [Regex samples and hit counters](dev-regex-0.6.json)

## Error feedback with a large tracked input

A dependency-free library includes a 64 MiB file. It builds successfully once. An edit to its Rust source then introduces a compile error. Both versions returned Cargo’s failure status and the expected compiler diagnostic. The benchmark uses one Cargo job, alternates nine untraced edits, and traces a separate check per version to count only the wrapper’s reads of the asset.

| Failed release check | Jato 0.6 | Jato 0.5 |
| --- | ---: | ---: |
| Asset bytes read by the wrapper | **3,072** | 67,108,864 |
| End-to-end median | **385 ms** | 438 ms |

rustc still processes its own inputs. The I/O reduction applies to the cache lookup. An integration test changes a byte outside the sample, verifies the new value came from compilation, and restores the original program after an undo. [Timing and byte counts](input-reads-0.6.json)

## Storage across artifact revisions

The fixture builds a Rust library that embeds a deterministic, incompressible 16 MiB asset. Seven revisions insert bytes 8 KiB into the asset. For each contender a separate downstream Rust program links every revision and checks every embedded byte. After the target is removed, the first archive must match its original SHA-256 and the consumer runs again. Every tool reported a cache hit on that restore. kache’s original archive had a different digest from the others and restored its own original exactly.

| Eight insertion revisions | Jato 0.6 packed | Jato 0.5 packed | kache | sccache |
| --- | ---: | ---: | ---: | ---: |
| Store file lengths | **23.0 MiB** | 258.2 MiB | 384.2 MiB | 384.1 MiB |
| Btrfs exclusive extents | 23.0 MiB | 258.2 MiB | 336.3 MiB | 384.1 MiB |
| Sum of eight Cargo build times | 5.39 s | 6.45 s | 5.13 s | 4.94 s |
| Clean-target restore | 99 ms | 180 ms | 163 ms | 127 ms |

File lengths count each store inode once after closing the private daemons. The restored Cargo target was still present. Btrfs reported 48.0 MiB of shared extents in kache’s store. The comparison uses `JATO_STORAGE=packed`. All contenders build the library and run the downstream checks at the same paths. Build and restore times come from one sequence per tool and exclude consumer verification. [All revision samples, hit counts, extent sizes and archive digests](insert-history-0.6.json)

| Packed payload after eight revisions | Jato 0.6 | Jato 0.5 |
| --- | ---: | ---: |
| Insert bytes near the beginning of a 16 MiB asset | 21.4 MiB | 256.0 MiB |
| Change a byte in place in a 16 MiB asset | 18.4 MiB | 37.3 MiB |
| Change a Rust symbol beside a 4 MiB asset | 15.2 MiB | 90.9 MiB |

Across the insertion builds Jato’s filesystem output fell from 2,927,800 to 1,465,736 Linux output blocks. The in-place and symbol workloads also verified all eight downstream programs and the restored original. [In-place edits](edit-history-0.6.json) · [Symbol edits](symbol-history-0.6.json)

A separate store stress test wrote two revisions of a 272 MiB sparse artifact, spilled its index of distinct chunks to disk, restored the original and checked its digest. The unoptimized test process peaked at 8,396 KiB RSS. The embedded and zlib release fixtures also executed their restored binaries in automatic and packed mode. [Stress-test resource usage](packed-spill-0.6.json) · [Embedded release](embedded-0.6.json) · [zlib release](zlib-0.6.json)

## System files in projected builds

A warm packed ripgrep release restored all 39 compiler units with both versions. Under a private rootless mount, 0.5 made 189 connections to the host to verify system-file ownership. The 0.6 wrappers needed 33. The supervisor can return up to 32 previously authenticated inode identities, and each client checks its actual file's identity before using one. A separate checkout reused the migrated 0.5 cache and the restored binary passed all 299 upstream integration tests. [Connection counts and executable digest](worktree-owners-0.6.json)

## Clean targets and concurrent builds

Five trials alternated the original ripgrep checkout, a new target and another checkout. Jato used its projected worktree mode. sccache received all source and target roots through `SCCACHE_BASEDIRS` before the server started. Every release binary was run with `--version`.

| Median warm ripgrep release | Jato 0.6 | Jato 0.5 | kache | sccache |
| --- | ---: | ---: | ---: | ---: |
| Original checkout, clean target | 211 ms | 213 ms | 344 ms | 4.78 s |
| Original checkout, new target | 207 ms | 214 ms | 379 ms | 15.63 s |
| Other checkout, empty target | 211 ms | 235 ms | 376 ms | 18.06 s |

The cold priming builds took 39.61 s, 28.02 s, 39.36 s and 18.86 s in table order. These are single observations on the shared machine. Seven alternating trials on ordinary physical paths put 0.6 and 0.5 at 178 ms and 176 ms for a warm release, and 161 ms and 156 ms for a warm check. [Worktree trials, cold builds and hits](worktree-revisits-0.6.json) · [Ordinary-path paired controls](paired-0.6.json)

Two simultaneous clean release builds of the regex application started from one empty store and independent checkouts. Each trial used four jobs per checkout, included the private servers and their descendants in resource accounting, and executed both binaries. The table shows medians over three trials.

| Two cold regex builds | Jato 0.6 | Jato 0.5 | kache | sccache |
| --- | ---: | ---: | ---: | ---: |
| Time to both executables | 7.14 s | 7.93 s | 10.38 s | 8.51 s |
| Total CPU-seconds | 21.78 | 23.55 | 24.62 | 52.08 |
| Peak sampled process-tree PSS | 380 MiB | 366 MiB | 499 MiB | 820 MiB |
| Store file lengths | 34.4 MiB | 34.4 MiB | 33.9 MiB | 20.0 MiB |

Jato compiled six units and restored six counterparts in each trial. The recorded Jato 0.6 wall times include a 12.11 s trial. sccache had the smallest store on this workload. [Concurrent timings and memory samples](concurrent-regex-0.6.json)

## Save to compiler feedback

In eleven break/fix cycles on a two-package workspace, `jato watch` delivered the first compiler error in a median 53.1 ms and the next successful full workspace check in 53.9 ms. The idle process tree used 2,265 KiB of PSS. [Watch samples](feedback-0.6.json) · [Large-tree measurements from 0.5](0.5.md#save-to-compiler-feedback)

## Reproduce

Prefetch locked dependencies, install strace and bubblewrap for native and worktree tests, and place the upstream peer binaries on PATH. The harnesses accept explicit executable paths and a disk-backed scratch directory. Use an unpacked copy of ripgrep from the published source archive. [ripgrep 14.1.1](https://crates.io/crates/ripgrep/14.1.1)

```sh
python3 bench/devloop.py --previous-jato /path/to/jato-0.5.0 --runs 7 \
    --project /path/to/ripgrep-14.1.1 --source crates/core/main.rs --verify-bin rg
python3 bench/devloop.py --previous-jato /path/to/jato-0.5.0 --runs 7
python3 bench/input_reads.py --previous-jato /path/to/jato-0.5.0 --runs 9
python3 bench/history.py --previous-jato /path/to/jato-0.5.0 --variants 8 --insert-asset --kache kache --sccache sccache
python3 bench/history.py --previous-jato /path/to/jato-0.5.0 --variants 8 --edit-asset
python3 bench/history.py --previous-jato /path/to/jato-0.5.0 --variants 8
python3 bench/worktrees.py --tools worktree kache sccache_basedirs --previous-worktree /path/to/jato-0.5.0 \
    --project /path/to/ripgrep-14.1.1 --verify-bin rg --revisit --runs 5
python3 bench/compare.py --offline --previous-jato /path/to/jato-0.5.0 --paired-restores 7 \
    --project /path/to/ripgrep-14.1.1 --verify-bin rg
python3 bench/concurrent.py --previous-jato /path/to/jato-0.5.0 --copies 2 --jobs 4 --runs 3
python3 bench/feedback.py --runs 11
```

To count host connections separately from build latency, prime a projected release, run `jato worktree --root . cargo clean`, then run `strace -f -qq -e trace=connect -o owners.trace jato worktree --root . --system-linker build --release --locked -q`. Count lines containing `jato-view-` and check the difference in compiler hits with `jato stats --json`.
