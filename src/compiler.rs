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
    plans: Vec<Vec<u8>>,
    flight_hint: &mut Option<Digest>,
) -> Result<Option<Hit>> {
    let before = Instant::now();
    if plans.is_empty() {
        if env::var_os("JATO_LOG").as_deref() == Some(OsStr::new("debug")) {
            if let Some((hash, strong)) = store.pending_requirement(&recipe.key)? {
                let level = if strong {
                    "code generation"
                } else {
                    "metadata"
                };
                trace(
                    &inv.name,
                    format_args!(
                        "record awaits {level} proof for {}",
                        blake3::Hash::from_bytes(hash)
                    ),
                );
            } else {
                trace(
                    &inv.name,
                    "no previous entry for these compiler arguments and toolchain",
                );
            }
        }
        return Ok(None);
    }
    let start = SystemTime::now();
    let mut externs = None;
    let recheck_link = inv.observe_native
        && (inv.incremental.is_some()
            || env::var_os("JATO_WORKTREE_ACTIVE").as_deref() == Some(OsStr::new("1")));
    for encoded in plans {
        let plan: fingerprint::Plan = serde_json::from_slice(&encoded)?;
        if inv.observe_native && plan.native.is_none() {
            continue;
        }
        if let Ok(Some(sample)) = plan.sample(recipe, inv, start)
            && !store.may_match_sample(&recipe.key, &sample)?
        {
            flight_hint.get_or_insert(sample);
            trace(
                &inv.name,
                "large input precheck ruled out recorded variants",
            );
            continue;
        }
        let Ok(inputs) = plan.inputs(recipe, inv, start, None) else {
            continue;
        };
        // Reuse work already done for lookup to keep different branch
        // edits out of the same wait queue.
        flight_hint.get_or_insert(inputs.sources);
        // An edit with new source bytes cannot hit, even if all dependency
        // artifacts match. Send it to rustc before reading those artifacts.
        if !store.has_sources(&inputs.sources)? {
            continue;
        }
        let sources_checked = Instant::now();
        let mut early = recheck_link.then(|| inputs.topology(start)).transpose()?;
        if let Some(current) = &mut early
            && !store.has_source_topology(&inputs.sources, current)?
        {
            if inv.incremental.is_some()
                && let Ok(layout) = inputs.editor_topology(start)
                && store.has_source_topology(&inputs.sources, &layout)?
            {
                *current = layout;
            } else {
                // Resolve unknown layouts before reading the link closure.
                if let Some(hit) = rediscover(store, compiler, inv, recipe, start, None, plan)? {
                    return Ok(Some(hit));
                }
                continue;
            }
        }
        let externs = match &externs {
            Some(key) => key,
            None => externs.insert(fingerprint::externs(inv, start, store, false)?.key),
        };
        let externs_checked = Instant::now();
        let current = match early {
            Some(current) => current,
            None => inputs.topology(start)?,
        };
        let key = match inputs.key(&plan, recipe, inv, start, externs) {
            Ok(key) => key,
            Err(error) => {
                trace(&inv.name, error);
                continue;
            }
        };
        *flight_hint = Some(key);
        let Some(topology) = store.matches_topology(&key, &current)? else {
            continue;
        };
        if topology {
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
        if plan.native.is_some() && !recheck_link {
            continue;
        }
        return rediscover(store, compiler, inv, recipe, start, Some(externs), plan);
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
    externs: Option<&Digest>,
    recorded: fingerprint::Plan,
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
    let mut plan = fingerprint::dependencies(probe.path())?;
    plan.nightly = recorded.nightly;
    plan.extended_admission = recorded.extended_admission;
    plan.reviewed_assembly = recorded.reviewed_assembly;
    if recorded.native.is_some() {
        // A copied source tree has new directory inodes. Ask this rustc to
        // resolve modules again before revalidating the observed link inputs.
        if !plan.same_rust_inputs(&recorded) {
            return Ok(None);
        }
        plan.native = recorded.native;
    }
    let inputs = plan.inputs(recipe, inv, start, None)?;
    let topology = inputs.topology(start)?;
    let checked;
    let externs = match externs {
        Some(digest) => digest,
        None => {
            checked = fingerprint::externs(inv, start, store, false)?.key;
            &checked
        }
    };
    let key = inputs.key(&plan, recipe, inv, start, externs)?;
    let restored = store.materialize(&key, &inv.outputs)?;
    if restored.is_some() {
        let projected = env::var_os("JATO_WORKTREE_ACTIVE").as_deref() == Some(OsStr::new("1"));
        let _ = store.refresh_topology(&key, &topology, projected);
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
    let setup = Store::open().and_then(|mut store| {
        let recipe = fingerprint::recipe(compiler, args, inv.observe_native, &mut store)?;
        Ok((store, recipe))
    });
    let (mut store, recipe) = match setup {
        Ok(v) => v,
        Err(e) => {
            trace(&inv.name, e);
            return passthrough(original(compiler, args));
        }
    };
    let mut ticket = None;
    let mut waited = false;
    loop {
        let mut flight_hint = None;
        let lookup = store.plans(&recipe.key).and_then(|(plans, version)| {
            discover(&mut store, compiler, inv, &recipe, plans, &mut flight_hint)
                .map(|hit| (hit, version))
        });
        let version = match lookup {
            Ok((Some(hit), _)) => {
                io::stderr().write_all(&hit.diagnostics)?;
                let event = if inv.live_incremental {
                    Some("live_incremental_hit")
                } else {
                    waited.then_some("coalesced_hit")
                };
                match store.record_hit(&hit, event) {
                    Ok(()) => trace(&inv.name, "hit"),
                    Err(error) => trace(
                        &inv.name,
                        format_args!("hit (statistics update failed: {error})"),
                    ),
                }
                return Ok(0);
            }
            Ok((None, _)) if inv.live_incremental || ticket.is_some() => break,
            Ok((None, version)) => version,
            Err(e) => {
                trace(&inv.name, format_args!("compile after lookup: {e:#}"));
                break;
            }
        };
        ticket = crate::flight::Ticket::enter(
            &store.dir,
            &recipe.key,
            &inv.source,
            flight_hint.as_ref(),
        );
        let Some(claim) = &ticket else { break };
        if claim.is_owner() {
            // Another compiler may have published between the first miss
            // and this election. Recheck only if a transaction intervened.
            if store.data_version().is_ok_and(|now| now == version) {
                break;
            }
        } else {
            waited = claim.wait();
            if !waited {
                break;
            }
        }
    }
    if inv.live_incremental {
        trace(&inv.name, "active_incremental");
        let _ = store.count("bypass.active_incremental", 1);
        return passthrough(original(compiler, args));
    }
    let _ = store.count("miss", 1);
    let since = SystemTime::now();
    let timer = Instant::now();
    let mut native = None;
    let mut native_inputs = None;
    let cmd = if inv.observe_native {
        if let Err(e) = crate::native::Capture::available(inv, &recipe.root) {
            trace(&inv.name, e);
            return passthrough(original(compiler, args));
        }
        match fingerprint::native_externs(inv, since, &store) {
            Ok(inputs) => native_inputs = Some(inputs),
            Err(e) => {
                trace(&inv.name, e);
                return passthrough(original(compiler, args));
            }
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
    let compiled = Instant::now();
    let elapsed = (compiled - timer).as_millis() as u64;
    if status.success() {
        let result = (|| -> Result<()> {
            let log = diagnostics.context("diagnostics exceed recording limit")?;
            ensure!(
                inv.confirms_outputs(&log),
                "compiler reported an unexpected artifact set"
            );
            let externs = match &native_inputs {
                Some(inputs) => {
                    fingerprint::certify_externs(inputs, inv.codegen, since, &store, false)?
                }
                None => fingerprint::externs(inv, since, &store, true)?,
            };
            let mut plan = fingerprint::dependencies(&inv.outputs[0])?;
            if let Some(observed) = observed {
                plan.native =
                    Some(observed?.manifest(inv, &recipe.root, &recipe.root.join("bin/rustc"))?);
            }
            let channel = store.compiler_channel(compiler, &recipe.toolchain)?;
            ensure!(
                channel != crate::admission::Channel::Unsupported,
                "unsupported compiler channel"
            );
            plan.nightly = channel == crate::admission::Channel::Nightly;
            let inputs = plan.inputs(&recipe, inv, since, Some(compiler))?;
            plan.extended_admission = inputs.extended_admission;
            plan.reviewed_assembly = inputs.reviewed_assembly;
            if let Some(profile) = inputs.origin {
                if inputs.reviewed_assembly {
                    ensure!(
                        profile.verify_dependencies(&store, &externs.digests, &mut 24)?,
                        "assembly input has an unverified source provider"
                    );
                }
                plan.source_origin_v1 = Some(profile.origin(externs.digests)?);
            }
            crate::admission::verify_conditions(compiler, inv, &store.dir, &inputs.nightly_guards)?;
            plan.large_input = inputs.large_input;
            plan.large_extern = externs.largest;
            let sample = plan.sample(&recipe, inv, since).unwrap_or(None);
            let sources = inputs.sources;
            let topology = inputs.topology(since)?;
            let editor = if inv.observe_native && inv.incremental.is_some() {
                inputs.editor_topology(since).ok()
            } else {
                None
            };
            let key = inputs.key(&plan, &recipe, inv, since, &externs.key)?;
            let encoded = serde_json::to_vec(&plan)?;
            ensure!(
                encoded.len() <= 256 * 1024,
                "input manifest exceeds recording limit"
            );
            let validated = Instant::now();
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
                    sample,
                    codegen: inv.codegen,
                }),
            )? {
                bail!("entry exceeds cache budget");
            }
            if let Some(layout) = editor {
                let _ = store.refresh_topology(&key, &layout, true);
            }
            if env::var_os("JATO_LOG").as_deref() == Some(OsStr::new("timings")) {
                eprintln!(
                    "jato [{}] compile {} us, validate {} us, store {} us",
                    inv.name,
                    (compiled - timer).as_micros(),
                    (validated - compiled).as_micros(),
                    validated.elapsed().as_micros()
                );
            }
            Ok(())
        })();
        if let Err(e) = result {
            trace(
                &inv.name,
                format_args!("compile succeeded, skipping store: {e:#}"),
            );
            let _ = store.count("uncached", 1);
        }
    }
    Ok(exit_code(status))
}
