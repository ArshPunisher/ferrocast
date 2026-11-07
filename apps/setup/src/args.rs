//! What setup was asked to do, from its command line.
//!
//! It takes the old installer's switches, because the app's updater starts it with them:
//! `/P` (passive: progress only, no questions), `/S` (silent: no window), `/UPDATE`, `/R`
//! (open the app afterwards) and `/ARGS <app arguments...>`, plus `/D=<folder>` for where to
//! install. `--uninstall` removes the app; `pack` builds a setup `.exe` (on any system).

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Install(Install),
    Uninstall(Uninstall),
    /// `pack <folder> --main <Ferrocast.exe> --version <x.y.z> --out <setup.exe>`
    Pack { dir: PathBuf, main: String, version: String, out: PathBuf },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Install {
    /// No window at all.
    pub silent: bool,
    /// Progress only: starts at once and closes itself.
    pub passive: bool,
    /// Started by the app's updater.
    pub update: bool,
    /// Open the app afterwards, with these arguments.
    pub relaunch: Option<Vec<String>>,
    pub dir: Option<PathBuf>,
    /// `/NS`: no shortcuts (the old installer's switch).
    pub no_shortcuts: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Uninstall {
    pub silent: bool,
    /// The install folder, when this is the copy that runs from the temp folder.
    pub from: Option<PathBuf>,
    /// `--delete-data`: also the scenes, settings and sign-ins (silent uninstalls keep them).
    pub delete_data: bool,
    /// Started by the app's own "Uninstall" (or the copy in the temp folder): don't copy again.
    pub relocated: bool,
}

pub fn parse(args: &[String]) -> Result<Mode, String> {
    let mut it = args.iter().peekable();
    if it.peek().is_some_and(|a| *a == "pack") {
        it.next();
        let dir = it.next().ok_or("pack: which folder?")?.into();
        let (mut main, mut version, mut out) = (None, None, None);
        while let Some(a) = it.next() {
            let v = it.next().ok_or_else(|| format!("pack: {a} needs a value"))?.clone();
            match a.as_str() {
                "--main" => main = Some(v),
                "--version" => version = Some(v),
                "--out" => out = Some(PathBuf::from(v)),
                other => return Err(format!("pack: unknown option {other}")),
            }
        }
        return Ok(Mode::Pack {
            dir,
            main: main.ok_or("pack: --main is needed")?,
            version: version.ok_or("pack: --version is needed")?,
            out: out.ok_or("pack: --out is needed")?,
        });
    }
    if args.iter().any(|a| a == "--uninstall") {
        let mut u = Uninstall::default();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--uninstall" => {}
                "/S" | "/s" => u.silent = true,
                "--delete-data" => u.delete_data = true,
                "--relocated" => u.relocated = true,
                "--from" => u.from = Some(it.next().ok_or("--from needs a folder")?.into()),
                // The old uninstaller's switches, accepted and ignored.
                "/P" | "/UPDATE" => {}
                other => return Err(format!("unknown option {other}")),
            }
        }
        return Ok(Mode::Uninstall(u));
    }
    let mut i = Install::default();
    while let Some(a) = it.next() {
        match a.as_str() {
            "/S" | "/s" => i.silent = true,
            "/P" | "/p" => i.passive = true,
            "/UPDATE" => i.update = true,
            "/NS" => i.no_shortcuts = true,
            "/R" => {
                i.relaunch.get_or_insert_with(Vec::new);
            }
            "/ARGS" => {
                // Everything after belongs to the app.
                i.relaunch = Some(it.by_ref().cloned().collect());
            }
            // As with the old installer, `/D=` comes last and takes the rest, spaces and all.
            d if d.starts_with("/D=") => {
                let rest: Vec<&str> = std::iter::once(&d[3..]).chain(it.by_ref().map(String::as_str)).collect();
                i.dir = Some(PathBuf::from(rest.join(" ")));
            }
            other => return Err(format!("unknown option {other}")),
        }
    }
    // `/ARGS` without `/R` doesn't reopen the app (as the old installer).
    if !args.iter().any(|a| a == "/R") {
        i.relaunch = None;
    }
    Ok(Mode::Install(i))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Mode {
        parse(&s.split_whitespace().map(String::from).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn the_updaters_command_line_is_understood() {
        // What tauri-plugin-updater 2.14 passes for the default (passive) mode.
        let Mode::Install(i) = p("/P /UPDATE /R /ARGS --minimized") else { panic!() };
        assert!(i.passive && i.update && !i.silent);
        assert_eq!(i.relaunch, Some(vec!["--minimized".to_string()]));
        let Mode::Install(i) = p("/S /UPDATE") else { panic!() };
        assert!(i.silent && i.relaunch.is_none());
        let Mode::Install(i) = p("/P /UPDATE /R /ARGS") else { panic!() };
        assert_eq!(i.relaunch, Some(vec![]));
    }

    #[test]
    fn a_double_click_is_a_normal_install() {
        assert_eq!(p(""), Mode::Install(Install::default()));
        let Mode::Install(i) = p("/S /D=C:\\Apps\\Ferrocast") else { panic!() };
        assert_eq!(i.dir, Some(PathBuf::from("C:\\Apps\\Ferrocast")));
        let Mode::Install(i) = p("/S /D=C:\\My Apps\\Ferrocast") else { panic!() };
        assert_eq!(i.dir, Some(PathBuf::from("C:\\My Apps\\Ferrocast")));
    }

    #[test]
    fn uninstall_and_pack_have_their_own_options() {
        let Mode::Uninstall(u) = p("--uninstall /S --delete-data") else { panic!() };
        assert!(u.silent && u.delete_data && u.from.is_none());
        let Mode::Uninstall(u) = p("--uninstall --relocated --from C:\\x") else { panic!() };
        assert!(u.relocated && u.from == Some(PathBuf::from("C:\\x")));
        assert_eq!(
            p("pack stage --main Ferrocast.exe --version 2.4.0 --out out.exe"),
            Mode::Pack { dir: "stage".into(), main: "Ferrocast.exe".into(), version: "2.4.0".into(), out: "out.exe".into() }
        );
        assert!(parse(&["/bogus".into()]).is_err());
        assert!(parse(&["pack".into(), "dir".into()]).is_err());
    }
}
