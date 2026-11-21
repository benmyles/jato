# Measuring Rust compiler caches

These measurements compare Jato 0.2.0, the locally installed kache 2.1.1, upstream sccache 0.19.0 and plain Cargo. Each time includes Cargo startup and input validation. The machine ran Fedora 45 on an Intel Core Ultra 7 356H, Linux 7.3.4, Btrfs with Zstd level 1 compression, rustc and Cargo 1.100.0, and strace 7.3. Builds used four Cargo jobs. The results are medians from three interleaved trials on a shared development machine. [ripgrep samples](ripgrep-0.2.json) · [regex samples](regex-0.2.json) · [watch samples](feedback-0.2.json) · [Jato 0.1 report](0.1.md)

## ripgrep 14.1.1

| Workload | Cargo | Jato auto | Jato packed | kache | sccache |
| --- | ---: | ---: | ---: | ---: | ---: |
| First check | 5.92 s | 7.32 s | 5.17 s | 4.64 s | 4.73 s |
| Clean-target check, warm store | 5.00 s | **221 ms** | **211 ms** | 314 ms | 1.60 s |
| Unchanged check with an existing target | 60 ms | 53 ms | 43 ms | 81 ms | 70 ms |
| First release build | 28.04 s | 28.15 s | 30.19 s | 23.00 s | 13.74 s |
| Clean-target release rebuild, warm store | 25.68 s | **281 ms** | 408 ms | 341 ms | 4.78 s |
| Unchanged release build with an existing target | 67 ms | 40 ms | 44 ms | 94 ms | 71 ms |

Jato's warm release samples ranged from 275 to 282 ms. kache's ranged from 241 to 351 ms. Two of three head-to-head release trials favored Jato. First builds favored kache for check and sccache for release. Machine load varied, especially during cold compiles. Each tool counts compiler work differently. Individual timings and tool-reported hits are in the [raw measurements](ripgrep-0.2.json).

### Release cache space and memory

| Release workload | Store file lengths | Btrfs exclusive extents | Idle cache server RSS |
| --- | ---: | ---: | ---: |
| Jato auto | 160.7 MiB | 3.1 MiB | 0 MiB |
| Jato packed | 48.8 MiB | 48.8 MiB | 0 MiB |
| kache | 160.9 MiB | 0.7 MiB | 27.2 MiB |
| sccache | 29.2 MiB | 29.2 MiB | 19.8 MiB |

Automatic mode shared file extents on this compressed Btrfs volume. Packed mode reduced the release store's apparent length by 70% and restored in 408 ms. On the warm release build, the largest process in Cargo's command tree used 367.4 MiB with plain Cargo and 85.3 MiB with Jato auto. The raw samples also include cold and fresh RSS and the separately measured peer daemon RSS.

## Small builds and edits

The locked regex application has five library dependencies and one executable. The edit changes a string literal. Its undo returns to an earlier source version. Every check and edit uses the default dev profile, including Cargo's incremental setting.

| Workload | Cargo | Jato 0.2 auto | Jato 0.2 packed | kache | sccache | Jato 0.1 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| First check | 2.33 s | 2.34 s | 2.01 s | 2.55 s | 2.90 s | 2.25 s |
| Clean-target check, warm store | 2.07 s | **76 ms** | 82 ms | 143 ms | 487 ms | 140 ms |
| Check after an edit | 83 ms | **55 ms** | 57 ms | 131 ms | 83 ms | 95 ms |
| Check after undo | 79 ms | **31 ms** | 31 ms | 114 ms | 91 ms | 57 ms |
| First release build | 9.25 s | 8.73 s | 11.81 s | 8.60 s | 8.13 s | 7.56 s |
| Clean-target release rebuild, warm store | 10.00 s | **110 ms** | 142 ms | 162 ms | 821 ms | 968 ms |

Jato 0.2 used 19.9 MiB peak process RSS on the warm regex release build. The automatic store occupied 34.4 MiB of file lengths, with 0.8 MiB of exclusive Btrfs extents. The packed store occupied 12.9 MiB. [All edit and storage samples](regex-0.2.json)

## Save to compiler feedback

The watcher benchmark edits a member of a two-package workspace, times the first real rustc error, fixes the edit, then waits for the workspace check to finish. The table shows medians across nine edits.

| Watch configuration | Save to first diagnostic | Fix to workspace green | Idle watcher RSS |
| --- | ---: | ---: | ---: |
| Jato 0.2, default | **72.1 ms** | **80.8 ms** | 4.5 MiB |
| Jato 0.1, default | 189.7 ms | 261.8 ms | 4.1 MiB |
| Jato 0.1, explicit 16 ms debounce | 117.0 ms | 178.9 ms | 4.0 MiB |

The 0.2 diagnostic samples ranged from 69.5 to 74.5 ms. The equal-debounce comparison includes the compiler and watcher changes together. [All watch samples](feedback-0.2.json)

## Reproduce the comparison

Use Linux, GNU time, a C toolchain, strace and release builds of the three cache tools. The kache executable must support private local daemon sockets. The harness creates private Cargo configuration, caches and peer daemons, selects the stock toolchain and system C driver, and runs all contenders locally. It reuses downloaded registry sources. A run can also compare a saved older Jato binary and the portable packed store.

```sh
python3 bench/compare.py --jato jato --kache kache --sccache sccache --runs 3 --jobs 4 --include-packed
python3 bench/compare.py --project /path/to/ripgrep-14.1.1 --verify-bin rg --include-packed --runs 3 --jobs 4
python3 bench/feedback.py --jato jato --runs 9
```

All project builds share the same absolute target path and Cargo arguments. Each tool, profile and trial gets an empty private store. The cold build begins with an empty target. The warm build uses the same store and another empty target. The fresh build retains the target. For the locked regex fixture, a one-line change and its undo follow the fresh check. The harness executes each release binary, checks the fixture's answer, and compares ripgrep's version output across tools. Trial order rotates between tools and reverses between profiles. Peer daemon startup and shutdown happen outside the timer. Use `--offline` after fetching dependencies and `--scratch` to choose a disk for temporary files.

Add `--previous-jato /path/to/jato-0.1.0` to reproduce the previous-version column for the regex fixture. The watch comparison runs the feedback script with each version's defaults and again with `--debounce 16` on 0.1.0.

Each timing is in seconds in the raw comparison data. GNU time reports the maximum resident size of a process in the timed command tree. A peer server and compiler children it launches independently can live outside this tree. The harness measures the idle resident size of the private listening server separately. The Jato build cache exits after the build. The optional file watcher has its own measured idle footprint.

Store bytes count file lengths once per inode in the private store, including database files after daemon shutdown. Those lengths include extents shared with Cargo's target. The harness flushes the private store after timing and reports Btrfs extents and sharing separately. Filesystem compression also affects physical use.

The ripgrep input is the published 14.1.1 crate, SHA-256 `f77b8032dc584527975f34aa5a897d0ef5a785573fda778771a614ff9da501d9`. The regex input and its dependency lockfile ship with Jato. External projects retain their sources during the benchmark. [ripgrep 14.1.1](https://crates.io/crates/ripgrep/14.1.1) · [regex fixture](regex-app/)

<details>
<summary>SHA-256 of measured executables</summary>

| Executable | SHA-256 |
| --- | --- |
| Jato 0.2.0 | `f23ed2e4a7f8bac49dabadf102424458005fd16c36edb07d5e475bf633b91a2a` |
| Jato 0.1.0 | `d82d99d619b95178d1e3d4f302a641875ade3cd1fba7ad4f60156e2832c7d8c6` |
| kache 2.1.1 | `144e495945e1cc86f4223e6ef7182fb646be7db867010f07284e1b492c5a3979` |
| kache-engine | `6e234e3e6e21a3618231e6cf299eb84b8f55e353b8628ea7bfe5de748ba085b0` |
| sccache 0.19.0 | `0b44834be857e21b026f7db49a7762023fedb5eb5ed60725a66f698d8d79a34b` |

</details>
