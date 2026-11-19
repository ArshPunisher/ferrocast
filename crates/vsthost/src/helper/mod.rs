//! The helper process, `ferrocast-vst`: it loads one VST3 and runs it for Ferrocast, which talks
//! to it over stdin and stdout (`proto`). A crash or a hang here ends this process only.
//!
//! `ferrocast-vst scan <bundle>`: the bundle's sound effects, as JSON lines.
//! `ferrocast-vst run <bundle> <class id> <rate> <max block>`: one effect, running.

pub mod com;
pub mod instance;
pub mod module;
#[cfg(target_os = "macos")]
pub mod window;

use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::atomic::Ordering;

use crate::proto::{self, read_frame, write_frame};

/// The helper's main: what it was asked, done; the process's exit code.
pub fn main(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("scan") => match args.get(1) {
            Some(bundle) => scan(Path::new(bundle)),
            None => usage(),
        },
        Some("run") => {
            let (Some(bundle), Some(cid), Some(rate), Some(block)) =
                (args.get(1), args.get(2), args.get(3), args.get(4))
            else {
                return usage();
            };
            let (Ok(rate), Ok(block)) = (rate.parse::<u32>(), block.parse::<usize>()) else { return usage() };
            run(Path::new(bundle), cid, rate.clamp(8_000, 192_000), block.clamp(16, 8192))
        }
        // For the tests of its sandbox: can it read, write or reach this? Exit 0 yes, 1 no;
        // nothing it reads is printed.
        Some("probe") => match (args.get(1).map(String::as_str), args.get(2)) {
            (Some("read"), Some(p)) => i32::from(std::fs::read(p).is_err()),
            (Some("write"), Some(p)) => i32::from(std::fs::write(p, b"x").is_err()),
            (Some("net"), Some(addr)) => {
                let a = addr.parse::<std::net::SocketAddr>();
                i32::from(a.map_or(true, |a| {
                    std::net::TcpStream::connect_timeout(&a, std::time::Duration::from_secs(2)).is_err()
                }))
            }
            _ => usage(),
        },
        _ => usage(),
    }
}

fn usage() -> i32 {
    eprintln!("ferrocast-vst scan <bundle> | run <bundle> <class id> <rate> <max block>");
    2
}

fn scan(bundle: &Path) -> i32 {
    let module = match module::Module::load(bundle) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let mut out = std::io::stdout().lock();
    for f in module.effects(bundle) {
        if let Ok(line) = serde_json::to_string(&f) {
            let _ = writeln!(out, "{line}");
        }
    }
    let _ = out.flush();
    0
}

fn run(bundle: &Path, cid: &str, rate: u32, block: usize) -> i32 {
    let fail = |why: &str| {
        let _ = write_frame(&mut std::io::stdout().lock(), proto::FAILED, why.as_bytes());
        1
    };
    let module = match module::Module::load(bundle) {
        Ok(m) => m,
        Err(e) => return fail(&e),
    };
    let name = module
        .effects(bundle)
        .into_iter()
        .find(|f| f.cid.eq_ignore_ascii_case(cid))
        .map_or_else(|| "VST plugin".to_string(), |f| f.name);
    // Made on this thread: the main one, as plugins expect.
    let inst = match instance::Instance::new(module, cid, name, rate, block, None) {
        Ok(i) => i,
        Err(e) => return fail(&e),
    };
    let mut ready = inst.latency.to_le_bytes().to_vec();
    ready.extend_from_slice(inst.name.as_bytes());
    if write_frame(&mut std::io::stdout().lock(), proto::READY, &ready).is_err() {
        return 1;
    }
    #[cfg(target_os = "macos")]
    return run_mac(inst);
    #[cfg(not(target_os = "macos"))]
    serve(&inst, &|_| Err("a plugin's own window isn't on this system yet".into()))
}

/// The plugin, shared by the main thread (its window) and the one serving Ferrocast (blocks,
/// state), as VST3 hosts do: a plugin is made for a UI thread and an audio thread.
#[cfg(target_os = "macos")]
struct Shared(instance::Instance);
// SAFETY: VST3 plugins are made to be called from the UI thread and the audio thread at once;
// the window is only touched on the main thread (`window`), blocks on the serving one.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
unsafe impl Send for Shared {}
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
unsafe impl Sync for Shared {}

#[cfg(target_os = "macos")]
fn run_mac(inst: instance::Instance) -> i32 {
    use dispatch2::DispatchQueue;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    let Some(mtm) = objc2::MainThreadMarker::new() else { return 1 };
    let shared = std::sync::Arc::new(Shared(inst));
    let serving = shared.clone();
    let spawned = std::thread::Builder::new().name("ferrocast-vst-serve".into()).spawn(move || {
        let window = |open: bool| -> Result<(), String> {
            let (tx, rx) = std::sync::mpsc::channel();
            let on_main = serving.clone();
            DispatchQueue::main().exec_async(move || {
                let r = if open {
                    window::open(&on_main.0)
                } else {
                    window::close();
                    Ok(())
                };
                let _ = tx.send(r);
            });
            rx.recv_timeout(std::time::Duration::from_secs(10))
                .unwrap_or_else(|_| Err("its window didn't open in time".into()))
        };
        let code = serve(&serving.0, &window);
        // Ferrocast let go: the window first (on the main thread), then out.
        DispatchQueue::main().exec_sync(window::drop_editor);
        std::process::exit(code);
    });
    if spawned.is_err() {
        return 1;
    }
    let app = NSApplication::sharedApplication(mtm);
    // No Dock icon for it; its window still comes to the front when opened.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.run();
    0
}

/// Serves Ferrocast's requests until it lets go (stdin ends); `window` opens or closes the
/// plugin's window. The exit code.
fn serve(inst: &instance::Instance, window: &dyn Fn(bool) -> Result<(), String>) -> i32 {
    let mut input = std::io::BufReader::new(std::io::stdin());
    let mut output = BufWriter::new(std::io::stdout());
    let mut told = false;
    loop {
        let Ok((op, payload)) = read_frame(&mut input) else { return 0 };
        let sent = match op {
            proto::PROCESS => match proto::block_of(&payload) {
                Some((seq, mut l, mut r)) => {
                    let done = match inst.process(&mut l, &mut r) {
                        Ok(()) => write_frame(&mut output, proto::PROCESS, &proto::block_bytes(seq, &l, &r)),
                        Err(e) => write_frame(&mut output, proto::FAILED, e.as_bytes()),
                    };
                    // Its window changed something: said once, until the state is read.
                    if done.is_ok() && !told && inst.changed.load(Ordering::Acquire) {
                        told = true;
                        write_frame(&mut output, proto::CHANGED, &[])
                    } else {
                        done
                    }
                }
                None => write_frame(&mut output, proto::FAILED, b"not a block"),
            },
            proto::SET_STATE => match proto::state_of(&payload).map(|(c, k)| inst.set_state(&c, &k)) {
                Some(Ok(())) => write_frame(&mut output, proto::OK, &[]),
                Some(Err(e)) => write_frame(&mut output, proto::FAILED, e.as_bytes()),
                None => write_frame(&mut output, proto::FAILED, b"not a state"),
            },
            proto::GET_STATE => {
                inst.changed.store(false, Ordering::Release);
                told = false;
                let (c, k) = inst.state();
                write_frame(&mut output, proto::GET_STATE, &proto::state_bytes(&c, &k))
            }
            proto::OPEN_WINDOW | proto::CLOSE_WINDOW => match window(op == proto::OPEN_WINDOW) {
                Ok(()) => write_frame(&mut output, proto::OK, &[]),
                Err(e) => write_frame(&mut output, proto::FAILED, e.as_bytes()),
            },
            _ => write_frame(&mut output, proto::FAILED, b"an unknown request"),
        };
        if sent.is_err() {
            return 0;
        }
    }
}
