# Jato 0.10 measurements

Benchmarks ran on a shared Intel Core Ultra 7 356H with Fedora 45, Linux 7.3.4 and compressed Btrfs. Each contender had a private artifact store, target directory and daemon socket. The machine's load varied substantially. The Olo table contains individual runs. [Previous release measurements](0.9.md)

## Fully rebuilding Olo

The workload built both `olo-protocol` and `olo-system` at Olo commit `cb35992a5c0b`. It used four Cargo jobs, the normal Olo profile, workspace feature unification, upstream mold 3.0.0 and the pinned `nightly-2026-11-14` with `-Zthreads=8`. Stores began empty with a 12 GiB budget. The second run removed both target and intermediate files. The third kept both. Each tool ran through the pinned Cargo binary with its compiler wrapper. An explicit empty workspace-wrapper override matched the earlier trials. Cargo can repeat compiler-info queries with that override.

| Olo | Jato 0.9.0 | Jato 0.10.0 | kache 2.3.0 |
| --- | ---: | ---: | ---: |
| Empty store and target | 289.14 s | 834.30 s | 313.60 s |
| Populated store, target and intermediates removed | 366.72 s | 188.92 s | 2.97 s |
| Already fresh | 0.72 s | 0.51 s | 0.42 s |
| Tool-reported hits during the full restore | 123 | 188 | 304 |

Jato 0.10 still bypassed 26 direct macro consumers and missed on 62 further units. It recompiled 26 units whose previously captured output was still awaiting producer evidence. It skipped recapturing those outputs. Eighteen of 20 sampled library or metadata files matched the cold build. The two recompiled Olo libraries varied. The full restore left 230 MiB charged to Jato's artifact budget. GNU time reported a maximum individual process RSS of 1.09 GiB, which includes rustc. An earlier development build took 344.50 s cold and 120.20 s to restore, with 180 hits. Workstation load and compiler parallelism make wall-time differences between these individual runs hard to assign to a particular change. [0.10 timings, counters, CPU, memory and digests](olo-0.10.json) · [0.9 and kache controls](olo-0.9.json)

The upstream kache **Cargo shim** also ran with its already populated private store, into a target path it had never seen. It took 5.55 s on the first delivery, 5.55 s after the target and build directory were removed, and 0.99 s with an existing target. The shim seeded 22 Cargo-fresh artifacts on each cleared build. An independent Olo run with kache 2.1.1 reported 2.0 s, versus 97–127 s with Jato 0.9, for `olo-system` alone. Olo's fully cleared rebuild continues to favor kache. [Shim samples](olo-0.10.json) · [How kache seeds targets](https://github.com/kunobi-ninja/kache/blob/v2.1.1/docs/getting-started/worktrees.mdx)

## Changing public target directories

A retained Cargo build directory lets Cargo preserve its own fingerprints, build-script results and incremental state. Olo had 276 Cargo-fresh artifacts when delivering to each new public target. The five executables and four sampled Rust artifacts all matched their primed digests.

| Elapsed time | New target A | New target B | Removed and recreated A | Existing A |
| --- | ---: | ---: | ---: | ---: |
| Jato 0.9, ordinary retained directory | 0.51 s | 0.67 s | 0.59 s | 0.65 s |
| kache 2.3, ordinary retained directory | 0.81 s | 0.70 s | 0.56 s | 0.53 s |
| Jato 0.10, Btrfs subvolume boundary | 0.83 s | 0.67 s | 0.72 s | 0.71 s |

Cargo hardlinked copies of `nm-fake` across the two same-filesystem targets. On opposite sides of a Btrfs subvolume boundary, the public targets had distinct inodes. Overwriting A in place preserved B and the next delivery to C. C took 0.78 s. The Olo trial reused an existing physical intermediate directory and put public output on a new subvolume. The CLI regression exercises newly managed intermediates on their own subvolume. [Delivery timings and byte checks](cargo-delivery-0.10.json) · [Cargo build cache](https://doc.rust-lang.org/cargo/reference/build-cache.html)

## Stable ripgrep

Three rotating trials built ripgrep 14.1.1 using Rust 1.100, identical development profile settings, four jobs and a compiler selected through PATH. Each executable ran after each build. The comparison included both the kache wrapper and its Cargo shim. Normal Cargo compiler-info caching was enabled.

| Median elapsed | Jato 0.10 | Jato 0.9 | kache wrapper | kache Cargo shim | sccache | Bare Cargo |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Cold | 11.84 s | 13.55 s | 9.59 s | 14.51 s | 8.94 s | 11.44 s |
| Full restore | 200 ms | 258 ms | 569 ms | 909 ms | 4.10 s | 11.60 s |
| Already fresh | 29 ms | 34 ms | 32 ms | 43 ms | 37 ms | 33 ms |

The Jato store occupied 108.4 MiB in logical files and 2.57 MiB of exclusive Btrfs extents while the target existed. The kache wrapper used 109.3 MiB logical and 0.66 MiB exclusive. sccache used 23.6 MiB logical. Median peak individual-process RSS during restore was 85 MiB with Jato and 34 MiB with kache. Jato had zero idle daemon RAM. The kache and sccache daemons each retained about 19 MiB RSS. These disk and memory metrics describe different resources and phases. [Trial results and filesystem accounting](named-ripgrep-0.10.json)

One separate ripgrep trial kept each contender's Cargo intermediates. After removing the delivery target, rebuilding took 29 ms with Jato, 31 ms with the kache wrapper, 40 ms with its Cargo shim, 28 ms with sccache and 30 ms with bare Cargo. The Jato trial's Cargo intermediate files totaled 143.2 MiB logical, separately from its compiler cache. [Retained-directory controls](retained-ripgrep-0.10.json)

## Concurrent Cargo jobs

Two concurrent builds selected separate packages and delivery targets in one workspace using `jato --keep-build`. Each build script slept for 800 ms. With default locking their script intervals had zero overlap in both trials. Olo's pinned Cargo with `-Zfine-grain-locking` overlapped them by 765 and 769 ms. Both delivered programs ran with their expected output. This exercises Cargo's scheduling behavior on a controlled fixture. [Timing and overlap samples](cargo-scheduling-0.10.json) · [Benchmark](cargo_scheduling.py)

We also timed five alternating Olo unit-graph constructions with nightly's experimental `-Zavoid-dev-deps`. The canonical graphs matched for all 304 units and dependency edges. Median graph generation was 0.57 s normally and 0.48 s with the flag. That flag changes how Cargo handles development dependency resolution and the lockfile. Jato leaves it to the caller. [Unit-graph samples](resolver-0.10.json)

## Compiler input reads and correctness

Jato authenticates the user-installed mold executable while hashing it for the native-input fingerprint. On 27 observed build-script links, one fewer 42,246,008-byte read per link avoids 1.06 GiB of logical reads. The same-size modified-linker regression still rejects publication. For a linked macro library, an external input was changed after cleaning the target. The library hit and its consumer generated the new value.

A separate serial Cargo test retained a 15.5 MiB dependency in the target and evicted the producer's Jato entry. The dependency was too old to have a publication in flight. On that consumer the 0.9 wrapper read 16,255,732 dependency bytes. The 0.10 wrapper read zero, and rustc produced metadata with the expected value. Source edits, failing consumers and reversions are separate cases in the harness. A 0.9 store also restored all 39 ripgrep units under 0.10. Its executable matched SHA-256 and passed 299 upstream tests. [Uncertified dependency trace](uncertified-extern-0.10.json) · [Cache migration and upstream tests](upstream-0.10.json)

For four unchanged consumers of an edited producer with an 8 MiB embedded asset, three iterations measured median successful checks of 317 ms for Jato, 485 ms for kache and 855 ms for sccache. Reverting the edit took 75, 227 and 210 ms respectively. A downstream type check verified each successful revision. A rejected revision produced the expected compiler error. [Dependency edit samples and wrapper byte traces](dependency-edits-0.10.json)

## Save-to-compiler feedback

The preceding release measured seven atomic break/fix cycles. Median save-to-error and fix-to-success were 46 ms each. [Watch measurements](0.9.md#save-to-compiler-feedback)

## Reproduction

Use prefetched dependencies and a disk-backed scratch directory. The head-to-head harness defaults to clearing Cargo intermediates along with the public target. Add `--retain-build-dir` to measure Cargo's retained state. Add `--empty-wrapper-overrides` to reproduce the explicit empty workspace wrapper used in earlier releases.

```sh
python3 bench/compare.py --offline --project /path/to/ripgrep-14.1.1 --verify-bin rg \
    --compiler-path path --workloads jato-dev --runs 3 --kache-cargo-shim
rustup run nightly-2026-11-14 python3 bench/compare.py --offline --project /path/to/olo \
    --package olo-protocol --package olo-system --tools jato kache --kache-cargo-shim \
    --workloads dev --cache-gib 12 --runs 1
python3 bench/dependency_edits.py --jato /path/to/jato --previous-jato /path/to/jato-0.9
rustup run nightly-2026-11-14 python3 bench/cargo_scheduling.py --jato /path/to/jato
```
