# Work journal

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
