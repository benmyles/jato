# Jato release checklist

- [x] Study Cargo, ccache, sccache, kache, rust-cache, and edit watchers.
- [x] Implement a bounded, transactional local artifact store with deduplication, integrity checks, statistics, and eviction.
- [x] Integrate the rustc wrapper with Cargo and validate compiler input discovery, diagnostics, and bypasses.
- [x] Implement change-driven workspace feedback and Cargo argument forwarding.
- [x] Exercise clean rebuilds, edited inputs, concurrent builds, cache failures, and watch behavior on real Cargo invocations.
- [x] Measure performance, resource use, and installability. Document the operating envelope and release checks.
