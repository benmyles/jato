# Jato 0.7 measurements

The installed Jato 0.7.0, Jato 0.6.0, upstream kache 2.2.0, upstream sccache 0.19.0 and bare Cargo built ripgrep 14.1.1 on a shared Intel Core Ultra 7 356H. The workstation ran Fedora 45, Linux 7.3.4, Rust 1.100.0 and compressed Btrfs. Cargo had four jobs. Each contender used private cache state and each peer daemon had a private socket. [0.6 report and packed-storage comparisons](0.6.md) · [kache release](https://github.com/kunobi-ninja/kache/releases/tag/v2.2.0) · [sccache release](https://github.com/mozilla/sccache/releases/tag/v0.19.0)

## Cold builds, fresh targets and restores

The two profiles use the same sources, target paths and toolchain. Every contender in the fast-profile group receives the settings used by the actual `jato dev build` command. The trials shared prefetched registry sources and the host’s filesystem page cache. Each trial starts with an empty store and target, runs the resulting `rg`, clears the target, rebuilds, runs `rg` again, and times a third build with Cargo's target already fresh. Contender order rotates. Times are medians of three trials per profile and contender.

| ripgrep with the jato-dev profile | Jato 0.7 | Jato 0.6 | kache | sccache | Bare Cargo |
| --- | ---: | ---: | ---: | ---: | ---: |
| Cold build | 8.52 s | 15.88 s | 7.12 s | 14.31 s | 7.51 s |
| Clean-target rebuild | 190 ms | 220 ms | 583 ms | 3.16 s | 6.32 s |
| Fresh target | 43 ms | 44 ms | 101 ms | 87 ms | 57 ms |
| Store file lengths | 108.4 MiB | 108.4 MiB | 109.3 MiB | 23.6 MiB | — |

| ripgrep with Cargo’s ordinary dev profile | Jato 0.7 | Jato 0.6 | kache | sccache | Bare Cargo |
| --- | ---: | ---: | ---: | ---: | ---: |
| Cold build | 17.15 s | 12.93 s | 11.60 s | 10.91 s | 9.64 s |
| Clean-target rebuild | 213 ms | 207 ms | 523 ms | 4.46 s | 9.10 s |
| Fresh target | 42 ms | 40 ms | 109 ms | 70 ms | 59 ms |
| Store file lengths | 204.2 MiB | 204.2 MiB | 203.5 MiB | 32.2 MiB | — |

Jato recorded 39 compiler hits in each fast-profile clean rebuild. Peak RSS of the largest process in Jato 0.7’s timed cold command went from 351 MiB to 322 MiB across the two profiles. The local store includes sealed files that can share extents with the Cargo target. Host load varied. The three Jato 0.7 cold observations spanned 8.64–29.82 s with ordinary dev and 6.41–12.00 s with the fast profile. The raw samples include every contender. [Cold trials, RSS and Btrfs extent counts](dev-profiles-ripgrep-0.7.json)

Four additional ordinary-dev cold runs per Jato version alternated with private empty stores. Jato 0.7 took 9.60 s and 24.44 CPU-seconds for the Cargo process tree. Jato 0.6 took 11.10 s and 24.70 CPU-seconds. [Alternating cold controls and CPU totals](cold-cpu-0.7.json)

The profile choice came from three rotating cold builds per configuration using the 0.6 engine. In the same named profile with otherwise inherited defaults, ripgrep took a median 12.77 s and produced 205 MiB of tracked artifacts. Using 16 codegen units and line tables throughout gave 8.26 s and 146 MiB. Omitting debug data for non-workspace dependencies gave 7.49 s and 109 MiB. Workspace code retains line tables for backtraces. Applications that need full variable debugging can use their ordinary Cargo profile. [Profile selection trials](profile-selection-0.7.json) · [Cargo profile settings](https://doc.rust-lang.org/cargo/reference/profiles.html)

## Live editor saves

The harness changes ripgrep’s entry point by atomic rename, makes seven different edits and returns to the original revision after each edit. Each program prints its expected revision after each build. All contenders keep their previous target and receive the same profile settings within a group. The table gives end-to-end medians.

| ripgrep live builds | Jato 0.7 | Jato 0.6 | kache | sccache | Bare Cargo |
| --- | ---: | ---: | ---: | ---: | ---: |
| Cargo dev, new edit | 579 ms | 579 ms | 2.49 s | 531 ms | 635 ms |
| Cargo dev, undo | 88 ms | 155 ms | 250 ms | 534 ms | 526 ms |
| jato-dev, new edit | 482 ms | 489 ms | 1.64 s | 493 ms | 491 ms |
| jato-dev, undo | 83 ms | 139 ms | 183 ms | 485 ms | 501 ms |

Jato 0.7 recorded seven verified live hits and zero rustc module-discovery probes for the seven undos in each profile. Jato 0.6 recorded seven hits and seven probes in each. The checked directory layout allows an atomic replacement of a fully verified source. Adding a competing module in an integration test still produced the expected compiler error. [Ordinary dev edits](dev-ripgrep-0.7.json) · [Fast-profile edits](fast-dev-ripgrep-0.7.json)

## Recording large outputs

A fresh release library embeds 32 MiB of deterministic incompressible data. A separate Linux syscall trace identifies the compiler wrapper by executable and crate name. It sums successful writes to the private store’s anonymous staging files and to the SQLite database and WAL. Both versions recorded the library.

| Cold packed capture, bytes written by the wrapper | Jato 0.7 | Jato 0.6 |
| --- | ---: | ---: |
| Anonymous staging files | 0 | 34,072,742 |
| SQLite database and WAL | 70,745,544 | 70,786,504 |
| Compressed payload retained | 34,049,095 | 34,049,102 |

New large raw frames can remain in the compiler output until publication. Jato rechecks each full frame before writing it to the store. SQLite still writes its database and transaction log. This measurement counts file writes and can differ from bytes written by the storage device.

A second test built a 34,009,880-byte executable and made seven distinct comment edits. The executable's complete SHA-256 stayed constant. Jato 0.7 matched 238,069,160 bytes to existing fs-verity objects. Each build ran a consumer that checked every embedded byte. Both versions restored the original library and executable after clearing their targets. Median end-to-end edit time was 516 ms with 0.7 and 623 ms with 0.6. [Capture timings, hashes and trace counts](capture-0.7.json)

The eight-revision packed histories retained 21.4 MiB for front insertions, 18.4 MiB for in-place edits and 15.2 MiB for Rust symbol edits in both versions. Each downstream consumer and original archive was verified. The insertion sequence used 1,449,440 Linux filesystem output blocks with 0.7 and 1,459,672 with 0.6. That sequence took 5.28 s and 5.68 s respectively. The zlib and embedded-asset fixtures also executed correctly on clean targets in automatic and packed modes. [Front insertions](insert-history-0.7.json) · [In-place edits](edit-history-0.7.json) · [Symbol changes](symbol-history-0.7.json) · [zlib](zlib-0.7.json) · [Embedded application](embedded-0.7.json)

## Worktrees and ordinary release builds

Three fast-profile trials alternated two physical checkouts of the locked regex application. Every restored program ran and its output was checked. A clean target in the other checkout took a median 102 ms with Jato 0.7, 171 ms with 0.6, 182 ms with kache and 3.44 s with sccache configured with all base directories. [Cold, same-target, new-target and copied-checkout trials](fast-worktrees-0.7.json)

Seven alternating ordinary-path clean ripgrep targets took 165 ms for a check and 304 ms for a release build with Jato 0.7. Jato 0.6 took 160 ms and 280 ms. The harness verified the executable after every release build. [Paired ordinary-path controls](paired-0.7.json)

## Save to compiler feedback

Eleven break/fix cycles in a two-package workspace took a median 61.7 ms to the first compiler error and 65.0 ms to a successful workspace check with ordinary watch. With `jato dev watch`, the corresponding medians were 59.3 ms and 62.3 ms. [Ordinary watch](feedback-0.7.json) · [Fast-profile watch](fast-feedback-0.7.json)

## Release qualification

The stable and minimum supported toolchains, Rust 1.100.0 and 1.89.0, each passed 55 tests with native hits, sealed storage and worktree operation required. The separate 272 MiB packed capture/restore stress test passed. Two other checkouts restored all 39 ripgrep compiler units, one from a populated 0.6 packed release cache and one from a new automatic-storage jato-dev cache. Both resulting binaries passed the 299 upstream integration tests. Eviction reclaimed all charged bytes in both stores. [Upstream results and binary digests](upstream-0.7.json)

## Reproduce

Prefetch the locked dependencies, install strace and bubblewrap, and put the upstream peer executables on PATH. All harnesses accept a disk-backed `--scratch` directory. The profile comparisons use the published ripgrep source archive. [ripgrep 14.1.1](https://crates.io/crates/ripgrep/14.1.1)

```sh
python3 bench/compare.py --offline --previous-jato /path/to/jato-0.6.0 \
    --project /path/to/ripgrep-14.1.1 --verify-bin rg --workloads dev jato-dev --runs 3
python3 bench/devloop.py --previous-jato /path/to/jato-0.6.0 --profile dev \
    --project /path/to/ripgrep-14.1.1 --source crates/core/main.rs --verify-bin rg --runs 7
python3 bench/devloop.py --previous-jato /path/to/jato-0.6.0 --profile jato-dev \
    --project /path/to/ripgrep-14.1.1 --source crates/core/main.rs --verify-bin rg --runs 7
python3 bench/capture.py --previous-jato /path/to/jato-0.6.0 --mib 32 --runs 7
python3 bench/history.py --previous-jato /path/to/jato-0.6.0 --variants 8 --insert-asset
python3 bench/compare.py --offline --tools jato --previous-jato /path/to/jato-0.6.0 \
    --project /path/to/ripgrep-14.1.1 --verify-bin rg --workloads dev --runs 4
python3 bench/compare.py --offline --previous-jato /path/to/jato-0.6.0 \
    --project /path/to/ripgrep-14.1.1 --verify-bin rg --paired-restores 7
python3 bench/worktrees.py --mode jato-dev --tools worktree kache sccache_basedirs --revisit --runs 3 \
    --previous-worktree /path/to/jato-0.6.0
python3 bench/feedback.py --runs 11
python3 bench/feedback.py --dev --runs 11
```
