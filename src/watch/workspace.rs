use crate::{cargo_command, store};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
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

/// Carry location-changing options over to Cargo metadata. The program's
/// own arguments following a second `--` are opaque to the watcher.
fn metadata_command(
    job: &[OsString],
    system_linker: bool,
) -> Result<(std::process::Command, Vec<PathBuf>)> {
    let mut cmd = cargo_command(&[], system_linker)?;
    if let Some(toolchain) = job
        .first()
        .filter(|s| s.as_encoded_bytes().starts_with(b"+"))
    {
        cmd.arg(toolchain);
    }
    cmd.args(["metadata", "--format-version", "1", "--no-deps"]);
    let mut excluded = Vec::new();
    let mut args = job.iter().take_while(|a| *a != "--");
    while let Some(arg) = args.next() {
        let (option, inline) = arg
            .to_str()
            .and_then(|a| a.split_once('='))
            .map_or((arg.as_os_str(), None), |(k, v)| {
                (OsStr::new(k), Some(OsStr::new(v)))
            });
        if matches!(
            option.to_str(),
            Some("--manifest-path" | "--config" | "--target-dir")
        ) {
            let value = inline
                .or_else(|| args.next().map(OsString::as_os_str))
                .context("missing Cargo option value")?;
            if option == "--target-dir" {
                excluded.push(std::path::absolute(value)?);
            } else {
                cmd.arg(option).arg(value);
            }
        } else if matches!(option.to_str(), Some("--offline" | "--locked" | "--frozen")) {
            cmd.arg(option);
        }
    }
    Ok((cmd, excluded))
}

impl Workspace {
    pub fn load(options: &super::Options) -> Result<Self> {
        let (mut cmd, mut excluded) = metadata_command(&options.job, options.system_linker)?;
        let out = cmd.output()?;
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
        for dir in &options.extra {
            roots.insert(
                std::fs::canonicalize(dir)
                    .with_context(|| format!("watching {}", dir.display()))?,
            );
        }
        members.sort_by_key(|(p, _)| std::cmp::Reverse(p.components().count()));
        excluded.push(meta.target_directory);
        let cache = store::cache_dir()?;
        if roots.iter().any(|r| r.starts_with(&cache)) {
            for name in [
                "artifacts-v1.sqlite3",
                "artifacts-v1.sqlite3-wal",
                "artifacts-v1.sqlite3-shm",
                "schema.lock",
                "objects-v1",
                ".jato-staging-v1",
            ] {
                excluded.push(cache.join(name));
            }
        } else {
            excluded.push(cache);
        }
        excluded.extend(meta.build_directory);
        if std::env::var_os("JATO_WORKTREE_ACTIVE").as_deref() == Some(OsStr::new("1")) {
            excluded.extend(
                ["JATO_WORKTREE_TARGET_ALIAS", "JATO_WORKTREE_BUILD_ALIAS"]
                    .into_iter()
                    .filter_map(std::env::var_os)
                    .map(PathBuf::from),
            );
        }
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
