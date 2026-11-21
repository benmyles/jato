# Work journal

## 2026-11-21 — profiling the 0.2 release

- Profiled a clean-target ripgrep release restore. Its final wrapper spent 15.7 ms on direct externs, 65.4 ms on namespace and native inputs, and 1.0 ms restoring outputs. The recorded native reads included 104 MiB of Cargo target files.
- Borrowed up to three immediately available Cargo job slots for independent content reads. Five alternating restores gave medians of 234 ms for the prototype and 286 ms for 0.2. Each read retains content and file-replacement checks.
- Added observed native archive inputs for Rust libraries with native-only search paths and ordinary static or dynamic link flags. Verified byte changes, search priority, removal and bundled link attributes using a real C archive.
- Shared checked, compressed input manifests across source variants. Stored single-chunk artifacts directly by digest and joined artifact descriptors in one SQLite read. Tested shared-record corruption, eviction and recovery.
- Added bounded parallel BLAKE3 subtrees for large single inputs and streaming admission for large embedded assets. Validated empty, partial and power-of-two boundaries against standard BLAKE3 and changed an embedded byte through a real Cargo release build.
- Found exact sidecar metadata embedded at unaligned offsets in real rlibs. Packed storage now follows archive member and metadata boundaries and compresses repeated frames once per entry. Verified exact restores after shifting members and changing archive headers.
- Allowed an idle watcher to act on a completed Rust-file save. Close-only events require a pending edit, and a busy watcher or explicit debounce keeps batching. Published verified compiler diagnostics before waiting to record hit statistics.
- Investigated a slower ripgrep check in the first interleaved comparison. Alternating warm targets narrowed the gap. Checked small dependency sets through retained file descriptors, reserved spare threads for larger metadata batches, and limited read-ahead to actual pending inputs. Added a regression for replacing a path while its previous inode is open.

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
