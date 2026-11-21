# Jato release checklist

- [x] Study Cargo, ccache, sccache, kache, rust-cache, and edit watchers.
- [x] Implement a bounded, transactional local artifact store with deduplication, integrity checks, statistics, and eviction.
- [x] Integrate the rustc wrapper with Cargo and validate compiler input discovery, diagnostics, and bypasses.
- [x] Implement change-driven workspace feedback and Cargo argument forwarding.
- [x] Exercise clean rebuilds, edited inputs, concurrent builds, cache failures, and watch behavior on real Cargo invocations.
- [x] Measure performance, resource use, and installability. Document the operating envelope and release checks.

## Optimization pass

- [x] Establish isolated head-to-head baselines with kache and sccache.
- [ ] Reduce compiler validation and artifact restore overhead.
- [ ] Accelerate eligible native executable builds with verified linker inputs.
- [ ] Reduce save-to-diagnostic latency and idle work in watch mode.
- [ ] Qualify correctness, run repeated comparisons, and install the measured release.
