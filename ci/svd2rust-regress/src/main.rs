#[macro_use]
extern crate error_chain;
extern crate inflections;
extern crate rayon;
extern crate reqwest;
#[macro_use]
extern crate structopt;

mod errors;
mod svd_test;
mod tests;

use error_chain::ChainedError;
use rayon::prelude::*;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::process::{exit, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use structopt::StructOpt;

#[derive(StructOpt, Debug)]
#[structopt(name = "svd2rust-regress")]
struct Opt {
    /// Run a long test (it's very long)
    #[structopt(short = "l", long = "long-test")]
    long_test: bool,

    /// Path to an `svd2rust` binary, relative or absolute.
    /// Defaults to `target/release/svd2rust[.exe]` of this repository
    /// (which must be already built)
    #[structopt(short = "p", long = "svd2rust-path", parse(from_os_str))]
    bin_path: Option<PathBuf>,

    /// Filter by chip name, case sensitive, may be combined with other filters
    #[structopt(short = "c", long = "chip")]
    chip: Option<String>,

    /// Filter by manufacturer, case sensitive, may be combined with other filters
    #[structopt(short = "m", long = "manufacturer")]
    mfgr: Option<String>,

    /// Filter by architecture, case sensitive, may be combined with other filters
    /// Options are: "CortexM", "RiscV", and "Msp430"
    #[structopt(short = "a", long = "architecture")]
    arch: Option<String>,

    /// Include tests expected to fail (will cause a non-zero return code)
    #[structopt(short = "b", long = "bad-tests")]
    bad_tests: bool,

    /// Enable formatting with `rustfmt`
    #[structopt(short = "f", long = "format")]
    format: bool,

    /// Path to an `rustfmt` binary, relative or absolute.
    /// Defaults to `$(rustup which rustfmt)`
    #[structopt(long = "rustfmt_bin_path", parse(from_os_str))]
    rustfmt_bin_path: Option<PathBuf>,

    /// Use verbose output
    #[structopt(long = "verbose", short = "v", parse(from_occurrences))]
    verbose: u8,
    // TODO: Specify smaller subset of tests? Maybe with tags?
    // TODO: Early fail
    // TODO: Compile svd2rust?
}

/// Validate any assumptions made by this program
fn validate_tests(tests: &[&tests::TestCase]) {
    use std::collections::HashSet;

    let mut fail = false;

    // CONDITION 1: All mfgr+chip names must be unique
    let mut uniq = HashSet::new();
    for t in tests {
        let name = t.name();
        if !uniq.insert(name.clone()) {
            fail = true;
            eprintln!("{} is not unique!", name);
        }
    }

    if fail {
        panic!("Tests failed validation");
    }
}

fn main() {
    let opt = Opt::from_args();

    // Validate all test pre-conditions
    validate_tests(tests::TESTS);

    // Determine default svd2rust path
    let default_svd2rust_iter = ["..", "..", "..", "..", "target", "release"];

    let default_svd2rust = if cfg!(windows) {
        default_svd2rust_iter.iter().chain(["svd2rust.exe"].iter())
    } else {
        default_svd2rust_iter.iter().chain(["svd2rust"].iter())
    }.collect();

    let bin_path = match opt.bin_path {
        Some(ref bp) => bp,
        None => &default_svd2rust,
    };

    let default_rustfmt: Option<PathBuf> = if let Some((v, true)) = Command::new("rustup")
        .args(&["which", "rustfmt"])
        .output()
        .ok()
        .map(|o| (o.stdout, o.status.success()))
    {
        Some(String::from_utf8_lossy(&v).into_owned().trim().into())
    } else {
        if opt.format && opt.rustfmt_bin_path.is_none() {
            panic!("rustfmt binary not found, is rustup and rustfmt-preview installed?");
        }
        None
    };

    let rustfmt_bin_path = match (&opt.rustfmt_bin_path, opt.format) {
        (_, false) => None,
        (&Some(ref path), true) => Some(path),
        (&None, true) => {
            // FIXME: Use Option::filter instead when stable, rust-lang/rust#45860
            if default_rustfmt
                .iter()
                .filter(|p| p.is_file())
                .next()
                .is_none()
            {
                panic!("No rustfmt found");
            }
            default_rustfmt.as_ref()
        }
    };
    // collect enabled tests
    let tests = tests::TESTS
        .iter()
        // Short test?
        .filter(|t| t.should_run(!opt.long_test))
        // selected architecture?
        .filter(|t| {
            if let Some(ref arch) = opt.arch {
                arch == &format!("{:?}", t.arch)
            } else {
                true
            }
        })
        // selected manufacturer?
        .filter(|t| {
            if let Some(ref mfgr) = opt.mfgr {
                mfgr == &format!("{:?}", t.mfgr)
            } else {
                true
            }
        })
        // Specify chip - note: may match multiple
        .filter(|t| {
            if let Some(ref chip) = opt.chip {
                chip == t.chip
            } else {
                true
            }
        })
        // Run failable tests?
        .filter(|t| opt.bad_tests || t.should_pass)
        .collect::<Vec<_>>();

    let any_fails = AtomicBool::new(false);

    // TODO: It would be more efficient to reuse directories, so we don't
    // have to rebuild all the deps crates
    tests.par_iter().for_each(|t| {
        let start = Instant::now();

        match svd_test::test(t, &bin_path, rustfmt_bin_path) {
            Ok(()) => {
                // TODO: If verbosity is > 1, print every logged stderr
                eprintln!(
                    "Passed: {} - {} seconds",
                    t.name(),
                    start.elapsed().as_secs()
                );
            }
            Err(e) => {
                any_fails.store(true, Ordering::Release);
                let additional_info = if opt.verbose > 0 {
                    match e.kind() {
                        &errors::ErrorKind::ProcessFailed(ref command, _, Some(ref stderr))
                            if command == "cargo check" =>
                        {
                            let mut buf = String::new();
                            // Unwrap is safe
                            File::open(stderr)
                                .expect("Couldn't open file")
                                .read_to_string(&mut buf)
                                .expect("Couldn't read file to string");
                            buf.insert_str(0, &format!("\n---{:?}---\n", stderr.as_os_str()));
                            buf
                        }
                        _ => "".into(),
                    }
                } else {
                    "".into()
                };
                eprintln!(
                    "Failed: {} - {} seconds. {}{}",
                    t.name(),
                    start.elapsed().as_secs(),
                    e.display_chain().to_string().trim_right(),
                    additional_info,
                );
            }
        }
    });

    if any_fails.load(Ordering::Acquire) {
        exit(1);
    } else {
        exit(0);
    }
}
