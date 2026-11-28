//! Opt-in crash and error reports (3.8.1), the app's side. The relay's side is
//! `crates/relay/src/crash`; the contract is in docs/API.md § Crash reports.
//!
//! After a crash or an unclean exit the next start builds **one report** from what the
//! support zip already collects (docs/API.md `/diagnostics/*`), saves it under
//! `<data>/crash-report/pending.json` and asks nothing more of the disk: the preview and the
//! upload both use exactly that saved text, so "See what's sent" is what is sent, and a GET
//! can never undo a DELETE by rebuilding it.
//!
//! **What is in it** (one UTF-8 text, gzipped for the upload): a `SENT` header (the fields the
//! relay stores), `SUMMARY`, `report.json`, the text crash notes from the last day, the panic
//! trace from the previous session's log, and the last 512 KB of the redacted logs. Everything
//! goes through the same scrubber as the zip (stream keys, tokens, emails, IPs, usernames in
//! paths), and the whole text is scrubbed once more at the end. **Never a binary minidump**
//! (`.dmp`, `.mdmp`): text scrubbing can't clean one, so those stay in the manual zip only.
//!
//! **Signature** (groups the same problem): the first 32 hex characters of
//! `SHA-256("ferrocast-crash-sig-v1" \n kind \n place \n message)`, where for a panic `place`
//! is the source file name (no directory, no line or column) and `message` the panic message
//! with every digit run turned into `#`, every token that looks like a path into `<path>` and
//! cut at 160 characters, so a line number, an address or a user's folder never splits one
//! bug into many. Without a panic (a kill, a power cut) the message is the last error line of
//! the previous session, normalised the same way, or empty.
//!
//! **Consent** (`crash_reports` setting): `ask` (default) builds the report and waits for a
//! yes (`POST …/send`); `always` also sends it at start, in the background, at most 3 tries
//! (never again after the relay refused it for good); `never` builds nothing, sends nothing
//! and drops a report left from before.

use std::io::Write;
use std::path::PathBuf;

use base64::Engine as _;
use ferrocast_core::diagnostics::{SupportReport, build_support_report, format_summary_txt, redact_text};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{Engine, EngineError, now};

/// How much of the redacted logs goes in.
const LOG_TAIL_BYTES: usize = 512 * 1024;
const SECTION_CAP: usize = 128 * 1024;
const NOTE_CAP: usize = 64 * 1024;
const TRACE_CAP: usize = 32 * 1024;
const TRACE_LINES: usize = 200;
/// The JSON body stays under this (the relay's cap is 1 MB).
const MAX_BODY: usize = 900_000;
/// Tries to send on its own while `always`.
const MAX_AUTO_ATTEMPTS: u32 = 3;
const DAY_MS: i64 = 86_400_000;
const NO_PANIC: &str = "Ferrocast closed unexpectedly (no panic was recorded).";

/// The one report waiting for a yes (or for the next start, when `always`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Pending {
    pub created_ms: i64,
    /// `crash` or `unclean_exit`.
    pub kind: String,
    pub signature: String,
    pub version: String,
    pub os: String,
    pub arch: String,
    pub cause_codes: Vec<String>,
    pub message: String,
    /// Exactly what is sent (gzipped) and what the preview shows.
    pub text: String,
    /// Sends the app made on its own (`always`).
    #[serde(default)]
    pub auto_attempts: u32,
    /// Why the relay refused it for good; no more tries on its own.
    #[serde(default)]
    pub refused: Option<String>,
}

/// How an upload ended.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Sent,
    /// Kept on this PC; `reason` is the code the API answers with; `terminal` when trying
    /// again on its own can't help.
    Failed {
        reason: &'static str,
        terminal: bool,
    },
}

// ── Pure parts ───────────────────────────────────────────────────────────────────────

/// Digits to `#`, path-like words to `<path>`, one space between words, cut at 160 characters.
pub(crate) fn normalise(msg: &str) -> String {
    let mut out = Vec::new();
    for word in msg.split_whitespace() {
        if word.contains('/') || word.contains('\\') {
            out.push("<path>".to_string());
            continue;
        }
        // An address ("0x7ff6a1b2") is different every run: one placeholder.
        if word.strip_prefix("0x").is_some_and(|h| !h.is_empty() && h.bytes().all(|b| b.is_ascii_hexdigit())) {
            out.push("0x#".to_string());
            continue;
        }
        let mut w = String::with_capacity(word.len());
        let mut in_digits = false;
        for c in word.chars() {
            if c.is_ascii_digit() {
                if !in_digits {
                    w.push('#');
                }
                in_digits = true;
            } else {
                in_digits = false;
                w.push(c);
            }
        }
        out.push(w);
    }
    out.join(" ").chars().take(160).collect()
}

/// The part of a path after the last `/` or `\`, without a trailing `:line:col`.
pub(crate) fn file_of(location: &str) -> String {
    let name = location.rsplit(['/', '\\']).next().unwrap_or(location);
    let mut parts: Vec<&str> = name.split(':').collect();
    while parts.len() > 1 && parts.last().is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())) {
        parts.pop();
    }
    parts.join(":").trim_end_matches(':').to_string()
}

/// A panic found in text: where (as written) and what it said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Panic {
    pub location: String,
    pub message: String,
}

/// The last `panicked at …` in `text` (both the `path:1:2:\nmessage` form and the older
/// `'message', path:1:2` form).
pub(crate) fn find_panic(text: &str) -> Option<Panic> {
    let at = text.rfind("panicked at ")? + "panicked at ".len();
    let rest = &text[at..];
    let mut lines = rest.lines();
    // The desktop logs `info=<panic> trace=<backtrace>` as one event, so the message line
    // continues into ` trace=   0: <frame>`; the frames differ by system and build and must
    // never reach the message or the signature.
    let before_trace = |l: &str| l.split(" trace=").next().unwrap_or("").trim().to_string();
    let first = before_trace(lines.next().unwrap_or(""));
    if let Some(quoted) = first.strip_prefix('\'')
        && let Some((msg, loc)) = quoted.rsplit_once("', ")
    {
        return Some(Panic { location: loc.trim().to_string(), message: msg.trim().to_string() });
    }
    let location = first.trim_end_matches(':').trim().to_string();
    let message = before_trace(lines.next().unwrap_or("")).chars().take(300).collect();
    Some(Panic { location, message })
}

/// The stable id of a problem (see the module docs).
pub(crate) fn signature_of(kind: &str, place: &str, message: &str) -> String {
    let mut h = Sha256::new();
    h.update(format!("ferrocast-crash-sig-v1\n{kind}\n{place}\n{message}").as_bytes());
    h.finalize().iter().take(16).map(|b| format!("{b:02x}")).collect()
}

/// The part of the logs about the session that ended badly: from the second-to-last
/// `Ferrocast starting` to the last one (this session's own start), else everything before
/// this session's start, else all of it.
pub(crate) fn previous_session(logs: &str) -> &str {
    // A start is a whole INFO line (`<time> INFO <target>: Ferrocast starting …`), so the words
    // inside some other line's text (a panic message) don't count as one.
    let mut starts: Vec<usize> = Vec::new();
    let mut at = 0usize;
    for line in logs.split_inclusive('\n') {
        if line.contains(" INFO ") && line.contains("Ferrocast starting") {
            starts.push(at);
        }
        at += line.len();
    }
    match starts.as_slice() {
        [.., prev, last] => &logs[*prev..*last],
        [only] => &logs[..*only],
        [] => logs,
    }
}

/// The last error line of a session, as plain text after the level (for a kill with no panic).
fn last_error_line(text: &str) -> Option<String> {
    text.lines().rev().find_map(|l| {
        let (_, rest) = l.split_once(" ERROR ").or_else(|| l.strip_prefix("ERROR ").map(|r| ("", r)))?;
        Some(rest.trim().to_string())
    })
}

/// The tail of `s`, at most `cap` bytes, starting at a line.
fn tail(s: &str, cap: usize) -> &str {
    if s.len() <= cap {
        return s;
    }
    let mut start = s.len() - cap;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    match s[start..].find('\n') {
        Some(n) => &s[start + n + 1..],
        None => &s[start..],
    }
}

/// The start of `s`, at most `cap` bytes, ending at a character.
fn head(s: &str, cap: usize) -> &str {
    if s.len() <= cap {
        return s;
    }
    let mut end = cap;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// `[a-zA-Z0-9]` plus `extra`, at most `max` characters, anything else `_`.
fn token(s: &str, extra: &[char], max: usize) -> String {
    s.chars().take(max).map(|c| if c.is_ascii_alphanumeric() || extra.contains(&c) { c } else { '_' }).collect()
}

fn one_line(s: &str, max_bytes: usize) -> String {
    head(&s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>(), max_bytes).trim().to_string()
}

/// A binary dump by its name or its first bytes (never sent).
fn is_dump(name: &str, bytes: &[u8]) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".dmp") || lower.ends_with(".mdmp") || bytes.starts_with(b"MDMP") || bytes.contains(&0)
}

/// What the build needs from the diagnostics.
pub(crate) struct Parts<'a> {
    pub report: &'a SupportReport,
    pub summary: &'a str,
    pub report_json: &'a str,
    /// Redacted logs, oldest file first.
    pub logs: &'a [(String, String)],
    /// Redacted crash files with the time each was written.
    pub crash_files: &'a [(String, Vec<u8>, Option<i64>)],
    pub secrets: &'a [&'a str],
    pub users: &'a [&'a str],
    pub now_ms: i64,
}

fn gzip(text: &str) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    // Writing to a Vec can't fail.
    let _ = e.write_all(text.as_bytes());
    e.finish().unwrap_or_default()
}

/// The JSON body for the relay: the stored fields and the gzipped text.
pub(crate) fn upload_body(p: &Pending) -> Value {
    json!({
        "version": p.version, "os": p.os, "arch": p.arch, "kind": p.kind, "signature": p.signature,
        "cause_codes": p.cause_codes, "message": p.message,
        "bundle": base64::engine::general_purpose::STANDARD.encode(gzip(&p.text)),
    })
}

/// Builds the report from the collected diagnostics (pure: no disk, no network).
pub(crate) fn compose(parts: &Parts<'_>) -> Pending {
    let logs_all: String = parts.logs.iter().map(|(_, t)| t.trim_end_matches('\n')).collect::<Vec<_>>().join("\n");
    let session = previous_session(&logs_all);
    let notes: Vec<(String, String)> = parts
        .crash_files
        .iter()
        .filter(|(name, bytes, _)| !is_dump(name, bytes))
        .filter(|(_, _, modified)| modified.is_some_and(|m| m >= parts.now_ms - DAY_MS))
        .take(2)
        .map(|(name, bytes, _)| (name.clone(), String::from_utf8_lossy(bytes).into_owned()))
        .collect();
    let panic = find_panic(session).or_else(|| notes.iter().find_map(|(_, t)| find_panic(t)));
    let kind = if panic.is_some() || !notes.is_empty() { "crash" } else { "unclean_exit" };
    let (place, norm_msg, message) = match &panic {
        Some(p) => (
            file_of(&p.location),
            normalise(&p.message),
            one_line(&format!("panicked at {}: {}", p.location, p.message), 1800),
        ),
        None => {
            (String::new(), last_error_line(session).map(|l| normalise(&l)).unwrap_or_default(), NO_PANIC.to_string())
        }
    };
    let signature = signature_of(kind, &place, &norm_msg);
    // The panic and the lines after it, from the session that ended.
    let trace = session
        .rfind("panicked at ")
        .map(|at| {
            let from = session[..at].rfind('\n').map_or(0, |n| n + 1);
            let mut cut = 0usize;
            for (i, l) in session[from..].split_inclusive('\n').enumerate() {
                if i >= TRACE_LINES {
                    break;
                }
                cut += l.len();
            }
            head(&session[from..from + cut], TRACE_CAP).to_string()
        })
        .unwrap_or_default();
    let cause_codes: Vec<String> = parts
        .report
        .likely_causes
        .iter()
        .map(|c| token(&c.code, &['_', '-', '.', ':'], 64))
        .filter(|c| !c.is_empty())
        .take(8)
        .collect();
    let os_name = format!("{} {}", parts.report.os.name, parts.report.os.version);
    let version = token(&parts.report.app.version, &['.', '-', '_', '+'], 32);
    let arch = token(&parts.report.app.arch, &['-', '_'], 16);
    let os = {
        let s = one_line(&os_name, 64);
        if s.is_empty() { std::env::consts::OS.to_string() } else { s }
    };

    let render = |log_cap: usize| -> String {
        let mut body = String::new();
        let mut section = |title: &str, text: &str| {
            body.push_str(&format!("\n=== {title} ===\n{}\n", text.trim_end_matches('\n')));
        };
        section("SUMMARY", head(parts.summary, SECTION_CAP));
        section("report.json", head(parts.report_json, SECTION_CAP));
        for (name, text) in &notes {
            section(&format!("CRASH NOTE {}", one_line(name, 80)), head(text, NOTE_CAP));
        }
        if !trace.is_empty() {
            section("PANIC AND TRACE (previous session)", &trace);
        }
        section(&format!("LOGS (last {} KB, redacted)", log_cap / 1024), tail(&logs_all, log_cap));
        // The whole text once more through the same scrubber (idempotent on clean text).
        redact_text(&body, parts.secrets, parts.users)
    };
    let header = format!(
        "FERROCAST CRASH REPORT (format 1)\n=== SENT ===\nkind: {kind}\nsignature: {signature}\nversion: {version}\nos: {os}\n\
         arch: {arch}\ncause_codes: {}\nmessage: {message}\n",
        cause_codes.join(", ")
    );
    let mut p = Pending {
        created_ms: parts.now_ms,
        kind: kind.into(),
        signature,
        version,
        os,
        arch,
        cause_codes,
        message,
        text: String::new(),
        auto_attempts: 0,
        refused: None,
    };
    // Smaller logs until the whole body is under the cap (the text compresses well, but a
    // log full of random ids may not).
    let mut cap = LOG_TAIL_BYTES;
    loop {
        p.text = format!("{header}{}", render(cap));
        let size = serde_json::to_vec(&upload_body(&p)).map_or(usize::MAX, |b| b.len());
        if size <= MAX_BODY || cap <= 8 * 1024 {
            break;
        }
        cap /= 2;
    }
    p
}

// ── The engine's part ────────────────────────────────────────────────────────────────

impl Engine {
    fn crash_report_path(&self) -> PathBuf {
        self.inner.data_dir.join("crash-report").join("pending.json")
    }

    fn load_pending(&self) -> Option<Pending> {
        let bytes = std::fs::read(self.crash_report_path()).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn save_pending(&self, p: &Pending) -> Result<(), EngineError> {
        let path = self.crash_report_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| EngineError::Internal(format!("crash report folder: {e}")))?;
        }
        ferrocast_store::durable::write_atomic_raw(
            &path,
            &serde_json::to_vec(p).map_err(|e| EngineError::Internal(e.to_string()))?,
        )
        .map_err(|e| EngineError::Internal(format!("saving the crash report: {e}")))
    }

    /// Removes the saved report (and any `.bak` copy of it: it holds log text). `Ok(false)`
    /// when there was none; an error when it is still there.
    fn drop_pending(&self) -> Result<bool, EngineError> {
        let path = self.crash_report_path();
        let gone = |p: &std::path::Path| match std::fs::remove_file(p) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        };
        let _ = gone(&ferrocast_store::durable::backup_path(&path));
        gone(&path).map_err(|e| EngineError::Unavailable(format!("Couldn't remove the saved crash report: {e}")))
    }

    /// Builds the report from the diagnostics now (the same collection as the support zip).
    pub(crate) async fn build_crash_report(&self) -> Result<Pending, EngineError> {
        let _guard = self.inner.diagnostics_busy.lock().await;
        let (input, secrets, users, logs, crash_files) = self.gather_diagnostics(true).await?;
        let s_refs: Vec<&str> = secrets.iter().map(String::as_str).collect();
        let u_refs: Vec<&str> = users.iter().map(String::as_str).collect();
        let report = build_support_report(input, &s_refs, &u_refs);
        let summary = format_summary_txt(&report, &s_refs, &u_refs);
        let report_json = redact_text(
            &serde_json::to_string_pretty(&report).map_err(|e| EngineError::Internal(e.to_string()))?,
            &s_refs,
            &u_refs,
        );
        let with_times: Vec<(String, Vec<u8>, Option<i64>)> = crash_files
            .into_iter()
            .map(|(name, bytes)| {
                let at = report.crashes.iter().find(|c| c.filename == name).and_then(|c| c.modified_ms);
                (name, bytes, at)
            })
            .collect();
        Ok(compose(&Parts {
            report: &report,
            summary: &summary,
            report_json: &report_json,
            logs: &logs,
            crash_files: &with_times,
            secrets: &s_refs,
            users: &u_refs,
            now_ms: now().0,
        }))
    }

    /// `GET /api/v1/diagnostics/pending-report`.
    pub async fn pending_crash_report(&self) -> Value {
        // No lock: the file is replaced whole, and a send in flight (up to 15 s) must not
        // make the dashboard wait to look.
        match self.load_pending() {
            Some(p) => json!({
                "pending": true, "kind": p.kind, "created_ms": p.created_ms, "preview_text": p.text,
            }),
            None => json!({ "pending": false, "kind": null, "created_ms": null, "preview_text": null }),
        }
    }

    /// `DELETE /api/v1/diagnostics/pending-report`: don't send; the local copy goes too.
    pub async fn discard_pending_crash_report(&self) -> Result<bool, EngineError> {
        let _one = self.inner.crash_report_op.lock().await;
        self.drop_pending()
    }

    /// `POST /api/v1/diagnostics/pending-report/send`: the person said yes.
    ///
    /// `shown`: the `created_ms` of the report the person looked at. A different report
    /// (a newer crash was saved meanwhile) is 409 and nothing is sent, so a yes always means
    /// the text that was shown. `None` sends whatever is waiting.
    pub async fn send_pending_crash_report(&self, shown: Option<i64>) -> Result<Value, EngineError> {
        let _one = self.inner.crash_report_op.lock().await;
        let Some(mut p) = self.load_pending() else {
            return Err(EngineError::Missing {
                code: "no_pending_report",
                message: "No crash report is waiting.",
                hint: "There is nothing to send.",
            });
        };
        if shown.is_some_and(|s| s != p.created_ms) {
            return Err(EngineError::Conflict(
                "A different crash report is waiting now. Look at it again before sending.".into(),
            ));
        }
        Ok(match self.upload_pending(&mut p, false).await {
            Outcome::Sent => json!({ "sent": true }),
            Outcome::Failed { reason, .. } => json!({ "sent": false, "reason": reason }),
        })
    }

    /// Sends the saved report; removes it when it went, keeps it (noting a final refusal)
    /// when it didn't. The caller holds `crash_report_op`.
    async fn upload_pending(&self, p: &mut Pending, auto: bool) -> Outcome {
        let outcome = self.try_upload(p, auto).await;
        match &outcome {
            Outcome::Sent => {
                if let Err(e) = self.drop_pending() {
                    // Sent, but the file stays: mark it so no start sends it a second time.
                    tracing::warn!(error = %e, "the crash report was sent but couldn't be removed");
                    p.refused = Some("already_sent".into());
                    let _ = self.save_pending(p);
                }
            }
            Outcome::Failed { reason, terminal } => {
                if *terminal {
                    p.refused = Some((*reason).to_string());
                    if let Err(e) = self.save_pending(p) {
                        tracing::warn!(error = %e, "couldn't note that the relay refused the crash report");
                    }
                }
            }
        }
        outcome
    }

    async fn try_upload(&self, p: &Pending, auto: bool) -> Outcome {
        let unavailable = Outcome::Failed { reason: "relay_unavailable", terminal: false };
        let Ok(url) = self.relay_url() else { return unavailable };
        let Ok(auth) = self.relay_auth().await else { return unavailable };
        // On its own only while the person still says "always": it may have changed while
        // this waited (the lock, the sign-up with the relay).
        if auto && self.settings().crash_reports != "always" {
            return Outcome::Failed { reason: "consent_changed", terminal: false };
        }
        let body = match serde_json::to_vec(&upload_body(p)) {
            Ok(b) => b,
            Err(_) => return Outcome::Failed { reason: "too_large", terminal: true },
        };
        if body.len() > MAX_BODY + 100_000 {
            return Outcome::Failed { reason: "too_large", terminal: true };
        }
        let res = self
            .relay_http()
            .post(format!("{url}/v1/crash-reports"))
            .bearer_auth(auth)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await;
        let status = match res {
            Ok(r) => r.status().as_u16(),
            Err(e) => {
                // The error text can carry the address; the status is all that's logged.
                tracing::info!(timeout = e.is_timeout(), "the crash report didn't reach the relay");
                return unavailable;
            }
        };
        match status {
            200..=299 => Outcome::Sent,
            // An older relay without the route: keep it here, quietly.
            404 | 405 => Outcome::Failed { reason: "relay_too_old", terminal: false },
            429 => Outcome::Failed { reason: "rate_limited", terminal: false },
            413 => Outcome::Failed { reason: "too_large", terminal: true },
            400 | 401 | 403 | 415 | 422 => Outcome::Failed { reason: "rejected", terminal: true },
            _ => unavailable,
        }
    }

    /// What start does about a crash (see the module docs). Once per run.
    pub(crate) async fn crash_report_startup(&self) {
        if self.inner.crash_report_started.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let mode = self.settings().crash_reports;
        if mode == "never" {
            let _one = self.inner.crash_report_op.lock().await;
            if let Err(e) = self.drop_pending() {
                tracing::warn!(error = %e, "couldn't remove an old crash report");
            }
            return;
        }
        if self.safe_mode_status().unclean_exit {
            match self.build_crash_report().await {
                Ok(p) => {
                    let _one = self.inner.crash_report_op.lock().await;
                    // The setting may have changed while it was built.
                    if self.settings().crash_reports == "never" {
                        return;
                    }
                    if let Err(e) = self.save_pending(&p) {
                        tracing::warn!(error = %e, "couldn't keep the crash report");
                    }
                }
                Err(e) => tracing::warn!(error = %e, "couldn't build the crash report"),
            }
        }
        let _one = self.inner.crash_report_op.lock().await;
        // Looked at again with the lock held: the person may have changed it meanwhile.
        if self.settings().crash_reports != "always" {
            return;
        }
        let Some(mut p) = self.load_pending() else { return };
        if p.refused.is_some() || p.auto_attempts >= MAX_AUTO_ATTEMPTS {
            return;
        }
        p.auto_attempts += 1;
        if let Err(e) = self.save_pending(&p) {
            // Without the count it could retry for ever: don't send.
            tracing::warn!(error = %e, "couldn't note the crash report try");
            return;
        }
        let outcome = self.upload_pending(&mut p, true).await;
        if matches!(outcome, Outcome::Failed { reason: "consent_changed", .. }) {
            // Nothing was sent: that wasn't a try.
            p.auto_attempts = p.auto_attempts.saturating_sub(1);
            let _ = self.save_pending(&p);
        }
        tracing::info!(sent = outcome == Outcome::Sent, "the crash report was sent on its own");
    }

    /// Starts [`Engine::crash_report_startup`] in the background (never blocks start).
    pub(crate) fn spawn_crash_report_startup(&self) {
        let engine = self.clone();
        tokio::spawn(async move { engine.crash_report_startup().await });
    }
}

#[cfg(test)]
impl Engine {
    pub(crate) fn load_pending_for_test(&self) -> Option<Pending> {
        self.load_pending()
    }
    pub(crate) fn save_pending_for_test(&self, p: &Pending) {
        self.save_pending(p).unwrap();
    }
    /// The reason an upload ended with (`"sent"` when it went).
    pub(crate) async fn upload_pending_for_test(&self, p: &mut Pending, auto: bool) -> &'static str {
        match self.upload_pending(p, auto).await {
            Outcome::Sent => "sent",
            Outcome::Failed { reason, .. } => reason,
        }
    }
}
