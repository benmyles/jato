# Work journal

## 2026-11-21 — cold capture and editor saves

- Added per-unit compile, validation and publication timings. On an isolated fresh incremental regex link, six runs put the bare compiler at 198 ms and its native observer at 479 ms. Switching from hexadecimal to printable trace strings took 541 ms.
- A warm undo in the 0.6 live benchmark launched one additional rustc dependency probe on each atomic save. Packed capture also spooled new incompressible frames before writing the same bytes to SQLite.
- Recorded a bounded directory layout for eligible incremental native units. The atomic-save undo and subsequent clean restore both hit, and adding a competing module still produced rustc error E0761. The undo used no discovery probe.
- Reduced a new 256 KiB incompressible frame to a 40-byte staging descriptor. Publication rechecks the entire frame. Sealed capture can reuse a fully matched existing object even near the storage budget. Exercised changes outside the sample and eviction of the last reference before publication.
- Compared four Cargo profile configurations in three rotating cold ripgrep builds each. Selected 16 codegen units, workspace line tables and dependency debug level zero for the explicit `jato dev` command. It inherits the project’s dev settings, and Cargo keeps a separate target profile. Verified explicit release selection, argument boundaries, test execution and watch feedback.
- Traced the installed release on a cold 32 MiB packed asset. Anonymous staging writes fell from 34,072,742 bytes to zero. Seven byte-identical executable captures reused 238,069,160 bytes of authenticated objects. Downstream executables checked all embedded bytes, and both tools restored their originals.
- Passed 55 tests on Rust 1.89 and 1.100 with native, sealed and worktree reuse required, plus the separate 272 MiB spill test. A migrated 0.6 release and a newly populated jato-dev cache each restored 39 ripgrep units in another checkout and passed all 299 upstream integration tests. Historical storage schemas reclaimed all charged bytes after GC.

## 2026-11-21 — live incremental replay and shifted artifacts

- A dev-profile edit and undo both bypassed the installed 0.5 cache because rustc’s incremental session was active. The original executable had already been recorded from a clean session.
- Inserted one byte per revision near the front of a 16 MiB embedded asset. Four releases grew from 32.0 to 128.0 MiB of packed payload, with no prior frames reused across those revisions.

- Moved the live incremental bypass behind verified native lookup. An atomic-save undo restored the clean-session executable and a subsequent new edit still used rustc’s live state. Fresh-session observation remains required for publication.
- Applied FastCDC 2020 inside long packed archive regions while keeping a single 256 KiB input window. Across eight insertions the payload fell from 256.0 MiB to 21.4 MiB. The original archives matched by SHA-256, and the 272 MiB spill/restore test still passed.

- Traced a failed Cargo check after a source edit beside a 64 MiB asset. The 0.5 wrapper read all 67,108,864 asset bytes before invoking rustc. A bounded rejection check read 3,072. An edit outside those reads still produced a new executable, and a byte-for-byte undo restored the original. Tested mixed legacy entries, sample accounting and eviction.

- A warm projected ripgrep build made 189 host ownership connections. Returning up to 32 already authenticated system inode identities on a wrapper’s first successful query reduced that to 33. Both traces recorded 39 verified compiler hits.
- Installed 0.6.0 and ran 50 tests each on Rust 1.89 and 1.100 with native, sealed and worktree hits required. The separate 272 MiB packed stress test passed. Upgraded nine populated historical schemas, restored a 0.5 packed ripgrep cache, wrote a new variant and reclaimed all charged bytes. A restored executable in another checkout passed all 299 upstream tests.
- Timed seven atomic edits and undos per tool with Cargo’s dev profile. Ripgrep undos took a median 161 ms with 0.6, 591 ms with 0.5, 270 ms with kache 2.2 and 604 ms with sccache 0.19. Every measured executable printed its expected revision.
- Linked and ran a downstream consumer against every shifted-asset revision from all four caches. Packed Jato retained 23.0 MiB of store files, kache 384.2 MiB and sccache 384.1 MiB. All four restored their own original archive exactly. sccache completed the eight first builds fastest.
- Measured five ripgrep worktree revisits, seven ordinary clean-target pairs, three simultaneous cold regex pairs and eleven watcher edits. A newly populated automatic-mode store also restored all 39 ripgrep units in another checkout and its executable passed the 299 upstream tests. The normal machine Cargo launcher restored and ran the six-unit regex application.
- Verified the locked Cargo source package offline, including the fixtures, benchmark harnesses, raw measurements and project documents. Checked the archive’s Git revision and the installed executable’s SHA-256 against the benchmarked binary.

## 2026-11-21 — concurrent builds and repeated validation

- Started two clean regex worktrees against the same empty 0.4 cache. All six compiler units ran twice and produced six stored entries. The pair finished in 11.18 s.

- Coalesced concurrent misses through process-lifetime local sockets. Two cold regex checkouts compiled six units once and restored the six counterparts. Tested divergent inputs, an error in one checkout, and owner termination without cleanup.
- Preserved up to three additional checked directory layouts per published entry. Six switches between two ripgrep checkouts required two module-resolution probes, down from twelve. Tested the metadata budget, bounded replacement and GC.
- Replaced duplicate full extern reads on native misses with an indexed size prefilter and a single post-compile content certification against the original file identities. A same-length unknown artifact and a replaced path both fail certification. All 47 standard tests passed on Rust 1.89 and 1.100.

- Moved the election off the warm-hit path. A miss compares SQLite’s connection-local data version after acquiring ownership to catch publication in the intervening interval. Added a concurrent benchmark that includes the CPU and sampled proportional memory of private cache daemons and their compilers.
- Found 0.4 rebuilding all watches after a subtree arrived. In a two-package workspace with 4,096 watched leaf directories, five directory arrivals took a median 458 ms to reach workspace success.

- Registered and polled Linux inotify watches directly. The watcher retains matchers only where ignore rules exist, shares directory paths, and attaches new subtrees in place after Cargo confirms workspace ownership. Tests move a populated subtree with ignore exceptions and add a workspace member with a previously unwatched external dependency.
- Across three launches and fifteen edits with 4,096 extra leaf directories, median time to start watching fell from 302 ms to 56.6 ms. Subtree arrival to workspace success fell from 346 ms to 70.9 ms, and idle process-tree PSS fell from 5,986 to 3,039 KiB.

- Verified the installed release with the system’s normal Cargo launcher. The stable and minimum toolchain each passed all 47 standard tests with worktree, sealed and native hits required. The 272 MiB packed stress test also passed.
- Migrated populated schema versions 1, 2, 3, 6, 8, 9, 10 and 11 and reclaimed all charged bytes. The 0.4 cache restored six real regex units after upgrading. A separate ripgrep checkout restored all 39 units, including the final executable, which passed all 299 upstream integration tests.

- Verified that a second checkout with a different included source could finish compiling while the first checkout’s actual rustc process was suspended. Both binaries printed their intended values.
- Three paired cold ripgrep trials finished in a median 25.92 s with the 0.5 engine, 59.96 s with 0.4, 31.63 s with upstream kache and 32.02 s with sccache. Jato used 77.05 CPU-seconds and 815 MiB sampled peak PSS. kache used less sampled memory.

- Measured the installed release with four concurrent cold regex checkouts. It used 24.65 CPU-seconds and 417 MiB sampled peak PSS. Jato 0.4 used 108.70 CPU-seconds and 1271 MiB. Seven alternating warm ripgrep trials put the installed release at 234 ms and kache at 379 ms.

- Built and verified the locked source package offline, including the Linux and portable watch backends, benchmark harnesses, raw samples, fixtures and project documents.

## 2026-11-21 — worktrees and packed writes

- Rebuilt the locked regex application in two target directories. All five registry libraries and their five metadata files were byte-identical, while their dep-info referred to different output paths. Jato 0.3 missed all five in the second target.
- A private mount-view prototype restored those five libraries in another checkout without copying sources or sharing writable targets. Identified source-directory identity and root ownership in user namespaces as remaining native-link checks.

- Added Linux worktree views for independent Cargo output locations. A second regex checkout restored all five libraries and the executable after rustc rechecked module resolution. The host validates root-owned linker inputs across the user namespace.
- Caught Cargo reusing a physical-path executable when switching modes. Separated each checkout’s writable Cargo state and verified builds, edits, competing modules, split output paths, process cancellation and simultaneous views.

- Packed writes now spool frame descriptors, skip compression for matching stored chunks, and spill the exact deduplication index after 1,024 distinct digests. Verified recovery when GC removes a chunk between staging and publication, including rejection of a changed compiler output.

- Added a concurrent Cargo build with two different generated sources sharing one global target, a projected watch/interrupt regression, and a host ownership reply test. All 40 tests passed with native hits, sealed storage and worktree support required on Rust 1.89 and 1.100.

- Avoided validating the native link closure twice when a checkout’s module directories change. Across seven alternating ripgrep pairs, the median fell from 319 ms to 289 ms. All 299 upstream integration tests passed against the restored executable.
- Profiling 192 shared 256 KiB chunks exposed SQLite rewriting payload pages just to increment a reference count. Switched reclamation to the existing indexed single-file and ordered-frame references. Tested both deletion orders and a live schema upgrade without rewriting payloads.

- Tested Ctrl-C through a real foreground terminal. Kept the mount supervisor alive until the command finishes handling its signal and restored the caller’s signal mask in the payload. Also confirmed target-only views share a dependency while preserving both applications’ physical source paths.
- Migrated populated schema versions 1, 2, 3, 6, 8, 9 and 10, then reclaimed all charged bytes. An installed-binary test restored six real 0.3 compiler entries. Checked the standard Cargo shim and ran all 299 ripgrep integration tests against a confirmed cross-checkout release hit.

- Across eight 16 MiB asset revisions, Jato skipped 218.75 MiB of recompression and used 22.1% fewer filesystem output blocks than 0.3. The previous and current releases restored the same original archive. A 272 MiB packed spill/restore test peaked at 7,776 KiB RSS.
- Across eleven edits, the installed ordinary watcher reported the first compiler error in 53.5 ms and workspace success after a fix in 54.9 ms. The projected watcher took 53.6 ms and 56.5 ms.

- Measured the installed release against upstream kache 2.2.0 and sccache 0.19.0 with explicit base directories. On a warm store and a separate checkout, ripgrep took 273 ms, 380 ms and 15.50 s respectively. The regex application took 109 ms, 180 ms and 4.98 s. Cold builds favored the peers.

- Passed all 43 standard tests on Rust 1.89 and 1.100 with native hits, sealed files and worktree support required. Ran the larger packed I/O test separately. Verified Cargo’s locked offline source package, its benchmarks, fixtures and all four project documents.

## 2026-11-21 — profiling the 0.2 release

- Profiled a clean-target ripgrep release restore. Its final wrapper spent 15.7 ms on direct externs, 65.4 ms on namespace and native inputs, and 1.0 ms restoring outputs. The recorded native reads included 104 MiB of Cargo target files.
- Borrowed up to three immediately available Cargo job slots for independent content reads. Five alternating restores gave medians of 234 ms for the prototype and 286 ms for 0.2. Each read retains content and file-replacement checks.
- Added observed native archive inputs for Rust libraries with native-only search paths and ordinary static or dynamic link flags. Verified byte changes, search priority, removal and bundled link attributes using a real C archive.
- Shared checked, compressed input manifests across source variants. Stored single-chunk artifacts directly by digest and joined artifact descriptors in one SQLite read. Tested shared-record corruption, eviction and recovery.
- Added bounded parallel BLAKE3 subtrees for large single inputs and streaming admission for large embedded assets. Validated empty, partial and power-of-two boundaries against standard BLAKE3 and changed an embedded byte through a real Cargo release build.
- Found exact sidecar metadata embedded at unaligned offsets in real rlibs. Packed storage now follows archive member and metadata boundaries and compresses repeated frames once per entry. Verified exact restores after shifting members and changing archive headers.
- Allowed an idle watcher to act on a completed Rust-file save. Close-only events require a pending edit, and a busy watcher or explicit debounce keeps batching. Published verified compiler diagnostics before waiting to record hit statistics.
- Investigated a slower ripgrep check in the first interleaved comparison. Alternating warm targets narrowed the gap. Checked small dependency sets through retained file descriptors, reserved spare threads for larger metadata batches, and limited read-ahead to actual pending inputs. Added a regression for replacing a path while its previous inode is open.
- Across 21 matched ripgrep checks on the installed release, 0.3 won 10 pairs and 0.2 won 11. The median within-pair difference was 0.17 ms. Verified all 32 project tests on both supported Rust toolchains.
- Verified the November 18 upstream kache 2.2.0 archive against its published SHA-256. Added independent contender selection and support for its changed statistics format alongside the locally installed 2.1.1.
- The final interleaved ripgrep release medians were 198 ms for Jato 0.3, 289 ms for 0.2, 251 ms for installed kache 2.1.1, and 5.52 s for sccache. Packed store size fell from 48.8 MiB to 37.3 MiB. Migrated schema versions 1, 2, 3, and 8 and verified byte accounting and pruning.
- Eight library revisions representing 280.0 MiB of compiler output occupied 90.9 MiB of packed payload, down from 187.3 MiB. Both versions restored the same first archive by SHA-256. Over 22 saves, median save-to-error latency improved from 65.3 ms to 48.3 ms.
- Compared directly with the upstream kache 2.2.0 executable over three additional ripgrep trials. Jato's warm check and release medians were 148 ms and 173 ms. Kache's were 353 ms and 305 ms.
- Warm zlib-backed release builds took 149 ms with Jato 0.3, 896 ms with 0.2, and 205 ms with upstream kache. The 12 MiB embedded-data release took 38 ms, 467 ms, and 131 ms respectively. Each executable's output was checked.
- The locked regex application restored in 69 ms for release and 64 ms for check. A new edit took 51 ms and undo took 28 ms. Verified `jato --system-linker build --release -j1` through the machine's ordinary Cargo launcher.
- Restored six legacy compiler entries after upgrading a live 0.2 cache. All 299 ripgrep integration tests passed on Rust 1.89 and against a confirmed restored Rust 1.100 executable. Verified the distributable 0.3.0 source package, documentation links, fixtures and benchmark samples.

## 2026-11-21

- Started from an empty repository. Found a locally customized kache installation and checked its scheduler notes and regression scenarios.
- Chose a local, daemon-free cache with content-addressed chunks and a separate edit watcher. Cargo keeps its normal target directory, scheduling, and incremental state.
- Selected cold, restored, unchanged, and edited builds as distinct measurement cases.
- Built and tested transactional storage, 256 KiB compressed chunks, duplicate elimination, staged restores, corruption recovery, and LRU pruning. Two storage regression tests pass.
- Integrated Cargo and rustc. Clean builds now restore libraries and check metadata, including empty metadata. Compiler errors, Cargo JSON diagnostics, test arguments, and ordinary incremental builds survive the wrapper.
- Exercised source edits, generated input paths containing spaces, environment changes, A→B→A reuse, new ambiguous modules, and an unusable cache in isolated Cargo projects.
- Added a native filesystem watcher with bounded event coalescing, cancellation of obsolete process groups, targeted package checks, and a workspace follow-up. A real workspace test covers saves, target-directory noise, and Ctrl-C.
- Added persistent compiler input manifests. A familiar input set now restores without starting rustc. Directory changes trigger fresh module discovery and ordinary source edits go straight to compilation.
- Added successful-compile admission checks for opaque macro chains, native inputs, and custom compilers. Verified generated files, an assembly include, proc-macro re-exports, two concurrent Cargo targets, and cache pruning.
- Reproduced an overly conservative native-link classification on aho-corasick and corrected it. Verified a clean restore with the older Cargo 1.85 target layout.
- Verified rustc's direct and transitive search-path behavior, then removed an unnecessary restriction on inactive extern declarations. Metadata-only evidence now stays separate from codegen evidence.
- Added artifact-notification checks, byte-for-byte restore tests, staged-file recovery after interrupted jobs, and coverage for nested wrappers, custom watch commands, and process-group cancellation.
- Built and ran the suite with Rust 1.89.0 in an isolated toolchain installation. Verified the wrapper against both newer and older Cargo output layouts.
- Added repeatable save-to-diagnostic and fix-to-workspace measurements, plus Linux CI for stable Rust and the minimum supported toolchain. Verified the installed binary with this machine's existing Cargo shim and test runner.
- A repeated watch test exposed a second run caused by Cargo's temporary sibling directory during target initialization. Filtered that short-lived directory and passed the cancellation test twelve times consecutively.
- Added end-to-end checks for damaged-cache recovery and independent invalidation by features, absent or empty environment variables, and escaped environment values. All sixteen tests pass on Rust 1.89.0 and 1.100.0.
- Measured the installed release over three runs per build scenario. Ripgrep clean checks went from 6.28 s to 0.65 s and release rebuilds from 61.61 s to 25.62 s. Its 116.1 MiB of artifacts occupied 36.0 MiB of SQLite files.
- Measured five edits end to end. Median save-to-diagnostic latency was 186 ms with a 100 ms debounce. Recorded raw samples, fresh and cold builds, edit overhead, RSS, and reproduction commands in bench/.
- Verified the distributable Cargo package and made the locked benchmark fixture available inside its source archive.

## 2026-11-21 — optimization pass

- Downloaded the upstream sccache 0.19.0 Linux release and verified its SHA-256. Ran kache 2.1.1 and sccache with private local stores and separate server sockets.
- A first regex fixture run placed warm release at 852 ms for Jato 0.1, 141 ms for kache, and 846 ms for sccache. Warm check took 112, 134, and 437 ms respectively. Identified the final executable as the main release gap.
- Replaced periodic child-status polling on Linux with pidfd notifications. Small saves now get a 16 ms default debounce, and larger or configuration changes retain a 100 ms window. Explicit debounce values still take precedence.
- Added Linux ELF recording for nonincremental GNU-target binaries. Successful and failed search paths, transitive link inputs, and tool identities are validated before replay. Regression tests change an archive, introduce a higher-priority library, and remove both libraries.
- Kept Cargo pipelining intact by withholding cache evidence for a consumer until the producer's exact artifact digest is certified. Folded source admission into hashing and memoized stable compiler identification.
- Added kernel-authenticated copy-on-write storage on compressed Btrfs. Large files can share extents with writable targets, with packed chunks on other filesystems. Verified clone independence, tamper recovery, reference-counted eviction, and cleanup after interrupted publication.
- Added verified Cargo version and target probes, a source-only rejection before large extern reads, bounded scratch buffers, anonymous output publication and less frequent SQLite checkpoints.
- Reused eligible proc-macro libraries and tested a macro reading an external file. Its consumer reruns and observes the change. Added per-command selection of the system linker for machines with compiler launchers on PATH.
- Rust 1.89 exposed a different temp directory and shared dependency output layout. Recorded the names, types and order returned by rustc's shared-directory reads and validated them before reuse. Exercised adding, changing, removing and reverting a native library.
- Tested PATH-based linker discovery as well as an explicit driver. Recognized the standard library's private close-on-exec acknowledgment, including split tracer records. The observer declines data from unknown sockets. All 23 tests passed on Rust 1.89 and 1.100 with native and sealed hits required.
- Traced the remaining compiler on a clean ripgrep check to its workspace build script. Added native reuse for a newly created incremental session, validating compiler-created work products and declining prior incremental inputs. A dev-profile regression runs an edit against live rustc state and restores the original executable after a clean.
- Qualified all 24 tests on stable Rust 1.100 and the 1.89 minimum with native hits and sealed storage required. Migrated a populated 0.1 cache and verified that garbage collection reclaimed its 11 entries and all charged bytes.
- Measured three ripgrep trials against isolated local peer caches. Warm checks took 221 ms with Jato, 314 ms with kache and 1.60 s with sccache. Warm release builds took 281 ms, 341 ms and 4.78 s. Packed Jato restored the release in 408 ms from 48.8 MiB of files.
- On the locked regex application, warm release fell from 968 ms in Jato 0.1 to 110 ms. A new edit checked in 55 ms and its undo in 31 ms. Nine watch edits reached the first error in a median 72.1 ms and the subsequent workspace success in 80.8 ms.
- Ran ripgrep's 299 upstream integration tests on both a cold and a clean-target warm Rust 1.89 build. The warm build logged 36 compiler hits. Verified the installed binary through the machine's normal Cargo setup with the system-linker option.
- Confirmed a native cache hit on the Rust 1.100 release build of ripgrep and ran all 299 upstream integration tests against that restored executable. Built and verified the locked Jato source package, including the fixtures, raw samples and all four project documents.
