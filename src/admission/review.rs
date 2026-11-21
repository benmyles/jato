//! Some macro token trees contain assembly names whose bindings can only be
//! understood in the context of the whole crate. These receipts describe
//! specific reviewed Linux source sets. Every ordinary input and producer
//! check still applies. See bench/source-reviews.md.
use crate::fingerprint::field;
use crate::invocation::Invocation;
use crate::store::{Digest, Store};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

pub(crate) struct Review<'a> {
    logical: PathBuf,
    physical: &'a Path,
    candidate: &'static Candidate,
    hash: Option<blake3::Hasher>,
    records: Vec<Digest>,
    generated: Option<(PathBuf, PathBuf)>,
    pub used: bool,
}

pub(crate) struct Candidate {
    name: &'static str,
    count: usize,
    sites: &'static [&'static str],
    digest: &'static str,
    deps: &'static [&'static str],
}

const CANDIDATES: &[Candidate] = &[
    Candidate {
        name: "libc",
        deps: &[],
        count: 65,
        sites: &["src/new/mod.rs"],
        digest: "57386c26cc2b97755b9691772089770e7d41fce3ba9f23572b5848f2716d1fd4",
    },
    Candidate {
        name: "cmov",
        deps: &[],
        count: 7,
        sites: &["src/backends/x86.rs"],
        digest: "153475abf838d6486f430eea68e52b498ade1dccb3be96d3a30d14379c543c5c",
    },
    Candidate {
        name: "num_bigint",
        deps: &["num_integer", "num_traits"],
        count: 28,
        sites: &["src/biguint/division.rs"],
        digest: "94ac60737081ebf25f3ce1b9258b28b4a990ed40163c3b77200262cecea02fb1",
    },
    Candidate {
        name: "rustix",
        deps: &["bitflags", "linux_raw_sys"],
        count: 180,
        sites: &[
            "src/backend/linux_raw/arch/mod.rs",
            "src/backend/linux_raw/arch/x86_64.rs",
            "src/backend/linux_raw/vdso_wrappers.rs",
        ],
        digest: "57370a846dee5bca21c57e40e2607fa3d83fc920f43a9ed80b4c7800503b2f39",
    },
    Candidate {
        name: "bitflags",
        count: 8,
        sites: &[],
        deps: &["serde_core"],
        digest: "7fb3b93764c13ff5d164679c2c4077af863d592ec73d2c6097d391f445948576",
    },
    Candidate {
        name: "serde_core",
        count: 19,
        sites: &[],
        deps: &[],
        digest: "1778708570a890d285ae1d81d85466896fee4fb49cbab49acdb0e3f0cd09d7a6",
    },
    Candidate {
        name: "linux_raw_sys",
        count: 12,
        sites: &[],
        deps: &[],
        digest: "e1b57122810cdc7a407296bce5738864c939fbc911eefb653637b42fa783d9a9",
    },
    Candidate {
        name: "num_traits",
        count: 19,
        sites: &[],
        deps: &["libm"],
        digest: "7eef0b90fae1b6e462e81d40a290039eebfb57de7f417600cc6d7f2dc077dbfe",
    },
    Candidate {
        name: "libm",
        count: 142,
        sites: &[],
        deps: &[],
        digest: "3226eefb7d2a575fc27db9acc08c7cd82e396cc8669feecec181d8608c57e3f5",
    },
    Candidate {
        name: "num_integer",
        count: 3,
        sites: &[],
        deps: &["num_traits"],
        digest: "6998e69cf01bf264d4fb774a069a58d6a79b7731a2b3d3d5063d5a78cda26df8",
    },
];

/// An origin can describe inputs to an ordinary independently admissible
/// producer before its pipelined dependencies finish. A consumer using a
/// reviewed assembly exception must prove the complete graph at publication.
#[derive(Serialize, Deserialize)]
pub(crate) struct Origin {
    pub receipt: String,
    pub dependencies: Vec<Digest>,
}

impl Candidate {
    pub fn origin(&self, dependencies: Vec<Digest>) -> Result<Origin> {
        ensure!(
            self.deps.len() == dependencies.len(),
            "reviewed extern set changed"
        );
        Ok(Origin {
            receipt: self.digest.to_owned(),
            dependencies,
        })
    }

    pub fn verify_dependencies(
        &self,
        store: &Store,
        dependencies: &[Digest],
        budget: &mut usize,
    ) -> Result<bool> {
        if self.deps.len() != dependencies.len() {
            return Ok(false);
        }
        for (&name, digest) in self.deps.iter().zip(dependencies) {
            let provider = CANDIDATES
                .iter()
                .find(|p| p.name == name)
                .expect("known source provider");
            if !store.has_reviewed_origin(digest, provider, budget)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub fn matches(&self, origin: &Origin) -> bool {
        origin.receipt == self.digest
    }
}

impl<'a> Review<'a> {
    pub fn new(
        inv: &Invocation,
        root: &'a Path,
        cwd: &Path,
        count: usize,
        env: &BTreeSet<String>,
    ) -> Option<Self> {
        if !cfg!(all(
            target_os = "linux",
            target_arch = "x86_64",
            target_env = "gnu"
        )) || !inv.codegen
            || inv.executable
            || !inv.deferred_links
            || !root.ends_with("src/lib.rs")
            || !inv.source.ends_with("src/lib.rs")
        {
            return None;
        }
        let candidate = CANDIDATES
            .iter()
            .find(|c| c.name == inv.name && c.count == count)?;
        let context = inv.review_settings.as_ref()?;
        let generated =
            if candidate.name == "serde_core" && env.len() == 1 && env.contains("OUT_DIR") {
                let path = PathBuf::from(std::env::var_os("OUT_DIR")?).join("private.rs");
                Some((path.clone(), path.canonicalize().ok()?))
            } else if env.is_empty() {
                None
            } else {
                return None;
            };
        let mut hash = blake3::Hasher::new();
        field(&mut hash, b"jato reviewed Rust assembly inputs v1");
        for option in context {
            field(&mut hash, option.as_bytes());
        }
        field(&mut hash, b"files");
        Some(Self {
            logical: cwd.join(inv.source.parent()?.parent()?),
            physical: root.parent()?.parent()?,
            candidate,
            hash: Some(hash),
            records: Vec::with_capacity(count),
            generated,
            used: false,
        })
    }

    pub fn is_candidate(name: &OsStr) -> bool {
        CANDIDATES.iter().any(|c| c.name == name)
    }

    pub fn is_provider(name: &OsStr) -> bool {
        CANDIDATES
            .iter()
            .any(|c| c.sites.is_empty() && c.name == name)
    }

    pub fn permits_site(&self, path: &Path) -> bool {
        self.hash.is_some()
            && path
                .strip_prefix(self.physical)
                .is_ok_and(|p| self.candidate.sites.iter().any(|s| p == Path::new(s)))
    }

    pub fn record(&mut self, logical: &Path, physical: &Path, digest: &Digest) {
        if self.hash.is_some() {
            let mut h = blake3::Hasher::new();
            match (
                logical.strip_prefix(&self.logical),
                physical.strip_prefix(self.physical),
            ) {
                (Ok(source), Ok(resolved)) => {
                    field(&mut h, b"package");
                    field(&mut h, source.as_os_str().as_bytes());
                    field(&mut h, resolved.as_os_str().as_bytes());
                }
                _ if self
                    .generated
                    .as_ref()
                    .is_some_and(|(src, real)| src == logical && real == physical) =>
                {
                    field(&mut h, b"OUT_DIR/private.rs");
                }
                _ => {
                    self.hash = None;
                    return;
                }
            }
            field(&mut h, digest);
            self.records.push(*h.finalize().as_bytes());
        }
    }

    pub fn finish(mut self) -> Result<Option<&'static Candidate>> {
        let valid = self.hash.is_some_and(|mut h| {
            self.records.sort_unstable();
            for item in self.records {
                field(&mut h, &item);
            }
            h.finalize().to_hex().as_str() == self.candidate.digest
        });
        ensure!(
            !self.used || valid,
            "assembly input outside the reviewed source set"
        );
        Ok(valid.then_some(self.candidate))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reviewed_artifact_graph_is_closed_and_settings_are_reproducible() {
        let audit: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../../bench/source-receipts-0.11.json")).unwrap();
        fn visit(profile: &Candidate, stack: &mut Vec<&'static str>) {
            assert!(!stack.contains(&profile.name), "source-provider cycle");
            stack.push(profile.name);
            for dep in profile.deps {
                visit(CANDIDATES.iter().find(|c| c.name == *dep).unwrap(), stack);
            }
            stack.pop();
        }
        assert_eq!(audit.len(), CANDIDATES.len());
        for profile in CANDIDATES {
            visit(profile, &mut Vec::new());
            let spec = audit
                .iter()
                .find(|a| a["receipt"] == profile.digest)
                .unwrap();
            assert_eq!(spec["files"], profile.count);
            let externs: Vec<_> = spec["settings"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|v| {
                    v.as_str()?
                        .strip_prefix("--extern[")?
                        .split_once("]=")
                        .map(|(_, name)| name)
                })
                .collect();
            assert_eq!(externs, profile.deps);
            assert!(blake3::Hash::from_hex(profile.digest).is_ok());
        }
    }
}
