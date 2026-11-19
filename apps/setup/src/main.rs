//! Ferrocast's Windows setup: one small program that installs the app (carrying it packed
//! inside), updates it when the app's updater starts it, and uninstalls it (a copy of itself
//! left in the install folder as `uninstall.exe`). Its window is a web page drawn by WebView2,
//! which Windows 10 and 11 already have and the app itself needs.
//!
//! `pack` (on any system) builds a setup `.exe` from a folder of the app's files.
#![cfg_attr(windows, windows_subsystem = "windows")]
// Most of setup only runs on Windows; elsewhere it can only pack.
#![cfg_attr(not(windows), allow(dead_code))]

mod args;
mod flow;
mod payload;
#[cfg(windows)]
mod win;

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args::parse(&args) {
        Ok(args::Mode::Pack { dir, main, version, out }) => match pack(&dir, &main, &version, &out) {
            Ok(m) => {
                println!("{}: {} files, {} unpacked", out.display(), m.files.len(), flow::mb(m.total));
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("pack failed: {e}");
                ExitCode::FAILURE
            }
        },
        Ok(mode) => run(mode),
        Err(e) => {
            eprintln!("{e}");
            #[cfg(windows)]
            win::complain(&format!("Setup didn't understand how it was started: {e}"));
            ExitCode::from(2)
        }
    }
}

/// Writes this program, carrying the files under `dir`, to `out`.
fn pack(dir: &std::path::Path, main: &str, version: &str, out: &std::path::Path) -> std::io::Result<payload::Manifest> {
    let me = std::fs::read(std::env::current_exe()?)?;
    // A setup that already carries an app packs a new one in its place.
    let (stub, _) = payload::split(&me);
    let mut packed = Vec::new();
    let manifest = payload::pack(dir, version, main, &mut packed)?;
    let exe = payload::attach(stub, &packed);
    let tmp = out.with_extension("partial");
    std::fs::write(&tmp, exe)?;
    std::fs::rename(&tmp, out)?;
    Ok(manifest)
}

#[cfg(windows)]
fn run(mode: args::Mode) -> ExitCode {
    win::run(mode)
}

#[cfg(not(windows))]
fn run(_mode: args::Mode) -> ExitCode {
    eprintln!("Ferrocast's setup installs on Windows; here it can only `pack`.");
    ExitCode::from(2)
}
