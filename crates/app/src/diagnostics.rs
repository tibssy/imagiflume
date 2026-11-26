//! Sanitized diagnostics and prefilled GitHub bug reports.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use gtk::glib;
use imagiflume_core::runtime::HardwareProfile;

use crate::Ui;

const ISSUE_URL: &str = "https://github.com/tibssy/imagiflume/issues/new";
const TEMPLATE: &str = "bug_report.yml";
/// Keep the query string well below the request-line limit used by GitHub's
/// front end. Diagnostics plus a useful log tail normally fit.
const URL_BUDGET: usize = 6_000;
const MAX_LOG_BYTES: u64 = 64 * 1024;
const BACKEND_LINES: usize = 100;
const HELPER_LINES: usize = 50;
const TRUNCATION_NOTE: &str =
    "[... earlier lines omitted; the full report was copied to the clipboard ...]";

/// Plain-text environment summary with personal paths removed.
pub(crate) fn summary(ui: &Ui) -> String {
    sanitize(&raw_summary(ui))
}

/// Diagnostics plus the ends of the backend and helper logs, for the clipboard.
pub(crate) fn full_report(diagnostics: &str) -> String {
    let mut report = String::new();
    report.push_str("### Diagnostics\n\n");
    report.push_str(diagnostics.trim_end());
    for (title, path, lines) in log_sources() {
        if let Some(tail) = tail(&path, lines) {
            report.push_str("\n\n### ");
            report.push_str(title);
            report.push_str(&format!(" (last {lines} lines)\n\n```text\n"));
            report.push_str(&tail);
            report.push_str("\n```");
        }
    }
    report.push('\n');
    report
}

/// A log tail that fits beside the diagnostics in the issue URL.
///
/// The backend log is preferred; the clipboard report still carries every
/// available log.
pub(crate) fn prefill_logs(diagnostics: &str) -> Option<String> {
    let base = issue_url(diagnostics, None).len();
    let marker = format!("{TRUNCATION_NOTE}\n");
    let budget = URL_BUDGET.saturating_sub(base + "&logs=".len() + encoded_len(&marker));
    if budget < 120 {
        return None;
    }

    for (title, path, lines) in log_sources() {
        let Some(tail) = tail(&path, lines) else {
            continue;
        };
        let header = format!("{title} (tail):\n");
        let available = budget.saturating_sub(encoded_len(&header));
        if available < 120 {
            continue;
        }
        let lines: Vec<&str> = tail.lines().collect();
        let mut start = 0;
        while start < lines.len() && encoded_len(&lines[start..].join("\n")) > available {
            start += 1;
        }
        if start >= lines.len() {
            continue;
        }
        let body = lines[start..].join("\n");
        return Some(if start == 0 {
            format!("{header}{body}")
        } else {
            format!("{header}{TRUNCATION_NOTE}\n{body}")
        });
    }
    None
}

/// GitHub issue URL with the form prefilled by field id.
pub(crate) fn issue_url(diagnostics: &str, logs: Option<&str>) -> String {
    let mut url = format!(
        "{ISSUE_URL}?template={TEMPLATE}&diagnostics={}",
        escape(diagnostics)
    );
    if let Some(logs) = logs {
        let encoded = escape(logs);
        if url.len() + "&logs=".len() + encoded.len() <= URL_BUDGET {
            url.push_str("&logs=");
            url.push_str(&encoded);
        }
    }
    url
}

fn escape(text: &str) -> String {
    glib::uri_escape_string(text, None::<&str>, false).to_string()
}

fn encoded_len(text: &str) -> usize {
    escape(text).len()
}

fn raw_summary(ui: &Ui) -> String {
    use std::fmt::Write as _;

    let config = ui.config.borrow();
    let locale = std::env::var("LC_ALL")
        .or_else(|_| std::env::var("LC_MESSAGES"))
        .or_else(|_| std::env::var("LANG"))
        .unwrap_or_else(|_| "unknown".into());
    let server = config
        .sd_server
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "auto-detected".into());
    let prompt_helper = crate::prompt_ai::find_cli(&config)
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "not found".into());

    let mut text = String::new();
    let _ = writeln!(
        text,
        "{} {}",
        imagiflume_core::APP_NAME,
        env!("CARGO_PKG_VERSION")
    );
    let _ = writeln!(
        text,
        "Channel: {}",
        if crate::backends::in_flatpak() {
            "Flatpak"
        } else {
            "Native"
        }
    );
    let _ = writeln!(text, "Locale: {locale}");
    let _ = writeln!(text, "GPU: {}", HardwareProfile::detect().summary());
    let _ = writeln!(
        text,
        "Backend: stable-diffusion.cpp · {}",
        config.backend.as_deref().unwrap_or("auto")
    );
    let _ = writeln!(
        text,
        "Backend id: {}",
        config.backend_id.as_deref().unwrap_or("system")
    );
    let _ = writeln!(text, "Server: {server}");
    let _ = writeln!(text, "Prompt helper: {prompt_helper}");
    let _ = writeln!(text, "Models: {}", config.models_dir.display());
    let _ = writeln!(text, "Output: {}", config.output_dir.display());
    let _ = writeln!(
        text,
        "History: {}",
        imagiflume_core::paths::history_db().display()
    );
    let _ = writeln!(
        text,
        "Log: {}",
        imagiflume_core::paths::cache_dir()
            .join("sd-server.log")
            .display()
    );
    let _ = writeln!(
        text,
        "Prompt helper log: {}",
        crate::prompt_ai::log_path().display()
    );
    text
}

/// Replace the home directory, hostname, runtime directory and username so a
/// public issue does not expose the local account.
pub(crate) fn sanitize(text: &str) -> String {
    let home = std::env::var("HOME").ok();
    let user = std::env::var("USER")
        .ok()
        .or_else(|| std::env::var("LOGNAME").ok());
    let host = glib::host_name();
    let runtime = std::env::var("XDG_RUNTIME_DIR").ok();
    sanitize_with(
        text,
        home.as_deref(),
        user.as_deref(),
        Some(host.as_str()),
        runtime.as_deref(),
    )
}

fn sanitize_with(
    text: &str,
    home: Option<&str>,
    user: Option<&str>,
    host: Option<&str>,
    runtime: Option<&str>,
) -> String {
    let mut text = text.to_string();
    if let Some(home) = home.filter(|home| !home.is_empty()) {
        text = replace_token(&text, home, "~");
    }
    if let Some(runtime) = runtime.filter(|runtime| !runtime.is_empty()) {
        text = replace_token(&text, runtime, "<runtime-dir>");
    }
    // Redact the full hostname before the username, otherwise a hostname that
    // contains the username would only be partially replaced.
    if let Some(host) = host.filter(|host| host.len() >= 3) {
        text = replace_token(&text, host, "<host>");
    }
    if let Some(user) = user.filter(|user| user.len() >= 3 && *user != "root") {
        text = replace_token(&text, user, "<user>");
    }
    text
}

fn replace_token(text: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find(needle) {
        let (before, after) = rest.split_at(index);
        let after_needle = &after[needle.len()..];
        let boundary_before = before
            .chars()
            .next_back()
            .is_none_or(|ch| !ch.is_alphanumeric() && ch != '_');
        let boundary_after = after_needle
            .chars()
            .next()
            .is_none_or(|ch| !ch.is_alphanumeric() && ch != '_');
        out.push_str(before);
        if boundary_before && boundary_after {
            out.push_str(replacement);
        } else {
            out.push_str(needle);
        }
        rest = after_needle;
    }
    out.push_str(rest);
    out
}

fn log_sources() -> Vec<(&'static str, PathBuf, usize)> {
    let cache = imagiflume_core::paths::cache_dir();
    vec![
        ("Backend log", cache.join("sd-server.log"), BACKEND_LINES),
        (
            "Upscale log",
            cache.join("sd-upscale-server.log"),
            HELPER_LINES,
        ),
        (
            "Prompt helper log",
            crate::prompt_ai::log_path(),
            HELPER_LINES,
        ),
    ]
}

/// Read the end of a log file, remove terminal noise and keep the newest lines.
fn tail(path: &Path, max_lines: usize) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(MAX_LOG_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let cleaned = clean_log(&text);
    let mut lines: Vec<&str> = cleaned
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    // A non-zero start may have cut the first line in half.
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    let from = lines.len().saturating_sub(max_lines);
    let tail = sanitize(&lines[from..].join("\n"));
    (!tail.trim().is_empty()).then_some(tail)
}

fn clean_log(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        // Progress bars rewrite the same line with carriage returns; the last
        // segment is what a terminal would leave visible.
        let line = line.rsplit('\r').next().unwrap_or("");
        out.push_str(&strip_ansi(line));
    }
    out
}

fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            out.push(ch);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                for ch in chars.by_ref() {
                    if ('@'..='~').contains(&ch) {
                        break;
                    }
                }
            }
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_home_and_username() {
        let text = "Server: /home/alice/VSCodeProjects/stable-diffusion.cpp\n\
                    Log: /home/alice/.cache/imagiflume/sd-server.log";
        let out = sanitize_with(
            text,
            Some("/home/alice"),
            Some("alice"),
            Some("workstation"),
            None,
        );
        assert!(out.contains("~/VSCodeProjects"));
        assert!(out.contains("~/.cache"));
        assert!(!out.contains("alice"));
    }

    #[test]
    fn keeps_usernames_inside_words() {
        let out = sanitize_with("aliceblue and alice", None, Some("alice"), None, None);
        assert_eq!(out, "aliceblue and <user>");
    }

    #[test]
    fn redacts_hostname_and_runtime_dir() {
        let out = sanitize_with(
            "host alice-laptop runtime /run/user/1000",
            None,
            None,
            Some("alice-laptop"),
            Some("/run/user/1000"),
        );
        assert_eq!(out, "host <host> runtime <runtime-dir>");
    }

    #[test]
    fn strips_ansi_and_progress_updates() {
        let text = "start\n\x1b[32m[I]\x1b[0m loading\n  |##   | 1/2\r  |####| 2/2\n";
        assert_eq!(clean_log(text), "start\n[I] loading\n  |####| 2/2\n");
    }

    #[test]
    fn keeps_only_the_newest_lines() {
        let path = std::env::temp_dir().join(format!(
            "imagiflume-diag-{}-{}.log",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, "one\ntwo\nthree\nfour\n").unwrap();
        let tail = tail(&path, 2).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(tail, "three\nfour");
    }

    #[test]
    fn prefilled_url_encodes_the_diagnostics() {
        let url = issue_url("GPU: test\nLocale: en", None);
        assert!(url.contains("template=bug_report.yml"));
        assert!(url.contains("diagnostics=GPU%3A%20test%0ALocale%3A%20en"));
    }

    #[test]
    fn prefilled_url_escapes_non_ascii() {
        let url = issue_url("stable-diffusion.cpp · CUDA", None);
        assert!(url.contains("%C2%B7"));
        assert!(!url.contains('·'));
    }

    #[test]
    fn drops_logs_that_would_make_the_url_too_long() {
        let logs = "x".repeat(URL_BUDGET);
        let url = issue_url("GPU: test", Some(&logs));
        assert!(!url.contains("&logs="));
    }
}
