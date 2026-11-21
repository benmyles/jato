use crate::{cargo_command, store};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
struct Dependency {
    path: Option<PathBuf>,
}
#[derive(Deserialize)]
struct Package {
    name: String,
    manifest_path: PathBuf,
    dependencies: Vec<Dependency>,
}
#[derive(Deserialize)]
struct Metadata {
    workspace_root: PathBuf,
    target_directory: PathBuf,
    build_directory: Option<PathBuf>,
    packages: Vec<Package>,
}

pub struct Workspace {
    pub roots: BTreeSet<PathBuf>,
    pub excluded: Vec<PathBuf>,
    members: Vec<(PathBuf, String)>,
}

pub fn configuration(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == ".cargo")
        || matches!(
            path.file_name().and_then(|s| s.to_str()),
            Some(
                "Cargo.toml"
                    | "Cargo.lock"
                    | "rust-toolchain"
                    | "rust-toolchain.toml"
                    | ".gitignore"
                    | ".ignore"
            )
        )
}

impl Workspace {
    pub fn load(extra: &[PathBuf]) -> Result<Self> {
        let out = cargo_command(&[])?
            .args(["metadata", "--format-version", "1", "--no-deps"])
            .output()?;
        ensure!(
            out.status.success(),
            "cargo metadata failed\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let meta: Metadata =
            serde_json::from_slice(&out.stdout).context("reading Cargo workspace metadata")?;
        let mut roots = BTreeSet::from([meta.workspace_root]);
        let mut members = Vec::new();
        for p in meta.packages {
            let dir = p
                .manifest_path
                .parent()
                .context("manifest has no directory")?
                .to_owned();
            roots.insert(dir.clone());
            members.push((dir, p.name));
            for dep in p.dependencies.into_iter().filter_map(|d| d.path) {
                roots.insert(dep);
            }
        }
        for dir in extra {
            roots.insert(
                std::fs::canonicalize(dir)
                    .with_context(|| format!("watching {}", dir.display()))?,
            );
        }
        members.sort_by_key(|(p, _)| std::cmp::Reverse(p.components().count()));
        let mut excluded = vec![meta.target_directory, store::cache_dir()?];
        excluded.extend(meta.build_directory);
        Ok(Self {
            roots,
            excluded,
            members,
        })
    }

    pub fn jobs(&self, changes: &super::Changes, options: &super::Options) -> Vec<Vec<OsString>> {
        if !options.job.is_empty() {
            return vec![options.job.clone()];
        }
        let full = vec![OsString::from("check"), OsString::from("--workspace")];
        let mut names = BTreeSet::new();
        if !changes.full && self.members.len() > 1 {
            for path in &changes.paths {
                let member = self.members.iter().find(|(root, _)| path.starts_with(root));
                match member {
                    Some((_, name)) if !configuration(path) => {
                        names.insert(name);
                    }
                    _ => return vec![full],
                }
            }
        }
        if names.is_empty() || names.len() == self.members.len() {
            return vec![full];
        }
        let mut focus = vec![OsString::from("check")];
        for name in names {
            focus.extend([OsString::from("-p"), OsString::from(name)]);
        }
        if options.focus_only {
            vec![focus]
        } else {
            vec![focus, full]
        }
    }
}
