use crate::fingerprint;
use crate::invocation::Invocation;
use crate::store::{DIAGNOSTIC_LIMIT, Digest, Evidence, Hit, Store};
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
) -> Result<Option<Hit>> {
    let before = Instant::now();
    let plans = store.plans(&recipe.key)?;
    if plans.is_empty() {
        trace(
            &inv.name,
            "no previous entry for these compiler arguments and toolchain",
        );
        return Ok(None);
    }
    let start = SystemTime::now();
    let mut externs = None;
    for encoded in plans {
        let plan: fingerprint::Plan = serde_json::from_slice(&encoded)?;
        if inv.observe_native && plan.native.is_none() {
            continue;
        }
        let Ok(inputs) = plan.inputs(recipe, inv, start, false) else {
            continue;
        };
        // An edit with new source bytes cannot hit, even if all dependency
        // artifacts match. Send it to rustc before reading those artifacts.
        if !store.has_sources(&inputs.sources)? {
            continue;
        }
        let sources_checked = Instant::now();
        let externs = match &externs {
            Some(key) => key,
            None => externs.insert(fingerprint::externs(inv, start, store, false)?.key),
        };
        let externs_checked = Instant::now();
        let current = inputs.topology(start)?;
        let key = match inputs.key(&plan, recipe, inv, start, externs) {
            Ok(key) => key,
            Err(error) => {
                trace(&inv.name, error);
                continue;
            }
        };
        let Some(topology) = store.topology(&key)? else {
            continue;
        };
        if topology == current {
            let restore = Instant::now();
            let result = store.materialize(&key, &inv.outputs)?;
            if env::var_os("JATO_LOG").as_deref() == Some(OsStr::new("timings")) {
                eprintln!(
                    "jato [{}] sources {} us, externs {} us, namespace and link {} us, restore {} us",
                    inv.name,
                    (sources_checked - before).as_micros(),
                    (externs_checked - sources_checked).as_micros(),
                    (restore - externs_checked).as_micros(),
                    restore.elapsed().as_micros()
                );
            }
            return Ok(result);
        }
        // Familiar bytes with a changed module search directory need fresh
        // name resolution. Ordinary edits go directly to the actual compiler.
        if plan.native.is_some() {
            continue;
        }
        return rediscover(store, compiler, inv, recipe, start, externs);
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
) -> Result<Option<Hit>> {
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
    let inputs = plan.inputs(recipe, inv, start, false)?;
    let topology = inputs.topology(start)?;
    let key = inputs.key(&plan, recipe, inv, start, externs)?;
    let restored = store.materialize(&key, &inv.outputs)?;
    if restored.is_some() {
        let _ = store.refresh_topology(&key, &topology);
    }
    Ok(restored)
}

struct Compiled {
    status: ExitStatus,
    diagnostics: Option<Vec<u8>>,
    early: Vec<u8>,
}

fn compile(mut cmd: Command, capture: Option<&crate::native::Capture>) -> Result<Compiled> {
    let mut child = cmd
        .stderr(Stdio::piped())
        .spawn()
        .context("starting rustc")?;
    drop(cmd);
    let mut pipe = child.stderr.take().unwrap();
    let mut log = Some(Vec::new());
    let mut early = Vec::new();
    let mut buf = [0; 16 * 1024];
    let mut stderr = io::stderr().lock();
    let streamed = (|| -> io::Result<()> {
        loop {
            let n = match pipe.read(&mut buf) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            if n == 0 {
                return Ok(());
            }
            // Keep attach errors out of Cargo diagnostics when a restricted
            // kernel requires a retry without the observer.
            if early.len() + n <= 64 * 1024 && capture.is_some_and(|c| !c.started()) {
                early.extend_from_slice(&buf[..n]);
            } else {
                if !early.is_empty() {
                    stderr.write_all(&early)?;
                    early.clear();
                }
                stderr.write_all(&buf[..n])?;
            }
            if let Some(bytes) = &mut log {
                if bytes.len() + n <= DIAGNOSTIC_LIMIT {
                    bytes.extend_from_slice(&buf[..n]);
                } else {
                    log = None;
                }
            }
        }
    })();
    drop(pipe);
    if let Err(e) = streamed {
        let _ = child.kill();
        let _ = child.wait();
        return Err(e.into());
    }
    Ok(Compiled {
        status: child.wait()?,
        diagnostics: log,
        early,
    })
}

pub fn wrap(compiler: &OsStr, args: &[OsString]) -> Result<i32> {
    if env::var_os("JATO_DISABLE").as_deref() == Some(OsStr::new("1")) {
        return passthrough(original(compiler, args));
    }
    let crate_name = env::var("CARGO_CRATE_NAME").unwrap_or_default();
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
        trace(&crate_name, "custom compiler environment");
        return passthrough(original(compiler, args));
    }
    if let Some(code) = crate::probe::run(compiler, args)? {
        return Ok(code);
    }
    crate::content::inherit_jobs();
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
    let setup = Store::open()
        .and_then(|s| Ok((s, fingerprint::recipe(compiler, args, inv.observe_native)?)));
    let (mut store, recipe) = match setup {
        Ok(v) => v,
        Err(e) => {
            trace(&inv.name, e);
            return passthrough(original(compiler, args));
        }
    };
    match discover(&mut store, compiler, inv, &recipe) {
        Ok(Some(hit)) => {
            // Cargo can schedule dependent units as soon as their verified
            // metadata notification arrives, even with a busy WAL writer.
            io::stderr().write_all(&hit.diagnostics)?;
            let _ = store.record_hit(&hit);
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
    let cmd = if inv.observe_native {
        if let Err(e) = crate::native::Capture::available(inv, &recipe.root) {
            trace(&inv.name, e);
            return passthrough(original(compiler, args));
        }
        if let Err(e) = fingerprint::externs(inv, since, &store, false) {
            trace(&inv.name, e);
            return passthrough(original(compiler, args));
        }
        match crate::native::Capture::command(compiler, args, inv) {
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
    let result = compile(cmd, native.as_ref());
    let observed = if let Some(capture) = native {
        let (started, observed) = capture.finish();
        if !started && result.as_ref().map_or(true, |out| !out.status.success()) {
            return passthrough(original(compiler, args));
        }
        Some(observed)
    } else {
        None
    };
    let Compiled {
        status,
        diagnostics,
        early,
    } = result?;
    if !early.is_empty() {
        io::stderr().write_all(&early)?;
    }
    let elapsed = timer.elapsed().as_millis() as u64;
    if status.success() {
        let result = (|| -> Result<()> {
            let log = diagnostics.context("diagnostics exceed recording limit")?;
            ensure!(
                inv.confirms_outputs(&log),
                "compiler reported an unexpected artifact set"
            );
            let externs = fingerprint::externs(inv, since, &store, !inv.observe_native)?;
            let mut plan = fingerprint::dependencies(&inv.outputs[0])?;
            if let Some(observed) = observed {
                plan.native =
                    Some(observed?.manifest(inv, &recipe.root, &recipe.root.join("bin/rustc"))?);
            }
            ensure!(
                store.stable_compiler(compiler, &recipe.toolchain)?,
                "prerelease or custom compiler"
            );
            let inputs = plan.inputs(&recipe, inv, since, inv.codegen)?;
            let sources = inputs.sources;
            let topology = inputs.topology(since)?;
            let key = inputs.key(&plan, &recipe, inv, since, &externs.key)?;
            let encoded = serde_json::to_vec(&plan)?;
            ensure!(
                encoded.len() <= 256 * 1024,
                "input manifest exceeds recording limit"
            );
            if !store.save(
                &recipe.key,
                &key,
                &inv.outputs,
                &log,
                elapsed,
                Some(Evidence {
                    inputs: &encoded,
                    needs: &externs.needs,
                    topology: &topology,
                    sources: &sources,
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
