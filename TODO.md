# Jato release checklist

- [x] Study Cargo, ccache, sccache, kache, rust-cache, and edit watchers.
- [ ] Implement a bounded, transactional local artifact store with deduplication, integrity checks, statistics, and eviction.
- [ ] Integrate the rustc wrapper with Cargo and validate compiler input discovery, diagnostics, and bypasses.
- [ ] Implement change-driven workspace feedback and Cargo argument forwarding.
- [ ] Exercise clean rebuilds, edited inputs, concurrent builds, cache failures, and watch behavior on real Cargo invocations.
- [ ] Measure performance, resource use, and installability. Document the operating envelope and release checks.
