use cargo_autoprune::{cli, wrapper};
use std::ffi::OsString;
use std::path::PathBuf;

fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match args.first().and_then(|a| a.to_str()) {
        Some("autoprune") => cli::run(&args[1..]), // `cargo autoprune ...`
        Some("status") => cli::run(&args),
        Some("--background") => wrapper::background(
            args.get(1).map(PathBuf::from),
            args.get(3).and_then(|p| p.to_str()?.parse().ok()),
        ),
        None => cli::run(&[]),
        // `cargo-autoprune DIR ...`: cargo always passes a rustc path first, never a directory
        Some(a) if a.starts_with('-') || std::path::Path::new(a).is_dir() => cli::run(&args),
        _ => wrapper::run_wrapper(args),
    }
}
