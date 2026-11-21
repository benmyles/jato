use crate::fingerprint;
use crate::invocation::Invocation;
use crate::store::{DIAGNOSTIC_LIMIT, Digest, Evidence, Store};
use anyhow::{Context, Result, bail, ensure};
use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Instant, SystemTime};

pub fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
}

pub fn passthrough(mut cmd: Command) -> Result<i32> {
    Err(cmd.exec()).context("starting original command")
}

fn original(compiler: &OsStr, args: &[OsString]) -> Command {
    let mut cmd = Command::new(compiler);
    cmd.args(args);
    cmd
}

fn trace(name: &str, message: impl std::fmt::Display) {
    if env::var_os("JATO_LOG").as_deref() == Some(OsStr::new("debug")) {
        eprintln!("jato [{name}] {message}");
    }
}

fn discover(
    store: &mut Store,
    compiler: &OsStr,
    inv: &Invocation,
    recipe: &fingerprint::Recipe,
) -> Result<Option<Vec<u8>>> {
    let plans = store.plans(&recipe.key)?;
    if plans.is_empty() {
        trace(
            &inv.name,
            "no previous entry for these compiler arguments and toolchain",
        );
        return Ok(None);
    }
    let start = SystemTime::now();
    let externs = fingerprint::externs(inv, start, store, false)?.key;
    for encoded in plans {
        let plan: fingerprint::Plan = serde_json::from_slice(&encoded)?;
        if inv.executable && plan.native.is_none() {
            continue;
        }
        let Ok(snapshot) = plan.snapshot(recipe, inv, start, &externs, false) else {
            continue;
        };
        let Some(topology) = store.topology(&snapshot.key)? else {
            continue;
        };
        if topology == snapshot.topology {
            return store.restore(&snapshot.key, &inv.outputs);
        }
        // Familiar bytes with a changed module search directory need fresh
        // name resolution. Ordinary edits go directly to the actual compiler.
        if plan.native.is_some() {
            continue;
        }
        return rediscover(store, compiler, inv, recipe, start, &externs);
    }
    trace(
        &inv.name,
        "tracked inputs changed or previous entry was evicted",
    );
    Ok(None)
}

fn rediscover(
    store: &mut Store,
    compiler: &OsStr,
    inv: &Invocation,
    recipe: &fingerprint::Recipe,
    start: SystemTime,
    externs: &Digest,
) -> Result<Option<Vec<u8>>> {
    let _ = store.count("discovery_probe", 1);
    let probe = crate::staging::file(&store.dir)?;
    let mut emit = OsString::from("--emit=dep-info=");
    emit.push(probe.path());
    let status = Command::new(compiler)
        .args(&inv.probe_args)
        .arg(emit)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    ensure!(status.success(), "input discovery failed");
    let plan = fingerprint::dependencies(probe.path())?;
    let snapshot = plan.snapshot(recipe, inv, start, externs, false)?;
    let restored = store.restore(&snapshot.key, &inv.outputs)?;
    if restored.is_some() {
        let _ = store.refresh_topology(&snapshot.key, &snapshot.topology);
    }
    Ok(restored)
}

fn compile(mut cmd: Command) -> Result<(ExitStatus, Option<Vec<u8>>)> {
    let mut child = cmd
        .stderr(Stdio::piped())
        .spawn()
        .context("starting rustc")?;
    drop(cmd);
    let mut pipe = child.stderr.take().unwrap();
    let mut stderr = io::stderr().lock();
    let mut log = Some(Vec::new());
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = match pipe.read(&mut buf) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            other => other?,
        };
        if n == 0 {
            break;
        }
        stderr.write_all(&buf[..n])?;
        if let Some(bytes) = &mut log {
            if bytes.len() + n <= DIAGNOSTIC_LIMIT {
                bytes.extend_from_slice(&buf[..n]);
            } else {
                log = None;
            }
        }
    }
    Ok((child.wait()?, log))
}

pub fn wrap(compiler: &OsStr, args: &[OsString]) -> Result<i32> {
    if env::var_os("JATO_DISABLE").as_deref() == Some(OsStr::new("1")) {
        return passthrough(original(compiler, args));
    }
    let crate_name = env::var("CARGO_CRATE_NAME").unwrap_or_default();
    let invocation = match Invocation::parse(compiler, args) {
        Ok(v) => v,
        Err(reason) => {
            if !crate_name.is_empty() {
                trace(&crate_name, reason);
                if let Ok(store) = Store::open() {
                    let _ = store.count(&format!("bypass.{reason}"), 1);
                }
            }
            return passthrough(original(compiler, args));
        }
    };
    let inv = &invocation;
    if [
        "RUSTC_BOOTSTRAP",
        "RUSTC_LOG",
        "LD_PRELOAD",
        "LD_AUDIT",
        "RUST_TARGET_PATH",
    ]
    .iter()
    .any(|k| env::var_os(k).is_some_and(|s| !s.is_empty() && (*k != "RUSTC_BOOTSTRAP" || s != "0")))
    {
        trace(&inv.name, "custom compiler environment");
        return passthrough(original(compiler, args));
    }
    let setup =
        Store::open().and_then(|s| Ok((s, fingerprint::recipe(compiler, args, inv.executable)?)));
    let (mut store, recipe) = match setup {
        Ok(v) => v,
        Err(e) => {
            trace(&inv.name, e);
            return passthrough(original(compiler, args));
        }
    };
    match discover(&mut store, compiler, inv, &recipe) {
        Ok(Some(stderr)) => {
            io::stderr().write_all(&stderr)?;
            trace(&inv.name, "hit");
            return Ok(0);
        }
        Ok(None) => (),
        Err(e) => trace(&inv.name, format_args!("compile after lookup: {e:#}")),
    }
    let _ = store.count("miss", 1);
    let since = SystemTime::now();
    let timer = Instant::now();
    let mut native = None;
    let cmd = if inv.executable {
        if let Err(e) = fingerprint::externs(inv, since, &store, false) {
            trace(&inv.name, e);
            return passthrough(original(compiler, args));
        }
        match crate::native::Capture::command(compiler, args) {
            Ok((cmd, capture)) => {
                native = Some(capture);
                cmd
            }
            Err(e) => {
                trace(&inv.name, e);
                return passthrough(original(compiler, args));
            }
        }
    } else {
        original(compiler, args)
    };
    let result = compile(cmd);
    let observed = if let Some(capture) = native {
        let (started, observed) = capture.finish();
        if !started
            && result
                .as_ref()
                .map_or(true, |(status, _)| !status.success())
        {
            return passthrough(original(compiler, args));
        }
        Some(observed)
    } else {
        None
    };
    let (status, diagnostics) = result?;
    let elapsed = timer.elapsed().as_millis() as u64;
    if status.success() {
        let result = (|| -> Result<()> {
            let log = diagnostics.context("diagnostics exceed recording limit")?;
            ensure!(
                inv.confirms_outputs(&log),
                "compiler reported an unexpected artifact set"
            );
            let externs = fingerprint::externs(inv, since, &store, !inv.executable)?;
            let mut plan = fingerprint::dependencies(&inv.outputs[0])?;
            if let Some(observed) = observed {
                plan.native =
                    Some(observed?.manifest(inv, &recipe.root, &recipe.root.join("bin/rustc"))?);
            }
            ensure!(
                store.stable_compiler(compiler, &recipe.toolchain)?,
                "prerelease or custom compiler"
            );
            let snapshot = plan.snapshot(&recipe, inv, since, &externs.key, inv.codegen)?;
            let encoded = serde_json::to_vec(&plan)?;
            ensure!(
                encoded.len() <= 256 * 1024,
                "input manifest exceeds recording limit"
            );
            if !store.save(
                &recipe.key,
                &snapshot.key,
                &inv.outputs,
                &log,
                elapsed,
                Some(Evidence {
                    inputs: &encoded,
                    needs: &externs.needs,
                    topology: &snapshot.topology,
                    codegen: inv.codegen,
                }),
            )? {
                bail!("entry exceeds cache budget");
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                let _ = store.count("stored", 1);
            }
            Err(e) => {
                trace(
                    &inv.name,
                    format_args!("compile succeeded, skipping store: {e:#}"),
                );
                let _ = store.count("uncached", 1);
            }
        }
    }
    Ok(exit_code(status))
}
