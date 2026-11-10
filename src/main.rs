use cargo_autoprune::{cli, wrapper};
use std::ffi::OsString;
use std::path::PathBuf;

fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match args.first().and_then(|a| a.to_str()) {
        Some("autoprune") => cli::run(&args[1..]), // `cargo autoprune ...`
        Some("--background") => wrapper::background(args.get(1).map(PathBuf::from)),
        None => cli::run(&[]),
        Some(a) if a.starts_with('-') => cli::run(&args),
        _ => wrapper::run_wrapper(args),
    }
}
