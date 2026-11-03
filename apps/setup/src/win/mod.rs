//! Installing, updating and uninstalling on Windows, with or without the window.

mod reg;
mod sys;
mod window;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::args::{Install, Mode, Uninstall};
use crate::flow::{self, DataPlaces, Problem};
use crate::payload::{self, Manifest};

pub const APP: &str = "Ferrocast";
pub const PUBLISHER: &str = "ClearCraft Labs";
/// The app's id (tauri.conf.json `identifier`): its web view data folders and taskbar id.
pub const APP_ID: &str = "com.clearcraftlabs.ferrocast";
/// The service name the app's sign-ins are saved under in Credential Manager.
const SECRETS_SERVICE: &str = "Ferrocast";

/// A line in `%TEMP%\ferrocast-setup.log`, for when something needs looking into.
pub fn log(line: &str) {
    let path = std::env::temp_dir().join("ferrocast-setup.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "[{:?}] {line}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs());
    }
}

pub fn complain(text: &str) {
    log(text);
    sys::message("Ferrocast Setup", text);
}

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_absolute())
}

/// Where a fresh install goes: the user's own programs folder, as before (no admin needed).
fn default_dir() -> PathBuf {
    env_dir("LOCALAPPDATA").unwrap_or_else(std::env::temp_dir).join(APP)
}

fn data_places() -> DataPlaces {
    let roaming = env_dir("APPDATA").unwrap_or_else(std::env::temp_dir);
    let local = env_dir("LOCALAPPDATA").unwrap_or_else(std::env::temp_dir);
    DataPlaces { data: roaming.join(APP), webview: vec![roaming.join(APP_ID), local.join(APP_ID)] }
}

/// A folder the user picked: Ferrocast goes in a `Ferrocast` folder inside it, so picking
/// `D:\Apps` never mixes the app's files with others.
pub fn install_dir_for(picked: &Path) -> PathBuf {
    if picked.file_name().is_some_and(|n| n.eq_ignore_ascii_case(APP)) {
        picked.to_path_buf()
    } else {
        picked.join(APP)
    }
}

pub fn run(mode: Mode) -> ExitCode {
    match mode {
        Mode::Install(opts) => install_mode(opts),
        Mode::Uninstall(opts) => uninstall_mode(opts),
        Mode::Pack { .. } => ExitCode::FAILURE,
    }
}

fn read_self() -> std::io::Result<Vec<u8>> {
    std::fs::read(std::env::current_exe()?)
}

/// What an install works from: this program's bytes, split into the setup and the app.
pub struct Package {
    exe: Vec<u8>,
    stub_len: usize,
    payload_at: usize,
    pub manifest: Manifest,
}

impl Package {
    fn load() -> Result<Option<Self>, String> {
        let exe = read_self().map_err(|e| format!("Setup couldn't read itself: {e}"))?;
        let (stub, carried) = payload::split(&exe);
        let Some(carried) = carried else { return Ok(None) };
        let (manifest, packed) = payload::read_head(carried).map_err(|e| e.to_string())?;
        let stub_len = stub.len();
        let payload_at = exe.len() - packed.len() - 16;
        Ok(Some(Self { stub_len, payload_at, manifest, exe }))
    }

    fn stub(&self) -> &[u8] {
        &self.exe[..self.stub_len]
    }

    fn packed(&self) -> &[u8] {
        &self.exe[self.payload_at..self.exe.len() - 16]
    }
}

fn install_mode(opts: Install) -> ExitCode {
    let pkg = match Package::load() {
        // This copy carries no app: it's an uninstaller started without its switch.
        Ok(None) => return uninstall_mode(Uninstall { silent: opts.silent, ..Uninstall::default() }),
        Ok(Some(p)) => p,
        Err(e) => {
            if !opts.silent {
                complain(&format!("{e}\n\nDownload Ferrocast again from ferrocast.clearcraftlabs.com."));
            }
            return ExitCode::FAILURE;
        }
    };
    let installed = reg::installed();
    let dir = opts.dir.clone().or_else(|| installed.as_ref().map(|(d, _)| d.clone())).unwrap_or_else(default_dir);
    let old_version = installed.and_then(|(_, v)| v);
    log(&format!("install {} into {} (update {}, silent {}, passive {})", pkg.manifest.version, dir.display(), opts.update, opts.silent, opts.passive));

    if opts.silent {
        let job = InstallJob { dir, desktop: false, opts: opts.clone() };
        return match job.run(&pkg, &AtomicBool::new(false), &mut |_| {}) {
            Ok(_) => {
                relaunch(&job, &pkg);
                ExitCode::SUCCESS
            }
            Err(e) => {
                log(&format!("install failed: {e}"));
                ExitCode::FAILURE
            }
        };
    }
    if !webview_ready() {
        return ExitCode::FAILURE;
    }
    window::show(window::Start::Install { pkg: Arc::new(pkg), dir, old_version, opts })
}

/// Setup's window is drawn by WebView2. Windows 10 and 11 come with it; if it was removed,
/// say so and open Microsoft's download page.
fn webview_ready() -> bool {
    if wry::webview_version().is_ok() {
        return true;
    }
    complain("Ferrocast needs Microsoft Edge WebView2, which is missing on this PC.\n\nSetup will open Microsoft's download page. Install \"Evergreen Bootstrapper\", then run Ferrocast's setup again.");
    sys::open_url("https://developer.microsoft.com/microsoft-edge/webview2/#download");
    false
}

/// Progress of a job, as the window shows it.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Event {
    /// A step of the job started (index into the job's steps).
    Step { index: usize },
    /// Bytes (or files) done of the total, and what's being written or removed.
    Progress { done: u64, total: u64, file: String },
    Done { seconds: f64, bytes: u64 },
    Error { title: String, detail: String, log: Vec<String> },
}

pub const INSTALL_STEPS: [&str; 5] = ["Checking space", "Closing Ferrocast", "Unpacking Ferrocast", "Adding shortcuts", "Registering uninstall"];
pub const UNINSTALL_STEPS: [&str; 4] = ["Closing Ferrocast", "Removing files", "Removing shortcuts", "Removing extras"];

#[derive(Debug, Clone)]
pub struct InstallJob {
    pub dir: PathBuf,
    pub desktop: bool,
    pub opts: Install,
}

impl InstallJob {
    pub fn run(&self, pkg: &Package, stop: &AtomicBool, on: &mut dyn FnMut(Event)) -> Result<(), Problem> {
        let started = Instant::now();
        let m = &pkg.manifest;
        let dir = &self.dir;

        on(Event::Step { index: 0 });
        // Room for the new files beside the old ones until the swap, plus a little.
        let need = m.total + m.total / 10;
        if let Some(free) = sys::free_space(dir)
            && free < need {
                return Err(Problem::NoSpace { need, free, drive: sys::drive_of(dir) });
            }
        if !flow::ours_or_empty(dir, &m.main) {
            return Err(Problem::NotOurs(dir.clone()));
        }

        on(Event::Step { index: 1 });
        // After an update starts, the app closes itself: give it a moment before ending it.
        let wait = if self.opts.update { Duration::from_secs(15) } else { Duration::from_secs(2) };
        if !sys::close_running(dir, wait) {
            return Err(Problem::Io {
                what: "closing Ferrocast".into(),
                err: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "it is still running"),
            });
        }

        on(Event::Step { index: 2 });
        let mut last = Instant::now() - Duration::from_secs(1);
        let mut last_file = String::new();
        let mut report = |p: payload::Progress<'_>| {
            // About 30 updates a second, and every new file name.
            if last.elapsed() >= Duration::from_millis(33) || p.file != last_file || p.done == p.total {
                last = Instant::now();
                last_file.clear();
                last_file.push_str(p.file);
                on(Event::Progress { done: p.done, total: p.total, file: p.file.replace('/', "\\") });
            }
            !stop.load(Ordering::Relaxed)
        };
        flow::install(pkg.packed(), m, dir, &[("uninstall.exe", pkg.stub())], &mut report)?;
        log("files in place");

        on(Event::Step { index: 3 });
        let target = dir.join(&m.main);
        let start_menu = sys::programs_dir().map(|d| d.join(format!("{APP}.lnk")));
        let desktop = sys::desktop_dir().map(|d| d.join(format!("{APP}.lnk")));
        for (lnk, wanted) in [(start_menu, !self.opts.no_shortcuts), (desktop, self.desktop && !self.opts.no_shortcuts)] {
            let Some(lnk) = lnk else { continue };
            // An update refreshes the shortcuts there are; it adds none.
            let make = if self.opts.update { lnk.exists() } else { wanted };
            if make
                && let Err(e) = sys::shortcut(&lnk, &target, APP_ID) {
                    log(&format!("shortcut {}: {e}", lnk.display()));
                }
        }

        on(Event::Step { index: 4 });
        let size_kb = u32::try_from((m.total + pkg.stub().len() as u64) / 1024).unwrap_or(u32::MAX);
        reg::write(&reg::Entry { dir: dir.clone(), version: m.version.clone(), main: m.main.clone(), size_kb })
            .map_err(Problem::io("adding Ferrocast to Windows' list of apps"))?;

        on(Event::Done { seconds: started.elapsed().as_secs_f64(), bytes: m.total });
        Ok(())
    }
}

/// Opens the app after an update that asked for it (`/R`).
fn relaunch(job: &InstallJob, pkg: &Package) {
    if let Some(args) = &job.opts.relaunch {
        open_app(&job.dir.join(&pkg.manifest.main), args);
    }
}

pub fn open_app(exe: &Path, args: &[String]) {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args);
    if let Some(dir) = exe.parent() {
        cmd.current_dir(dir);
    }
    if let Err(e) = cmd.spawn() {
        log(&format!("opening {}: {e}", exe.display()));
    }
}

/// Where the app to uninstall is: given, else the folder this uninstaller sits in (when it
/// holds the app), else what Windows' list says.
fn uninstall_dir(opts: &Uninstall) -> Option<PathBuf> {
    if let Some(d) = &opts.from {
        return Some(d.clone());
    }
    let here = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf));
    if let Some(here) = here.filter(|h| h.join(flow::MARK).is_file()) {
        return Some(here);
    }
    reg::installed().map(|(d, _)| d)
}

fn main_name(dir: &Path) -> String {
    std::fs::read(dir.join(flow::MARK))
        .ok()
        .and_then(|b| serde_json::from_slice::<Manifest>(&b).ok())
        .map(|m| m.main)
        .or_else(reg::main_binary)
        .unwrap_or_else(|| format!("{APP}.exe"))
}

fn uninstall_mode(opts: Uninstall) -> ExitCode {
    let Some(dir) = uninstall_dir(&opts) else {
        if !opts.silent {
            complain("Ferrocast isn't installed for this Windows user.");
        }
        return ExitCode::FAILURE;
    };
    // A program can't delete the folder it runs from: run a copy from the temp folder.
    if !opts.relocated {
        return match relocate(&dir, &opts) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                if !opts.silent {
                    complain(&format!("Setup couldn't start the uninstall: {e}"));
                }
                ExitCode::FAILURE
            }
        };
    }
    let _ = std::env::set_current_dir(std::env::temp_dir());
    let main = main_name(&dir);
    log(&format!("uninstall {} (silent {})", dir.display(), opts.silent));
    let result = if opts.silent {
        let job = UninstallJob { dir, main, delete_data: opts.delete_data, remove_extras: opts.delete_data };
        let r = job.run(&mut |_| {});
        if let Err(e) = &r {
            log(&format!("uninstall failed: {e}"));
        }
        if r.is_ok() { ExitCode::SUCCESS } else { ExitCode::FAILURE }
    } else if webview_ready() {
        let extras = data_places().extras_size();
        window::show(window::Start::Uninstall { dir, main, extras })
    } else {
        ExitCode::FAILURE
    };
    if let Ok(me) = std::env::current_exe() {
        sys::delete_after_exit(&me);
    }
    result
}

fn relocate(dir: &Path, opts: &Uninstall) -> std::io::Result<()> {
    let me = read_self()?;
    let (stub, _) = payload::split(&me);
    let copy = std::env::temp_dir().join(format!("ferrocast-uninstall-{}.exe", std::process::id()));
    std::fs::write(&copy, stub)?;
    let mut cmd = std::process::Command::new(&copy);
    cmd.arg("--uninstall").arg("--relocated").arg("--from").arg(dir).current_dir(std::env::temp_dir());
    if opts.silent {
        cmd.arg("/S");
    }
    if opts.delete_data {
        cmd.arg("--delete-data");
    }
    cmd.spawn()?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct UninstallJob {
    pub dir: PathBuf,
    pub main: String,
    pub delete_data: bool,
    pub remove_extras: bool,
}

impl UninstallJob {
    pub fn run(&self, on: &mut dyn FnMut(Event)) -> Result<(), Problem> {
        let started = Instant::now();
        on(Event::Step { index: 0 });
        if !sys::close_running(&self.dir, Duration::from_secs(2)) {
            return Err(Problem::Io {
                what: "closing Ferrocast".into(),
                err: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "it is still running"),
            });
        }

        on(Event::Step { index: 1 });
        let mut last = Instant::now() - Duration::from_secs(1);
        flow::remove_app(&self.dir, &self.main, None, &mut |i, n, f| {
            if last.elapsed() >= Duration::from_millis(33) || i == n {
                last = Instant::now();
                let file = f.strip_prefix(&self.dir).unwrap_or(f).display().to_string();
                on(Event::Progress { done: i as u64, total: n as u64, file });
            }
        })?;

        on(Event::Step { index: 2 });
        let target = self.dir.join(&self.main);
        for lnk in [sys::programs_dir(), sys::desktop_dir()].into_iter().flatten().map(|d| d.join(format!("{APP}.lnk"))) {
            // Only shortcuts that open this install.
            if lnk.is_file() && shortcut_points_at(&lnk, &target) {
                let _ = std::fs::remove_file(&lnk);
            }
        }
        reg::remove(&self.dir);

        on(Event::Step { index: 3 });
        let places = data_places();
        if self.delete_data {
            flow::remove_folder(&places.data, "your scenes and settings")?;
            for w in &places.webview {
                flow::remove_folder(w, "the app's page data")?;
            }
            let n = sys::forget_signins(SECRETS_SERVICE);
            log(&format!("removed {n} saved sign-ins"));
        } else if self.remove_extras {
            for d in places.extras() {
                flow::remove_folder(&d, "downloaded extras")?;
            }
        }
        on(Event::Done { seconds: started.elapsed().as_secs_f64(), bytes: 0 });
        Ok(())
    }
}

/// Whether the shortcut opens `target`.
fn shortcut_points_at(lnk: &Path, target: &Path) -> bool {
    let n = |p: &Path| p.to_string_lossy().replace('/', "\\").to_lowercase();
    sys::shortcut_target(lnk).is_some_and(|t| n(&t) == n(target))
}

/// A job's last log lines, for the error screen's details.
pub fn tail_log(n: usize) -> Vec<String> {
    let text = std::fs::read_to_string(std::env::temp_dir().join("ferrocast-setup.log")).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].iter().map(|s| s.to_string()).collect()
}
