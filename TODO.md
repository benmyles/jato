# Jato release checklist

- [x] Study Cargo, ccache, sccache, kache, rust-cache, and edit watchers.
- [x] Implement a bounded, transactional local artifact store with deduplication, integrity checks, statistics, and eviction.
- [x] Integrate the rustc wrapper with Cargo and validate compiler input discovery, diagnostics, and bypasses.
- [x] Implement change-driven workspace feedback and Cargo argument forwarding.
- [x] Exercise clean rebuilds, edited inputs, concurrent builds, cache failures, and watch behavior on real Cargo invocations.
- [x] Measure performance, resource use, and installability. Document the operating envelope and release checks.

## Compiler discovery and cache publication

- [x] Reproduce compiler discovery through PATH, rustup and an absolute toolchain.
- [x] Remove redundant discovery work while preserving compiler selection and cache validity.
- [x] Measure publication costs as unresolved Cargo dependencies accumulate and address verified bottlenecks.
- [x] Compare clean builds, warm targets and live edits with upstream caches and qualify the installed update.

## Olo and nightly Cargo

- [x] Test the reported nightly options, split library metadata, shared cache budgeting and delivery to a conventional artifact directory.
- [x] Measure the installed version on Olo protocol and system packages and report the results to the Olo developer.
- [x] Qualify upstream mold 3.0.0 with a fresh linked executable and a procedural macro whose expansion reads an untracked file.
- [x] Distinguish repeated quarantined records from actual stores and avoid reading their outputs again.
- [ ] Close the fully emptied-target Olo performance gap while preserving correct behavior for changing procedural-macro inputs and native dependencies.

## Coding agent skill

- [x] Write a portable Jato skill and exercise its Cargo commands in a Rust workspace.
- [x] Install the skill globally for Claude Code, Codex and agy and verify client discovery.

## Optimization pass

- [x] Establish isolated head-to-head baselines with kache and sccache.
- [x] Reduce compiler validation and artifact restore overhead.
- [x] Accelerate eligible native executable builds with verified linker inputs.
- [x] Reduce save-to-diagnostic latency and idle work in watch mode.
- [x] Qualify correctness, run repeated comparisons, and install the measured release.

## Verification and storage hot paths

- [x] Shorten content verification on the last link while respecting Cargo's job limit.
- [x] Reduce repeated store work on small artifacts and closely spaced saves.
- [x] Compare against the installed 0.2 release and peers, and qualify the installed update.

## Worktree reuse and packed writes

- [x] Reuse builds across independent worktrees and target locations with verified, stable compiler inputs.
- [x] Reduce repeated compression and bound per-entry indexing memory in packed storage.
- [x] Exercise concurrent and changed checkouts, compare warm and relocated builds with peers, and qualify the installed release.

## Concurrent builds and repeated validation

- [x] Coalesce matching compiler misses across independent Cargo builds and verify each follower’s own inputs.
- [x] Remove repeat validation work from native misses and recurring worktree restores.
- [x] Reduce watcher registration, idle memory and subtree attachment costs on large workspaces.
- [x] Test failure, cancellation, divergent inputs and eviction, then compare CPU time, wall time and storage against the installed release and upstream peers.

## Live edits and shifted artifacts

- [x] Restore verified native results on live incremental hits and keep new edits on rustc’s normal incremental path.
- [x] Reuse packed data across insertions and deletions while preserving bounded capture and exact restoration.
- [x] Reject impossible large-input variants before whole-file hashing and test changes inside and outside the sampled regions.
- [x] Reduce repeated worktree ownership checks, qualify the installed build and source package, and compare live edits, revision histories and clean builds with upstream peers.

## Cold capture and editor saves

- [x] Reduce temporary writes during cold capture without changing restored bytes.
- [x] Avoid repeated module discovery when a verified live executable survives an editor rename.
- [x] Select and expose a faster Cargo development profile with separate outputs and preserved project settings.
- [x] Measure cold builds and edit cycles against 0.6 and upstream caches, and qualify the installed release.

## Dependency changes and large packed frames

- [x] Reduce redundant whole-dependency reads when a producer changes under an otherwise unchanged consumer.
- [x] Reduce large packed payload writes and preserve bounded restoration, shared-frame eviction and corruption recovery.
- [x] Share a verified sealed base across nearby compiler-output revisions and verify the full result when restoring each edit.
- [x] Measure cold, restored and edited real Cargo builds, test upgrades and qualify the installed package.
