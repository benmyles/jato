//! Give Cargo repeatable paths while each invocation keeps its own files.
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Command;

mod host;
pub(crate) use host::system_owner;

pub const HELP: &str = "\
Usage
  jato worktree [--root DIR] [--system-linker] check|build|test|run|clippy [ARGS...]
  jato worktree [--root DIR] watch [WATCH_OPTIONS...]
  jato worktree [--root DIR] cargo [+toolchain] [CARGO_ARGS...]
  jato worktree [--root DIR] exec PROGRAM [ARGS...]
  jato worktree [--root DIR] paths [CARGO_LOCATION_OPTIONS...]

Reuse common dependencies across local target directories on Linux with bubblewrap.
By default source paths stay physical. --root also gives that source tree a stable
path, including local crates. Choose a root containing related path dependencies.
Cargo and its children see the view paths. Files are written to the original
checkout and private subdirectories under Cargo's target/build locations.
Use paths for their physical locations or cargo clean to remove their artifacts.
";

#[derive(Deserialize)]
struct Locations {
    target_directory: PathBuf,
    build_directory: Option<PathBuf>,
}

struct View {
    base: PathBuf,
    mounts: Vec<(PathBuf, PathBuf)>,
}

impl View {
    fn map(&self, path: &Path) -> PathBuf {
        // The most specific source wins when the target is inside the checkout.
        self.mounts
            .iter()
            .filter_map(|(physical, logical)| {
                path.strip_prefix(physical).ok().map(|tail| {
                    (
                        physical.components().count(),
                        if tail.as_os_str().is_empty() {
                            logical.clone()
                        } else {
                            logical.join(tail)
                        },
                    )
                })
            })
            .max_by_key(|(depth, _)| *depth)
            .map_or_else(|| path.to_owned(), |(_, p)| p)
    }

    fn add(&mut self, physical: &Path, name: &str) -> Result<PathBuf> {
        let physical = fs::canonicalize(physical)
            .with_context(|| format!("locating {}", physical.display()))?;
        ensure!(
            !self.base.starts_with(&physical) && !physical.starts_with(&self.base),
            "the worktree view directory overlaps {}. Select a JATO_CACHE_DIR outside the mapped directories",
            physical.display()
        );
        if let Some((_, alias)) = self.mounts.iter().find(|(p, _)| p == &physical) {
            return Ok(alias.clone());
        }
        let alias = self.base.join(name);
        self.mounts.push((physical, alias.clone()));
        Ok(alias)
    }
}

fn value<'a>(arg: &'a OsStr, name: &str) -> Option<Option<&'a OsStr>> {
    use std::os::unix::ffi::OsStrExt;
    if arg == name {
        return Some(None);
    }
    arg.as_bytes()
        .strip_prefix(name.as_bytes())?
        .strip_prefix(b"=")
        .map(|v| Some(OsStr::from_bytes(v)))
}

fn config_value(key: &str, path: &Path) -> Result<OsString> {
    Ok(format!(
        "{key}={}",
        serde_json::to_string(path.to_str().context("Cargo config paths must be UTF-8")?)?
    )
    .into())
}

// Use Cargo's own config resolution, including workspace-relative build-dir
// templates. Only inspect options before the program/test argument separator.
fn locations(args: &[OsString]) -> Result<Locations> {
    // CI and build drivers commonly set both paths. Absolute literal paths
    // have no workspace templates or Cargo config left to resolve.
    if !args
        .iter()
        .take_while(|a| *a != "--")
        .any(|a| value(a, "--config").is_some() || value(a, "--target-dir").is_some())
        && let (Some(target), Some(build)) = (
            env::var_os("CARGO_TARGET_DIR"),
            env::var_os("CARGO_BUILD_BUILD_DIR"),
        )
        && Path::new(&target).is_absolute()
        && Path::new(&build).is_absolute()
        && !build.as_encoded_bytes().contains(&b'{')
    {
        return Ok(Locations {
            target_directory: target.into(),
            build_directory: Some(build.into()),
        });
    }
    let mut cmd = Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    if let Some(t) = args
        .first()
        .filter(|a| a.as_encoded_bytes().starts_with(b"+"))
    {
        cmd.arg(t);
    }
    cmd.args(["metadata", "--format-version", "1", "--no-deps"]);
    let mut parts = args.iter().take_while(|a| *a != "--");
    let mut target = None;
    while let Some(arg) = parts.next() {
        for name in ["--manifest-path", "-m", "--config", "--target-dir"] {
            if let Some(inline) = value(arg, name) {
                let v = inline
                    .or_else(|| parts.next().map(OsString::as_os_str))
                    .with_context(|| format!("{name} needs a value"))?;
                if name == "--target-dir" {
                    target = Some(std::path::absolute(v)?);
                } else {
                    cmd.arg(if name == "-m" {
                        "--manifest-path"
                    } else {
                        name
                    })
                    .arg(v);
                }
                break;
            }
        }
        if matches!(arg.to_str(), Some("--locked" | "--frozen" | "--offline")) {
            cmd.arg(arg);
        }
    }
    if let Some(p) = target {
        cmd.arg("--config")
            .arg(config_value("build.target-dir", &p)?);
    }
    let out = cmd
        .output()
        .context("starting Cargo metadata for the worktree")?;
    ensure!(
        out.status.success(),
        "Cargo metadata failed\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).context("reading Cargo target directories")
}

fn view_args(args: &[OsString], view: &View, target: &Path, build: &Path) -> Result<Vec<OsString>> {
    let mut result = Vec::with_capacity(args.len());
    let mut it = args.iter();
    let mut opaque = None;
    let mut config_file = false;
    while let Some(arg) = it.next() {
        if arg == "--" {
            opaque = Some(it);
            break;
        }
        let Some((name, inline)) = ["--manifest-path", "-m", "--target-dir", "--config"]
            .into_iter()
            .find_map(|n| value(arg, n).map(|v| (n, v)))
        else {
            result.push(arg.clone());
            continue;
        };
        let raw = inline
            .or_else(|| it.next().map(OsString::as_os_str))
            .with_context(|| format!("{name} needs a value"))?;
        config_file |= name == "--config" && !raw.as_encoded_bytes().contains(&b'=');
        let mapped = match name {
            "--target-dir" => target.as_os_str().to_owned(),
            "--config"
                if raw.to_str().is_some_and(|v| {
                    v.split_once('=')
                        .is_some_and(|(k, _)| k.trim() == "build.target-dir")
                }) =>
            {
                config_value("build.target-dir", target)?
            }
            "--config"
                if raw.to_str().is_some_and(|v| {
                    v.split_once('=')
                        .is_some_and(|(k, _)| k.trim() == "build.build-dir")
                }) =>
            {
                config_value("build.build-dir", build)?
            }
            _ if Path::new(raw).is_absolute() => view.map(Path::new(raw)).into_os_string(),
            _ => raw.to_owned(),
        };
        result.push(name.into());
        result.push(mapped);
    }
    if config_file {
        // --config files outrank environment variables. Keep their other
        // settings and override only the two physical paths we mounted.
        result.push("--config".into());
        result.push(config_value("build.target-dir", target)?);
        result.push("--config".into());
        result.push(config_value("build.build-dir", build)?);
    }
    if let Some(tail) = opaque {
        result.push("--".into());
        result.extend(tail.cloned());
    }
    Ok(result)
}

fn ancestor_settings(start: &Path) -> Vec<PathBuf> {
    start
        .ancestors()
        .flat_map(|dir| {
            [
                ".cargo/config",
                ".cargo/config.toml",
                "rust-toolchain",
                "rust-toolchain.toml",
            ]
            .into_iter()
            .map(move |name| dir.join(name))
            .filter(|p| p.is_file())
        })
        .collect()
}

// Directory overrides in rustup's settings are keyed by the physical
// checkout. Pin its selection before Cargo moves into the projected tree.
fn project_toolchain(cmd: &mut Command) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    if env::var_os("RUSTUP_TOOLCHAIN").is_some() {
        return Ok(());
    }
    let cargo: PathBuf = env::var_os("CARGO")
        .unwrap_or_else(|| "cargo".into())
        .into();
    let resolved = if cargo.components().count() > 1 {
        Some(cargo)
    } else {
        env::split_paths(&env::var_os("PATH").unwrap_or_default())
            .map(|dir| dir.join(&cargo))
            .find(|path| path.is_file())
    };
    if let Some(cargo) = resolved {
        let proxy = cargo.with_file_name("rustup");
        let ident = |p| fs::metadata(p).map(|m| (m.dev(), m.ino()));
        if let (Ok(a), Ok(b)) = (ident(&cargo), ident(&proxy))
            && a == b
        {
            let out = Command::new(proxy)
                .args(["show", "active-toolchain"])
                .output()?;
            ensure!(
                out.status.success(),
                "selecting the worktree toolchain: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let name = std::str::from_utf8(&out.stdout)?
                .split_whitespace()
                .next()
                .context("rustup returned no active toolchain")?;
            cmd.env("RUSTUP_TOOLCHAIN", name);
        }
    }
    Ok(())
}

pub fn run(args: &[OsString], mut system_linker: bool) -> Result<i32> {
    let mut args = args;
    let mut source = None;
    loop {
        match args.first().and_then(|a| a.to_str()) {
            Some("--root") => {
                let p = args.get(1).context("--root needs a directory")?;
                ensure!(source.is_none(), "provide one --root");
                source = Some(
                    fs::canonicalize(p)
                        .with_context(|| format!("locating --root {}", Path::new(p).display()))?,
                );
                args = &args[2..];
            }
            Some("--system-linker") => {
                system_linker = true;
                args = &args[1..];
            }
            Some("-h" | "--help" | "help") | None => {
                print!("{HELP}");
                return Ok(0);
            }
            _ => break,
        }
    }
    let kind = args[0].to_str().context("invalid worktree command")?;
    ensure!(
        matches!(
            kind,
            "check" | "build" | "run" | "test" | "clippy" | "cargo" | "watch" | "exec" | "paths"
        ),
        "unknown worktree command {kind}. Run jato worktree --help"
    );
    ensure!(
        env::var_os("JATO_WORKTREE_ACTIVE").is_none(),
        "already inside a Jato worktree view"
    );
    let job = match kind {
        "cargo" => &args[1..],
        "watch" => args
            .iter()
            .position(|a| a == "--")
            .map_or(&[][..], |i| &args[i + 1..]),
        "exec" => &[],
        _ => args,
    };
    let cwd = env::current_dir()?;
    let locations = locations(job)?;
    let cache = crate::store::cache_dir()?;
    fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&cache)?;
    let cache = fs::canonicalize(cache)?;
    let base = cache.join("view-v1");
    fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(&base)?;
    ensure!(
        fs::canonicalize(&base)? == base,
        "redirected worktree view directory"
    );
    let mut view = View {
        base,
        mounts: Vec::new(),
    };
    if let Some(p) = &source {
        ensure!(
            p.is_dir() && cwd.starts_with(p),
            "--root must contain the working directory"
        );
        let parent = p
            .parent()
            .context("--root must be below the filesystem root")?;
        ensure!(
            ancestor_settings(parent) == ancestor_settings(&view.base),
            "the projected source would discover different ancestor Cargo or toolchain settings. Choose a common --root containing the project settings, use a cache under the same configured ancestry, or omit --root"
        );
        view.add(p, "workspace")?;
    }
    let physical_build = locations
        .build_directory
        .as_ref()
        .unwrap_or(&locations.target_directory);
    for p in [&locations.target_directory, physical_build] {
        fs::create_dir_all(p)
            .with_context(|| format!("creating Cargo directory {}", p.display()))?;
    }
    let physical_target = fs::canonicalize(&locations.target_directory)?;
    let physical_build = fs::canonicalize(physical_build)?;
    let source_target = view.map(&physical_target);
    let source_build = view.map(&physical_build);
    // Cargo freshness assumes a persistent compiler-visible layout. Keep
    // normal Cargo, each projected checkout, and different cache locations
    // independent even when the caller sets one global CARGO_TARGET_DIR.
    let mut layout = blake3::Hasher::new_derive_key("jato physical worktree layout v1");
    crate::fingerprint::field(&mut layout, cache.as_os_str().as_encoded_bytes());
    if let Some(p) = &source {
        crate::fingerprint::field(&mut layout, p.as_os_str().as_encoded_bytes());
    }
    layout.update(&[u8::from(physical_build != physical_target)]);
    let name = layout.finalize().to_hex();
    let scope = Path::new(".jato-worktree").join(&name[..24]);
    let target_parent = physical_target.join(&scope);
    let build_parent = physical_build.join(scope);
    for p in [&target_parent, &build_parent] {
        fs::create_dir_all(p.join("out"))?;
        ensure!(
            fs::canonicalize(p)? == *p && fs::canonicalize(p.join("out"))? == p.join("out"),
            "redirected worktree output directory"
        );
    }
    if kind == "paths" {
        println!(
            "{}",
            serde_json::json!({"target_directory": target_parent.join("out"), "build_directory": build_parent.join("out")})
        );
        return Ok(0);
    }
    // Mount the parent, so Cargo can safely unlink the output root on clean.
    let target = view.add(&target_parent, "target")?.join("out");
    let build = view.add(&build_parent, "build")?.join("out");
    let mut cmd = Command::new("bwrap");
    if source.is_some()
        && !job
            .first()
            .is_some_and(|a| a.as_encoded_bytes().starts_with(b"+"))
    {
        project_toolchain(&mut cmd)?;
    }
    cmd.env("JATO_WORKTREE_TARGET_ALIAS", source_target)
        .env("JATO_WORKTREE_BUILD_ALIAS", source_build);
    cmd.args(["--die-with-parent", "--dev-bind", "/", "/", "--tmpfs"])
        .arg(&view.base);
    for (real, alias) in &view.mounts {
        cmd.arg("--bind").arg(real).arg(alias);
    }
    let working = view.map(&cwd);
    cmd.arg("--chdir").arg(&working);
    let jato = env::current_exe()?;
    let mut payload = vec![jato.clone().into_os_string(), "__worktree-launch".into()];
    cmd.env("PWD", &working)
        .env("CARGO_TARGET_DIR", &target)
        .env("CARGO_BUILD_BUILD_DIR", &build)
        .env("JATO_CACHE_DIR", &cache)
        .env("JATO_WORKTREE_ACTIVE", "1");
    if env::var_os("CARGO_BUILD_TARGET_DIR").is_some() {
        cmd.env("CARGO_BUILD_TARGET_DIR", &target);
    }
    if kind == "exec" {
        let exec = args.get(1).context("worktree exec needs a program")?;
        payload.push("exec".into());
        payload.push(exec.clone());
        payload.extend_from_slice(&args[2..]);
        cmd.env("RUSTC_WRAPPER", &jato);
        if system_linker {
            crate::set_system_linker(&mut cmd)?;
        }
    } else {
        if system_linker {
            payload.push("--system-linker".into());
        }
        if kind == "watch" {
            let boundary = args.iter().position(|a| a == "--");
            payload.extend_from_slice(&args[..boundary.unwrap_or(args.len())]);
            if let Some(i) = boundary {
                payload.push("--".into());
                payload.extend(view_args(&args[i + 1..], &view, &target, &build)?);
            }
        } else {
            payload.extend(view_args(args, &view, &target, &build)?);
        }
    }
    if env::var_os("JATO_LOG").as_deref() == Some(OsStr::new("debug")) {
        for (real, alias) in &view.mounts {
            eprintln!("jato worktree: {} -> {}", alias.display(), real.display());
        }
    }
    host::run(cmd, &payload)
}

pub fn enter(args: &[OsString]) -> Result<i32> {
    ensure!(
        env::var_os("JATO_WORKTREE_ACTIVE").as_deref() == Some(OsStr::new("1")),
        "missing worktree view"
    );
    if env::var_os("JATO_WORKTREE_PROCESS_GROUP").as_deref() == Some(OsStr::new("1"))
        && unsafe { libc::setpgid(0, 0) } != 0
    {
        return Err(std::io::Error::last_os_error()).context("creating worktree process group");
    }
    if args.first().is_some_and(|a| a == "exec") {
        let mut cmd = Command::new(args.get(1).context("missing worktree program")?);
        cmd.args(&args[2..]);
        return crate::compiler::passthrough(cmd);
    }
    crate::dispatch(args)
}
