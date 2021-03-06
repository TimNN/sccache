// Copyright 2016 Mozilla Foundation
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use ::compiler::{
    Cacheable,
    Compiler,
    CompilerArguments,
    ParsedArguments,
    run_input_output,
    write_temp_file,
};
use local_encoding::{Encoding, Encoder};
use log::LogLevel::{Debug, Trace};
use futures::future::{self, Future};
use futures_cpupool::CpuPool;
use mock_command::{
    CommandCreatorSync,
    RunCommand,
};
use std::collections::{HashMap,HashSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{
    self,
    Write,
};
use std::path::Path;
use std::process::{self,Stdio};

use errors::*;

fn from_local_codepage(bytes: &Vec<u8>) -> io::Result<String> {
    Encoding::OEM.to_string(bytes)
}

/// Detect the prefix included in the output of MSVC's -showIncludes output.
pub fn detect_showincludes_prefix<T>(creator: &T, exe: &OsStr, pool: &CpuPool)
                                     -> SFuture<String>
    where T: CommandCreatorSync
{
    let write = write_temp_file(pool,
                                "test.c".as_ref(),
                                b"#include <stdio.h>\n".to_vec());

    let exe = exe.to_os_string();
    let mut creator = creator.clone();
    let output = write.and_then(move |(tempdir, input)| {
        let mut cmd = creator.new_command_sync(&exe);
        cmd.args(&["-nologo", "-showIncludes", "-c", "-Fonul"])
            .arg(&input)
            // The MSDN docs say the -showIncludes output goes to stderr,
            // but that's not true unless running with -E.
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        if log_enabled!(Trace) {
            trace!("detect_showincludes_prefix: {:?}", cmd);
        }

        run_input_output(cmd, None).map(|e| {
            drop(tempdir);
            e
        })
    });

    Box::new(output.and_then(|output| {
        if !output.status.success() {
            bail!("Failed to detect showIncludes prefix")
        }

        let process::Output { stdout: stdout_bytes, .. } = output;
        let stdout = try!(from_local_codepage(&stdout_bytes));
        for line in stdout.lines() {
            if line.ends_with("stdio.h") {
                for (i, c) in line.char_indices().rev() {
                    if c == ' ' {
                        // See if the rest of this line is a full pathname.
                        if Path::new(&line[i+1..]).exists() {
                            // Everything from the beginning of the line
                            // to this index is the prefix.
                            return Ok(line[..i+1].to_owned());
                        }
                    }
                }
            }
        }

        bail!("Failed to detect showIncludes prefix")
    }))
}

pub fn parse_arguments(arguments: &[String]) -> CompilerArguments {
    let mut output_arg = None;
    let mut input_arg = None;
    let mut common_args = vec!();
    let mut compilation = false;
    let mut debug_info = false;
    let mut pdb = None;
    let mut depfile = None;

    //TODO: support arguments that start with / as well.
    let mut it = arguments.iter();
    loop {
        match it.next() {
            Some(arg) => {
                match arg.as_ref() {
                    "-c" => compilation = true,
                    v if v.starts_with("-Fo") => {
                        output_arg = Some(String::from(&v[3..]));
                    }
                    // Arguments that take a value.
                    "-FI" => {
                        common_args.push(arg.clone());
                        if let Some(arg_val) = it.next() {
                            common_args.push(arg_val.clone());
                        }
                    }
                    v @ _ if v.starts_with("-deps") => {
                        depfile = Some(v[5..].to_owned());
                    }
                    // Arguments we can't handle.
                    "-showIncludes" => return CompilerArguments::CannotCache,
                    a if a.starts_with('@') => return CompilerArguments::CannotCache,
                    // Arguments we can't handle because they output more files.
                    // TODO: support more multi-file outputs.
                    "-FA" | "-Fa" | "-Fe" | "-Fm" | "-Fp" | "-FR" | "-Fx" => return CompilerArguments::CannotCache,
                    "-Zi" => {
                        debug_info = true;
                        common_args.push(arg.clone());
                    }
                    v if v.starts_with("-Fd") => {
                        pdb = Some(String::from(&v[3..]));
                        common_args.push(arg.clone());
                    }
                    // Other options.
                    v if v.starts_with('-') && v.len() > 1 => {
                        common_args.push(arg.clone());
                    }
                    // Anything else is an input file.
                    v => {
                        if input_arg.is_some() {
                            // Can't cache compilations with multiple inputs.
                            return CompilerArguments::CannotCache;
                        }
                        input_arg = Some(v);
                    }
                }
            }
            None => break,
        }
    }
    // We only support compilation.
    if !compilation {
        return CompilerArguments::NotCompilation;
    }
    let (input, extension) = match input_arg {
        Some(i) => {
            match Path::new(i).extension().and_then(|e| e.to_str()) {
                Some(e) => (i.to_owned(), e.to_owned()),
                _ => {
                    trace!("Bad or missing source extension: {:?}", i);
                    return CompilerArguments::CannotCache;
                }
            }
        }
        // We can't cache compilation without an input.
        None => return CompilerArguments::CannotCache,
    };
    let mut outputs = HashMap::new();
    match output_arg {
        // We can't cache compilation that doesn't go to a file
        None => return CompilerArguments::CannotCache,
        Some(o) => {
            outputs.insert("obj", o.to_owned());
            // -Fd is not taken into account unless -Zi is given
            if debug_info {
                match pdb {
                    Some(p) => outputs.insert("pdb", p.to_owned()),
                    None => {
                        // -Zi without -Fd defaults to vcxxx.pdb (where xxx depends on the
                        // MSVC version), and that's used for all compilations with the same
                        // working directory. We can't cache such a pdb.
                        return CompilerArguments::CannotCache;
                    }
                };
            }
        }
    }
    CompilerArguments::Ok(ParsedArguments {
        input: input,
        extension: extension,
        depfile: depfile,
        outputs: outputs,
        preprocessor_args: vec!(),
        common_args: common_args,
    })
}

#[cfg(windows)]
fn normpath(path: &str) -> String {
    use kernel32;
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::ptr;
    use std::os::windows::io::AsRawHandle;
    File::open(path)
        .and_then(|f| {
            let handle = f.as_raw_handle();
            let size = unsafe { kernel32::GetFinalPathNameByHandleW(handle,
                                                         ptr::null_mut(),
                                                         0,
                                                         0)
            };
            if size == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut wchars = Vec::with_capacity(size as usize);
            wchars.resize(size as usize, 0);
            if unsafe { kernel32::GetFinalPathNameByHandleW(handle,
                                                            wchars.as_mut_ptr(),
                                                            wchars.len() as u32,
                                                            0) } == 0 {
                return Err(io::Error::last_os_error());
            }
            // The return value of GetFinalPathNameByHandleW uses the
            // '\\?\' prefix.
            let o = OsString::from_wide(&wchars[4..wchars.len() - 1]);
            o.into_string()
                .map(|s| s.replace('\\', "/"))
                .or(Err(io::Error::new(io::ErrorKind::Other, "Error converting string")))
        })
        .unwrap_or(path.replace('\\', "/"))
}

#[cfg(not(windows))]
fn normpath(path: &str) -> String {
    path.to_owned()
}

pub fn preprocess<T>(creator: &T,
                     compiler: &Compiler,
                     parsed_args: &ParsedArguments,
                     cwd: &str,
                     includes_prefix: &str,
                     _pool: &CpuPool)
                     -> SFuture<process::Output>
    where T: CommandCreatorSync
{
    let mut cmd = creator.clone().new_command_sync(&compiler.executable);
    cmd.arg("-E")
        .arg(&parsed_args.input)
        .arg("-nologo")
        .args(&parsed_args.common_args)
        .current_dir(&cwd);
    if parsed_args.depfile.is_some() {
        cmd.arg("-showIncludes");
    }

    if log_enabled!(Debug) {
        debug!("preprocess: {:?}", cmd);
    }

    let parsed_args = parsed_args.clone();
    let includes_prefix = includes_prefix.to_string();
    let cwd = cwd.to_string();

    Box::new(run_input_output(cmd, None).and_then(move |output| {
        let parsed_args = &parsed_args;
        if let (Some(ref objfile), &Some(ref depfile)) = (parsed_args.outputs.get("obj"), &parsed_args.depfile) {
            let mut f = File::create(Path::new(&cwd).join(depfile))?;
            write!(f, "{}: {} ", objfile, parsed_args.input)?;
            let process::Output { status, stdout, stderr: stderr_bytes } = output;
            let stderr = from_local_codepage(&stderr_bytes)?;
            let mut deps = HashSet::new();
            let mut stderr_bytes = vec!();
            for line in stderr.lines() {
                if line.starts_with(&includes_prefix) {
                    let dep = normpath(line[includes_prefix.len()..].trim());
                    trace!("included: {}", dep);
                    if deps.insert(dep.clone()) && !dep.contains(' ') {
                        write!(f, "{} ", dep)?;
                    }
                } else {
                    stderr_bytes.extend_from_slice(line.as_bytes());
                    stderr_bytes.push(b'\n');
                }
            }
            writeln!(f, "")?;
            // Write extra rules for each dependency to handle
            // removed files.
            writeln!(f, "{}:", parsed_args.input)?;
            let mut sorted = deps.into_iter().collect::<Vec<_>>();
            sorted.sort();
            for dep in sorted {
                if !dep.contains(' ') {
                    writeln!(f, "{}:", dep)?;
                }
            }
            Ok(process::Output { status: status, stdout: stdout, stderr: stderr_bytes })
        } else {
            Ok(output)
        }
    }))
}

pub fn compile<T>(creator: &T,
                  compiler: &Compiler,
                  preprocessor_output: Vec<u8>,
                  parsed_args: &ParsedArguments,
                  cwd: &str,
                  pool: &CpuPool)
                  -> SFuture<(Cacheable, process::Output)>
    where T: CommandCreatorSync
{
    trace!("compile");
    let out_file = match parsed_args.outputs.get("obj") {
        Some(obj) => obj,
        None => {
            return future::err("Missing object file output".into()).boxed()
        }
    };

    // See if this compilation will produce a PDB.
    let cacheable = parsed_args.outputs.get("pdb")
        .map_or(Cacheable::Yes, |pdb| {
            // If the PDB exists, we don't know if it's shared with another
            // compilation. If it is, we can't cache.
            if Path::new(&cwd).join(pdb).exists() {
                Cacheable::No
            } else {
                Cacheable::Yes
            }
        });

    // MSVC doesn't read anything from stdin, so it needs a temporary file
    // as input.
    let write = {
        let filename = match Path::new(&parsed_args.input).file_name() {
            Some(name) => name,
            None => return future::err("missing input filename".into()).boxed(),
        };
        write_temp_file(pool, filename.as_ref(), preprocessor_output)
    };

    let mut cmd = creator.clone().new_command_sync(&compiler.executable);
    cmd.arg("-c")
        .arg(&format!("-Fo{}", out_file))
        .args(&parsed_args.common_args)
        .current_dir(&cwd);
    let output = write.and_then(move |(tempdir, input)| {
        cmd.arg(input);
        debug!("compile: {:?}", cmd);
        run_input_output(cmd, None).map(|e| {
            drop(tempdir);
            e
        })
    });

    // Sometimes MSVC can't handle compiling from the preprocessed source,
    // so have a fallback path that compiles from the original input file.
    //
    // We may just throw away this `cmd` if our execution turns out to be
    // successful.
    let mut cmd = creator.clone().new_command_sync(&compiler.executable);
    cmd.arg("-c")
        .arg(&parsed_args.input)
        .arg(&format!("-Fo{}", out_file))
        .args(&parsed_args.common_args)
        .current_dir(cwd);
    Box::new(output.and_then(move |output| -> SFuture<_> {
        if output.status.success() {
            future::ok((cacheable, output)).boxed()
        } else {
            debug!("compile: {:?}", cmd);
            Box::new(run_input_output(cmd, None).map(|output| {
                (cacheable, output)
            }))
        }
    }))
}


#[cfg(test)]
mod test {
    use ::compiler::*;
    use env_logger;
    use futures::Future;
    use futures_cpupool::CpuPool;
    use mock_command::*;
    use std::collections::HashMap;
    use super::*;
    use test::utils::*;

    #[test]
    fn test_detect_showincludes_prefix() {
        drop(env_logger::init());
        let creator = new_creator();
        let pool = CpuPool::new(1);
        let f = TestFixture::new();
        let srcfile = f.touch("stdio.h").unwrap();
        let mut s = srcfile.to_str().unwrap();
        if s.starts_with("\\\\?\\") {
            s = &s[4..];
        }
        let stdout = format!("blah: {}\r\n", s);
        let stderr = String::from("some\r\nstderr\r\n");
        next_command(&creator, Ok(MockChild::new(exit_status(0), &stdout, &stderr)));
        assert_eq!("blah: ", detect_showincludes_prefix(&creator, "cl.exe".as_ref(), &pool).wait().unwrap());
    }

    #[test]
    fn test_parse_arguments_simple() {
        match parse_arguments(&stringvec!["-c", "foo.c", "-Fofoo.obj"]) {
            CompilerArguments::Ok(ParsedArguments { input, extension, depfile: _depfile, outputs, preprocessor_args, common_args }) => {
                assert!(true, "Parsed ok");
                assert_eq!("foo.c", input);
                assert_eq!("c", extension);
                assert_map_contains!(outputs, ("obj", "foo.obj"));
                //TODO: fix assert_map_contains to assert no extra keys!
                assert_eq!(1, outputs.len());
                assert!(preprocessor_args.is_empty());
                assert!(common_args.is_empty());
            }
            o @ _ => assert!(false, format!("Got unexpected parse result: {:?}", o)),
        }
    }

    #[test]
    fn test_parse_arguments_extra() {
        match parse_arguments(&stringvec!["-c", "foo.c", "-foo", "-Fofoo.obj", "-bar"]) {
            CompilerArguments::Ok(ParsedArguments { input, extension, depfile: _depfile, outputs, preprocessor_args, common_args }) => {
                assert!(true, "Parsed ok");
                assert_eq!("foo.c", input);
                assert_eq!("c", extension);
                assert_map_contains!(outputs, ("obj", "foo.obj"));
                //TODO: fix assert_map_contains to assert no extra keys!
                assert_eq!(1, outputs.len());
                assert!(preprocessor_args.is_empty());
                assert_eq!(common_args, &["-foo", "-bar"]);
            }
            o @ _ => assert!(false, format!("Got unexpected parse result: {:?}", o)),
        }
    }

    #[test]
    fn test_parse_arguments_values() {
        match parse_arguments(&stringvec!["-c", "foo.c", "-FI", "file", "-Fofoo.obj"]) {
            CompilerArguments::Ok(ParsedArguments { input, extension, depfile: _depfile, outputs, preprocessor_args, common_args }) => {
                assert!(true, "Parsed ok");
                assert_eq!("foo.c", input);
                assert_eq!("c", extension);
                assert_map_contains!(outputs, ("obj", "foo.obj"));
                //TODO: fix assert_map_contains to assert no extra keys!
                assert_eq!(1, outputs.len());
                assert!(preprocessor_args.is_empty());
                assert_eq!(common_args, &["-FI", "file"]);
            }
            o @ _ => assert!(false, format!("Got unexpected parse result: {:?}", o)),
        }
    }

    #[test]
    fn test_parse_arguments_pdb() {
        match parse_arguments(&stringvec!["-c", "foo.c", "-Zi", "-Fdfoo.pdb", "-Fofoo.obj"]) {
            CompilerArguments::Ok(ParsedArguments { input, extension, depfile: _depfile, outputs, preprocessor_args, common_args }) => {
                assert!(true, "Parsed ok");
                assert_eq!("foo.c", input);
                assert_eq!("c", extension);
                assert_map_contains!(outputs, ("obj", "foo.obj"), ("pdb", "foo.pdb"));
                //TODO: fix assert_map_contains to assert no extra keys!
                assert_eq!(2, outputs.len());
                assert!(preprocessor_args.is_empty());
                assert_eq!(common_args, &["-Zi", "-Fdfoo.pdb"]);
            }
            o @ _ => assert!(false, format!("Got unexpected parse result: {:?}", o)),
        }
    }

    #[test]
    fn test_parse_arguments_empty_args() {
        assert_eq!(CompilerArguments::NotCompilation,
                   parse_arguments(&vec!()));
    }

    #[test]
    fn test_parse_arguments_not_compile() {
        assert_eq!(CompilerArguments::NotCompilation,
                   parse_arguments(&stringvec!["-Fofoo", "foo.c"]));
    }

    #[test]
    fn test_parse_arguments_too_many_inputs() {
        assert_eq!(CompilerArguments::CannotCache,
                   parse_arguments(&stringvec!["-c", "foo.c", "-Fofoo.obj", "bar.c"]));
    }

    #[test]
    fn test_parse_arguments_unsupported() {
        assert_eq!(CompilerArguments::CannotCache,
                   parse_arguments(&stringvec!["-c", "foo.c", "-Fofoo.obj", "-FA"]));

        assert_eq!(CompilerArguments::CannotCache,
                   parse_arguments(&stringvec!["-Fa", "-c", "foo.c", "-Fofoo.obj"]));

        assert_eq!(CompilerArguments::CannotCache,
                   parse_arguments(&stringvec!["-c", "foo.c", "-FR", "-Fofoo.obj"]));
    }

    #[test]
    fn test_parse_arguments_response_file() {
        assert_eq!(CompilerArguments::CannotCache,
                   parse_arguments(&stringvec!["-c", "foo.c", "@foo", "-Fofoo.obj"]));
    }

    #[test]
    fn test_parse_arguments_missing_pdb() {
        assert_eq!(CompilerArguments::CannotCache,
                   parse_arguments(&stringvec!["-c", "foo.c", "-Zi", "-Fofoo.obj"]));
    }

    #[test]
    fn test_compile_simple() {
        let creator = new_creator();
        let pool = CpuPool::new(1);
        let f = TestFixture::new();
        let parsed_args = ParsedArguments {
            input: "foo.c".to_owned(),
            extension: "c".to_owned(),
            depfile: None,
            outputs: vec![("obj", "foo.obj".to_owned())].into_iter().collect::<HashMap<&'static str, String>>(),
            preprocessor_args: vec!(),
            common_args: vec!(),
        };
        let compiler = Compiler::new(f.bins[0].to_str().unwrap(),
                                     CompilerKind::Msvc { includes_prefix: String::new() }).unwrap();
        // Compiler invocation.
        next_command(&creator, Ok(MockChild::new(exit_status(0), "", "")));
        next_command(&creator, Ok(MockChild::new(exit_status(1), "", "")));
        let (cacheable, _) = compile(&creator,
                                     &compiler,
                                     vec!(),
                                     &parsed_args,
                                     f.tempdir.path().to_str().unwrap(),
                                     &pool).wait().unwrap();
        assert_eq!(Cacheable::Yes, cacheable);
        // Ensure that we ran all processes.
        assert_eq!(0, creator.lock().unwrap().children.len());
    }

    #[test]
    fn test_compile_not_cacheable_pdb() {
        let creator = new_creator();
        let pool = CpuPool::new(1);
        let f = TestFixture::new();
        let pdb = f.touch("foo.pdb").unwrap();
        let parsed_args = ParsedArguments {
            input: "foo.c".to_owned(),
            extension: "c".to_owned(),
            depfile: None,
            outputs: vec![("obj", "foo.obj".to_owned()),
                          ("pdb", pdb.to_str().unwrap().to_owned())].into_iter().collect::<HashMap<&'static str, String>>(),
            preprocessor_args: vec!(),
            common_args: vec!(),
        };
        let compiler = Compiler::new(f.bins[0].to_str().unwrap(),
                                     CompilerKind::Msvc { includes_prefix: String::new() }).unwrap();
        // Compiler invocation.
        next_command(&creator, Ok(MockChild::new(exit_status(0), "", "")));
        next_command(&creator, Ok(MockChild::new(exit_status(1), "", "")));
        let (cacheable, _) = compile(&creator,
                                     &compiler,
                                     vec!(),
                                     &parsed_args,
                                     f.tempdir.path().to_str().unwrap(),
                                     &pool).wait().unwrap();
        assert_eq!(Cacheable::No, cacheable);
        // Ensure that we ran all processes.
        assert_eq!(0, creator.lock().unwrap().children.len());
    }

    #[test]
    fn test_compile_preprocessed_fails() {
        let creator = new_creator();
        let pool = CpuPool::new(1);
        let f = TestFixture::new();
        let parsed_args = ParsedArguments {
            input: "foo.c".to_owned(),
            extension: "c".to_owned(),
            depfile: None,
            outputs: vec![("obj", "foo.obj".to_owned())].into_iter().collect::<HashMap<&'static str, String>>(),
            preprocessor_args: vec!(),
            common_args: vec!(),
        };
        let compiler = Compiler::new(f.bins[0].to_str().unwrap(),
                                     CompilerKind::Msvc { includes_prefix: String::new() }).unwrap();
        // First compiler invocation fails.
        next_command(&creator, Ok(MockChild::new(exit_status(1), "", "")));
        // Second compiler invocation succeeds.
        next_command(&creator, Ok(MockChild::new(exit_status(0), "", "")));
        let (cacheable, _) = compile(&creator,
                                     &compiler,
                                     vec!(),
                                     &parsed_args,
                                     f.tempdir.path().to_str().unwrap(),
                                     &pool).wait().unwrap();
        assert_eq!(Cacheable::Yes, cacheable);
        // Ensure that we ran all processes.
        assert_eq!(0, creator.lock().unwrap().children.len());
    }
}
