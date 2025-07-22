use std::ffi::{OsStr, OsString};
use std::fs::{remove_file, File};
use std::io::Write;
use std::mem::take;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::{ascii, env, io};

use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use target::spec::{LinkerFlavor, Target};

pub fn link(
    path: Option<Utf8PathBuf>,
    target: &Target,
    out_filename: &Utf8Path,
    add_objects: impl FnOnce(&mut dyn Linker),
) -> Result<()> {
    let mut linker = linker_with_args(path, target, out_filename, add_objects);

    let import_lib_path = out_filename.with_file_name("__openvaf__import.lib");
    if !target.options.import_lib.is_empty() {
        let mut file = File::create(&import_lib_path).context("failed to create importlib")?;
        file.write_all(target.options.import_lib).context("failed to write importlib")?;
        linker.add_object(&import_lib_path);
    }
    let res = exec_linker(linker.take_cmd(), out_filename);
    if !target.options.import_lib.is_empty() {
        remove_file(import_lib_path).context("failed to delete importlib")?;
    }
    match res {
        Ok(prog) if !prog.status.success() => {
            let mut output = prog.stderr.clone();
            output.extend_from_slice(&prog.stdout);
            let escaped_output = escape_stdout_stderr_string(&output);
            eprintln!("{}", escaped_output);
            bail!("linking failed (see linker output for details)")
        }
        Ok(_) => Ok(()),
        Err(err) => bail!("linker not found: {}", err),
    }
}

fn escape_stdout_stderr_string(s: &[u8]) -> String {
    std::str::from_utf8(s).map(|s| s.to_owned()).unwrap_or_else(|_| {
        let mut x = "Non-UTF-8 output: ".to_string();
        x.extend(s.iter().flat_map(|&b| ascii::escape_default(b)).map(char::from));
        x
    })
}

fn disable_localization(linker: &mut Command) {
    linker.env("LC_ALL", "C");
    linker.env("VSLANG", "1033");
}

fn linker_with_args<'a>(
    path: Option<Utf8PathBuf>,
    target: &'a Target,
    out_filename: &Utf8Path,
    add_objects: impl FnOnce(&mut dyn Linker),
) -> Box<dyn Linker + 'a> {
    let flavor = target.options.linker_flavor;
    // The `flavor` is passed to `get_linker` but is no longer used to switch logic.
    let mut cmd = get_linker(path.map(|path| path.into_std_path_buf()), flavor, target);
    disable_localization(cmd.cmd());
    cmd.cmd().env("ZERO_AR_DATE", "1");

    cmd.add_pre_link_args(target, flavor);

    add_objects(&mut *cmd);
    cmd.output_filename(out_filename);
    cmd.set_output_kind();

    cmd.add_post_link_args(target, flavor);

    cmd
}

/// Creates a linker command using a unified approach.
///
/// It uses the provided `path` or defaults to `clang` as a universal compiler driver
/// for linking. This abstracts away the differences between platform-native linkers.
fn get_linker<'a>(
    path: Option<PathBuf>,
    _flavor: LinkerFlavor, // No longer used to switch logic
    target: &'a Target,
) -> Box<dyn Linker + 'a> {
    // Use the provided path, or default to "clang" as the unified driver.
    let linker_path = path.unwrap_or_else(|| "clang".into());
    let cmd = Command::new(linker_path);

    // We always use LdLinker, as compiler drivers like clang understand
    // its arguments (`-o`, `-shared`) on all platforms.
    Box::new(LdLinker { cmd, target }) as Box<dyn Linker>
}

fn exec_linker(mut cmd: std::process::Command, _out_filename: &Utf8Path) -> io::Result<Output> {
    match cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(child) => {
            let output = child.wait_with_output();
            
            #[cfg(windows)]
            if let Ok(of) = std::fs::OpenOptions::new().write(true).open(_out_filename) {
                of.sync_all()?;
            }

            output
        }
        Err(e) => Err(e),
    }
}

pub trait Linker {
    fn cmd(&mut self) -> &mut Command;
    fn output_filename(&mut self, path: &Utf8Path);
    fn add_object(&mut self, path: &Utf8Path);
    fn set_output_kind(&mut self);
}

impl dyn Linker + '_ {
    pub fn args<I: AsRef<OsStr>>(&mut self, args: impl IntoIterator<Item = I>) {
        self.cmd().args(args);
    }

    pub fn add_pre_link_args(&mut self, target: &Target, flavor: LinkerFlavor) {
        if let Some(args) = target.options.pre_link_args.get(&flavor) {
            self.args(args);
        }
    }

    pub fn add_post_link_args(&mut self, target: &Target, flavor: LinkerFlavor) {
        if let Some(args) = target.options.post_link_args.get(&flavor) {
            self.args(args);
        }
        if let Ok(flags) = std::env::var("OPENVAF_LDFLAGS") {
            let flags = flags
                .split(' ')
                .filter(|flag| !flag.is_empty() && !flag.chars().all(|c| c.is_whitespace()));
            self.args(flags)
        }
    }

    pub fn take_cmd(&mut self) -> std::process::Command {
        let cmd = self.cmd();
        let mut res = std::process::Command::new(cmd.command.as_os_str());
        res.args(cmd.args.iter()).envs(take(&mut cmd.env));
        res
    }
}

pub struct LdLinker<'a> {
    cmd: Command,
    target: &'a Target,
}

impl<'a> LdLinker<'a> {
    fn linker_arg(&mut self, arg: &str) -> &mut Self {
        // When using a compiler driver like clang, linker-specific arguments
        // would typically be passed with `-Wl,`. For common arguments like `-shared`
        // or `-o`, the driver understands them directly.
        self.cmd.arg(arg);
        self
    }

    fn build_dylib(&mut self) {
        if self.target.options.is_like_osx {
            self.linker_arg("-dylib");
        } else {
            self.linker_arg("-shared");
        }
    }
}

impl<'a> Linker for LdLinker<'a> {
    fn cmd(&mut self) -> &mut Command {
        &mut self.cmd
    }

    fn output_filename(&mut self, path: &Utf8Path) {
        self.cmd.arg("-o").arg(path.as_str());
    }

    fn add_object(&mut self, path: &Utf8Path) {
        self.cmd.arg(path.as_str());
    }

    fn set_output_kind(&mut self) {
        self.build_dylib();
    }
}

pub struct Command {
    command: PathBuf,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
}

impl Command {
    fn new(command: PathBuf) -> Command {
        Command { command, args: Vec::new(), env: Vec::new() }
    }
    fn args<I: AsRef<OsStr>>(&mut self, args: impl IntoIterator<Item = I>) {
        self.args.extend(args.into_iter().map(|arg| arg.as_ref().to_owned()))
    }

    fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    fn env(&mut self, env: impl AsRef<OsStr>, val: impl AsRef<OsStr>) -> &mut Self {
        self.env.push((env.as_ref().to_owned(), val.as_ref().to_owned()));
        self
    }
}