# Jato 0.9 measurements

The installed Jato 0.9.0, Jato 0.8.0, kache 2.3.0, sccache 0.19.0 and bare Cargo ran on a shared Intel Core Ultra 7 356H, Fedora 45, Linux 7.3.4 and compressed Btrfs. Trials used private stores, targets and daemon sockets with prefetched dependencies. Kernel page cache and machine load were shared. [0.8 storage and worktree measurements](0.8.md) · [kache 2.3.0](https://github.com/kunobi-ninja/kache/releases/tag/v2.3.0) · [sccache 0.19.0](https://github.com/mozilla/sccache/releases/tag/v0.19.0)

## Olo with split Rust metadata and mold

Built Olo commit `cb35992a5c0b` with its pinned `nightly-2026-11-14`, normal development profile, workspace feature unification, mold flags and eight rustc front-end threads. One Cargo invocation selected both `olo-protocol` and `olo-system` with four jobs. Each contender started with an empty 12 GiB-budget store. The second build removed the entire target, including Cargo’s intermediate directory. The third kept the target. Each cell is one observation.

| Olo | Jato 0.9 | kache 2.3 |
| --- | ---: | ---: |
| Cold | 289.14 s | 313.60 s |
| Clean-target rebuild | 366.72 s | 2.97 s |
| Already fresh | 0.72 s | 0.42 s |
| New hits on the clean rebuild | 123 | 304 |

Jato’s warm build still compiled 127 units after lookup and bypassed another 26 with opaque macro dependencies. It delegated native links through the user-installed mold. Cargo changed some just-produced dependencies during downstream compilation. These prevent reuse even when the source checkout stays fixed. Eighteen of 20 sampled files matched their cold SHA-256 with Jato. The two independently recompiled Olo rlibs differed. All 20 matched with kache. The Olo protocol library subsequently passed 75 tests with Jato in an isolated environment. An independent Olo run with a working checkout also favored kache, with 97–127 s for Jato’s two rebuilds and 2.0 s for kache 2.1.1. [Measured builds, file digests and counters](olo-0.9.json)

## Rust compiler discovery and ripgrep

Ran the published ripgrep 14.1.1 project on stable Rust 1.100 with identical jato-dev settings for every contender. The first three rows are medians from three rotating trials where Cargo resolved rustc on PATH. The last two rows come from seven atomic edits and undos with an absolute compiler. Every built executable ran, including a revision-specific behavior check after each edit and undo. [Cold, warm and fresh samples](named-ripgrep-0.9.json) · [Live edit samples](dev-ripgrep-0.9.json)

| ripgrep | Jato 0.9 | Jato 0.8 | kache 2.3 | sccache 0.19 | Bare Cargo |
| --- | ---: | ---: | ---: | ---: | ---: |
| Cold build | 4.56 s | 4.61 s | 4.18 s | 4.30 s | 4.07 s |
| Clean-target rebuild | 146 ms | 252 ms | 319 ms | 1,623 ms | 4,147 ms |
| Already fresh | 33 ms | 57 ms | 79 ms | 52 ms | 48 ms |
| New edit | 463 ms | 470 ms | 1,558 ms | 467 ms | 463 ms |
| Undo | 65 ms | 63 ms | 159 ms | 459 ms | 484 ms |

Jato had zero idle cache-daemon RSS. kache and sccache had 18,744 and 17,336 KiB respectively. Median store file lengths were 108.4 MiB for Jato, 109.3 MiB for kache and 23.6 MiB for sccache. Sealed Btrfs files can share physical extents with target files. [Resource and extent measurements](named-ripgrep-0.9.json)

A serial one-library check under 0.8 launched four extra rustc processes to query the sysroot. Version 0.9 used one on the first build and zero on restore. With a rustup proxy on PATH, 0.9 still asks rustup which compiler it selected. Changing the project toolchain file or setting an environment override returned the corresponding compiler's exact version output. In five paired ripgrep runs the absolute-compiler dev restore was 212 ms with 0.9 and 206 ms with 0.8. With an explicit rustup proxy the corresponding restores were 1.83 s and 1.91 s. [Discovery traces and toolchain switches](discovery-0.9.json) · [Absolute compiler](absolute-ripgrep-0.9.json) · [Rustup proxy](proxy-ripgrep-0.9.json)

## Waiting for pipelined dependencies

A real Cargo library published into three private SQLite fixtures. The larger fixtures contained 16,384 synthetic outstanding dependency records. The last also contained a chain of 64 entries whose ultimate producer was that real library. The harness alternated binaries for seven trials, verified the real rmeta by SHA-256, compiled an independent consumer, and checked the final number of outstanding dependencies. Synthetic entries supplied graph structure only. [Raw samples and check configuration](publication-0.9.json)

| Median publication time | Jato 0.9 | Jato 0.8 |
| --- | ---: | ---: |
| Empty graph | 0.51 ms | 0.54 ms |
| Unrelated pending records | 2.21 ms | 8.08 ms |
| Release the dependent chain | 3.46 ms | 283.64 ms |

End-to-end Cargo check times for the chain were 57 ms and 340 ms respectively. The reverse index visits actual consumers and a transaction-local SQLite table holds the work queue. Metadata-only producers can discharge only metadata requirements. [Benchmark implementation](publication.py)

## Save-to-compiler feedback

Seven two-package break/fix cycles took a median 45.8 ms from save to compiler error and 45.5 ms from fix to a successful workspace check. The idle watcher tree used 2,197 KiB PSS. A separate regex run exercised ordinary cold, restored, fresh and edited builds. [Watch samples](feedback-0.9.json) · [Ordinary Cargo control](regex-0.9.json)

## Reproduction

Use prefetched dependencies and a disk-backed scratch directory. Run the Olo comparison on its pinned nightly with the existing C driver and mold on PATH. The native macro and assembly boundaries affect which workloads can hit.

```sh
python3 bench/compare.py --offline --project /path/to/ripgrep-14.1.1 --verify-bin rg \
    --previous-jato /path/to/jato-0.8 --compiler-path path --workloads jato-dev --runs 3
python3 bench/compare.py --offline --project /path/to/olo --package olo-protocol --package olo-system \
    --tools jato kache --workloads dev --cache-gib 12 --runs 1
python3 bench/publication.py --previous-jato /path/to/jato-0.8 --pending 16384 --depth 64 --runs 7
python3 bench/devloop.py --previous-jato /path/to/jato-0.8 --profile jato-dev \
    --project /path/to/ripgrep-14.1.1 --source crates/core/main.rs --verify-bin rg --runs 7
python3 bench/feedback.py --dev --runs 7
```

A 0.8 store migrated from schema 15 to 17 and restored all 39 ripgrep units. The restored binary matched the cold SHA-256 and passed all 299 upstream integration tests. A separate two-crate trace distinguished cached Cargo stderr from actual new compiler hits. [Upgrade and upstream checks](upstream-0.9.json) · [Debug replay counts](debug-replay-0.9.json)
