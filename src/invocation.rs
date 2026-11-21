use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

pub struct Invocation {
    pub name: String,
    pub source: PathBuf,
    pub externs: Vec<PathBuf>,
    extern_names: Vec<String>,
    pub outputs: Vec<PathBuf>, // dep-info is always first
    pub probe_args: Vec<OsString>,
}

impl Invocation {
    pub fn has_extern(&self, name: &str) -> bool {
        matches!(
            name,
            "self" | "core" | "alloc" | "std" | "test" | "proc_macro"
        ) || self.extern_names.iter().any(|s| s == name)
    }

    pub fn parse(compiler: &OsStr, args: &[OsString]) -> Result<Self, &'static str> {
        if Path::new(compiler).file_name() != Some(OsStr::new("rustc")) {
            return Err("other_driver");
        }
        let mut name = None;
        let mut suffix = "";
        let mut crate_type = None;
        let mut test = false;
        let mut emit = None;
        let mut out_dir = None;
        let mut source = None;
        let mut externs = Vec::new();
        let mut extern_names = Vec::new();
        let mut probe_args = Vec::new();
        let mut i = 0;
        while i < args.len() {
            let start = i;
            let arg = args[i].to_str().ok_or("non_utf8_arguments")?;
            i += 1;
            let (flag, inline) = if arg.starts_with("--") {
                arg.split_once('=')
                    .map_or((arg, None), |(k, v)| (k, Some(v)))
            } else if ["-C", "-L", "-A", "-W", "-D", "-F"]
                .iter()
                .any(|p| arg.starts_with(p))
            {
                (&arg[..2], (arg.len() > 2).then_some(&arg[2..]))
            } else {
                (arg, None)
            };
            let mut value = || {
                if let Some(v) = inline {
                    return Ok(v);
                }
                let v = args
                    .get(i)
                    .and_then(|v| v.to_str())
                    .ok_or("unknown_arguments")?;
                i += 1;
                Ok(v)
            };
            let mut probe = true;
            match flag {
                "--crate-name" => name = Some(value()?),
                "--crate-type" => crate_type = Some(value()?),
                "--emit" => {
                    emit = Some(value()?);
                    probe = false;
                }
                "--out-dir" => out_dir = Some(PathBuf::from(value()?)),
                "--test" => test = true,
                "--extern" => {
                    let arg = value()?;
                    if matches!(arg, "proc_macro" | "std" | "alloc" | "core" | "test") {
                        probe_args.extend_from_slice(&args[start..i]);
                        continue;
                    }
                    let (name, path) = arg.split_once('=').ok_or("implicit_extern")?;
                    extern_names.push(name.rsplit(':').next().unwrap().to_owned());
                    let path = PathBuf::from(path);
                    if !matches!(
                        path.extension().and_then(OsStr::to_str),
                        Some("rlib" | "rmeta")
                    ) {
                        return Err("opaque_extern");
                    }
                    externs.push(path);
                }
                "-L" => {
                    if !value()?.starts_with("dependency=") {
                        return Err("native_search_path");
                    }
                }
                "-C" => {
                    let (setting, v) = value()?.split_once('=').ok_or("codegen_option")?;
                    match setting {
                        "incremental" => probe = false,
                        "extra-filename" => suffix = v,
                        "split-debuginfo" if v == "off" => (),
                        "target-cpu" if v != "native" => (),
                        "metadata"
                        | "opt-level"
                        | "embed-bitcode"
                        | "debuginfo"
                        | "debug-assertions"
                        | "overflow-checks"
                        | "panic"
                        | "codegen-units"
                        | "strip"
                        | "lto"
                        | "target-feature"
                        | "relocation-model"
                        | "symbol-mangling-version"
                        | "force-frame-pointers"
                        | "force-unwind-tables" => (),
                        _ => return Err("codegen_option"),
                    }
                }
                "--target" => {
                    let target = value()?;
                    if target.ends_with(".json") || !safe_component(target) {
                        return Err("custom_target");
                    }
                }
                "--cfg" | "--check-cfg" | "--edition" | "--cap-lints" | "--allow" | "--warn"
                | "--deny" | "--forbid" | "--force-warn" | "-A" | "-W" | "-D" | "-F"
                | "--error-format" | "--color" | "--diagnostic-width" => {
                    value()?;
                }
                "--json" => {
                    if !value()?.split(',').all(|v| {
                        matches!(
                            v,
                            "artifacts"
                                | "diagnostic-rendered-ansi"
                                | "future-incompat"
                                | "unused-externs"
                                | "unused-externs-silent"
                        )
                    }) {
                        return Err("diagnostic_option");
                    }
                }
                "-vV" | "--version" | "--print" => return Err("compiler_query"),
                _ if !arg.starts_with(['-', '@']) && arg.ends_with(".rs") && source.is_none() => {
                    source = Some(PathBuf::from(arg))
                }
                _ => return Err("unknown_arguments"),
            }
            if probe {
                probe_args.extend_from_slice(&args[start..i]);
            }
        }
        let name = name.filter(|v| safe_component(v)).ok_or("compiler_query")?;
        if !suffix.is_empty() && !safe_component(suffix) {
            return Err("custom_output");
        }
        let dir = out_dir.ok_or("custom_output")?;
        let kinds: Vec<_> = emit.ok_or("custom_output")?.split(',').collect();
        if !kinds.contains(&"dep-info")
            || !kinds
                .iter()
                .all(|v| matches!(*v, "dep-info" | "metadata" | "link"))
        {
            return Err("custom_output");
        }
        let library = matches!(crate_type, Some("lib" | "rlib"));
        if !library && !(test || crate_type == Some("bin")) {
            return Err("crate_type");
        }
        if kinds.contains(&"link") && (!library || test) {
            return Err("linker");
        }
        if !kinds.contains(&"link") && !kinds.contains(&"metadata") {
            return Err("custom_output");
        }
        let stem = format!("{name}{suffix}");
        let mut outputs = vec![dir.join(format!("{stem}.d"))];
        if kinds.contains(&"metadata") {
            outputs.push(dir.join(format!("lib{stem}.rmeta")));
        }
        if kinds.contains(&"link") {
            outputs.push(dir.join(format!("lib{stem}.rlib")));
        }
        Ok(Self {
            name: name.into(),
            source: source.ok_or("missing_source")?,
            externs,
            extern_names,
            outputs,
            probe_args,
        })
    }
}

fn safe_component(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}
