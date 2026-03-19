use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use url::Url;

const UBROWSER_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const UBROWSER_LOG_POLL_INTERVAL: Duration = Duration::from_millis(100);
const MAX_LOG_LINES: usize = 40;

pub struct TizenProcess {
    child: Child,
    pub ws_url: String,
    _log_drainers: Vec<std::thread::JoinHandle<()>>,
}

impl TizenProcess {
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for TizenProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Returns true if ubrowser is available on this system.
pub fn is_tizen() -> bool {
    find_ubrowser().is_some()
}

pub fn find_ubrowser() -> Option<PathBuf> {
    // Check well-known paths first
    let candidates = [
        "/usr/apps/org.tizen.chromium-efl/bin/ubrowser",
        "/usr/bin/ubrowser",
        "/usr/local/bin/ubrowser",
    ];
    for c in &candidates {
        let p = PathBuf::from(c);
        if p.exists() {
            return Some(p);
        }
    }

    // Fallback: check PATH
    if let Ok(output) = Command::new("which").arg("ubrowser").output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return Some(PathBuf::from(path));
            }
        }
    }

    None
}

/// Launches `ubrowser` in inspector mode.
pub fn launch_ubrowser(executable: Option<&str>) -> Result<TizenProcess, String> {
    let binary = match executable {
        Some(p) => PathBuf::from(p),
        None => find_ubrowser()
            .ok_or("ubrowser not found. Install ubrowser or use --executable-path.")?,
    };

    debug_log(&format!("launching ubrowser: {} -i -l 0", binary.display()));

    // -i: inspector mode (opens CDP port)
    // -l 0: no window decoration
    let mut cmd = Command::new(&binary);
    cmd.args(["-i", "-l", "0"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to launch ubrowser at {:?}: {}", binary, e))?;

    let (logs, log_rx, log_drainers) = start_log_drainers(&mut child)?;

    match wait_for_ws_url(log_rx, &mut child, &logs) {
        Ok(ws_url) => Ok(TizenProcess {
            child,
            ws_url,
            _log_drainers: log_drainers,
        }),
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(e)
        }
    }
}

#[derive(Clone, Copy)]
enum StreamKind {
    Stdout,
    Stderr,
}

struct StreamLine {
    kind: StreamKind,
    line: String,
}

#[derive(Clone, Default)]
struct LaunchLogBuffer {
    stdout: Arc<Mutex<VecDeque<String>>>,
    stderr: Arc<Mutex<VecDeque<String>>>,
}

impl LaunchLogBuffer {
    fn push(&self, kind: StreamKind, line: String) {
        match kind {
            StreamKind::Stdout => push_bounded(&self.stdout, line),
            StreamKind::Stderr => push_bounded(&self.stderr, line),
        }
    }

    fn snapshot_stdout(&self) -> Vec<String> {
        self.stdout
            .lock()
            .expect("stdout log buffer poisoned")
            .iter()
            .cloned()
            .collect()
    }

    fn snapshot_stderr(&self) -> Vec<String> {
        self.stderr
            .lock()
            .expect("stderr log buffer poisoned")
            .iter()
            .cloned()
            .collect()
    }
}

fn push_bounded(buffer: &Mutex<VecDeque<String>>, line: String) {
    let mut guard = buffer.lock().expect("log buffer poisoned");
    if guard.len() >= MAX_LOG_LINES {
        guard.pop_front();
    }
    guard.push_back(line);
}

fn start_log_drainers(
    child: &mut Child,
) -> Result<
    (
        LaunchLogBuffer,
        Receiver<StreamLine>,
        Vec<std::thread::JoinHandle<()>>,
    ),
    String,
> {
    let stdout = child.stdout.take().ok_or_else(|| {
        let _ = child.kill();
        "Failed to capture ubrowser stdout".to_string()
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        let _ = child.kill();
        "Failed to capture ubrowser stderr".to_string()
    })?;

    let logs = LaunchLogBuffer::default();
    let (tx, rx) = mpsc::channel();

    let stdout_logs = logs.clone();
    let stdout_tx = tx.clone();
    let stdout_handle =
        thread::spawn(move || drain_reader(stdout, StreamKind::Stdout, stdout_logs, stdout_tx));

    let stderr_logs = logs.clone();
    let stderr_handle =
        thread::spawn(move || drain_reader(stderr, StreamKind::Stderr, stderr_logs, tx));

    Ok((logs, rx, vec![stdout_handle, stderr_handle]))
}

fn drain_reader<R>(reader: R, kind: StreamKind, logs: LaunchLogBuffer, tx: Sender<StreamLine>)
where
    R: std::io::Read,
{
    for line in BufReader::new(reader).lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => break,
        };

        logs.push(kind, line.clone());
        match kind {
            StreamKind::Stdout => debug_log(&format!("ubrowser stdout: {}", line)),
            StreamKind::Stderr => debug_log(&format!("ubrowser stderr: {}", line)),
        }

        if tx.send(StreamLine { kind, line }).is_err() {
            break;
        }
    }
}

/// Read stdout/stderr looking for a ws:// URL that ubrowser prints
/// when inspector mode is ready.
fn wait_for_ws_url(
    log_rx: Receiver<StreamLine>,
    child: &mut Child,
    logs: &LaunchLogBuffer,
) -> Result<String, String> {
    let deadline = std::time::Instant::now() + UBROWSER_STARTUP_TIMEOUT;

    loop {
        if let Ok(Some(_)) = child.try_wait() {
            return Err(ubrowser_launch_error(
                "ubrowser exited before providing CDP URL",
                logs,
                None,
            ));
        }

        let now = std::time::Instant::now();
        if now >= deadline {
            return Err(ubrowser_launch_error(
                "Timeout waiting for ubrowser CDP URL",
                logs,
                None,
            ));
        }

        let wait = std::cmp::min(deadline - now, UBROWSER_LOG_POLL_INTERVAL);
        match log_rx.recv_timeout(wait) {
            Ok(StreamLine { kind, line }) => {
                if let Some(raw_ws_url) = extract_ws_url(&line) {
                    let ws_url = normalize_ws_url(&raw_ws_url);
                    let source = match kind {
                        StreamKind::Stdout => "stdout",
                        StreamKind::Stderr => "stderr",
                    };
                    debug_log(&format!(
                        "ubrowser CDP URL from {}: raw={} normalized={}",
                        source, raw_ws_url, ws_url
                    ));
                    return Ok(ws_url);
                }
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                return Err(ubrowser_launch_error(
                    "ubrowser closed stdout/stderr before providing CDP URL",
                    logs,
                    None,
                ));
            }
        }
    }
}

fn debug_log(message: &str) {
    if std::env::var_os("AGENT_BROWSER_DEBUG").is_some() {
        let _ = writeln!(std::io::stderr(), "[tizen] {}", message);
    }
}

/// Extract a ws:// or wss:// URL from a line of output.
fn extract_ws_url(line: &str) -> Option<String> {
    // Handle "DevTools listening on ws://..." format
    if let Some(url) = line.strip_prefix("DevTools listening on ") {
        let url = url.trim();
        if url.starts_with("ws://") || url.starts_with("wss://") {
            return Some(url.to_string());
        }
    }

    // Handle raw ws:// URL on its own line or embedded in text
    for word in line.split_whitespace() {
        if word.starts_with("ws://") || word.starts_with("wss://") {
            return Some(word.to_string());
        }
    }

    None
}

fn normalize_ws_url(ws_url: &str) -> String {
    let Ok(mut parsed) = Url::parse(ws_url) else {
        return ws_url.to_string();
    };

    let host = parsed.host_str();
    if matches!(host, Some("0.0.0.0") | Some("::")) {
        let _ = parsed.set_host(Some("127.0.0.1"));
    }

    parsed.to_string()
}

fn ubrowser_launch_error(
    message: &str,
    logs: &LaunchLogBuffer,
    connect_target: Option<&str>,
) -> String {
    let stdout_lines = logs.snapshot_stdout();
    let stderr_lines = logs.snapshot_stderr();

    if stdout_lines.is_empty() && stderr_lines.is_empty() {
        return format!(
            "{} (no stdout/stderr output from ubrowser)\n\
             Hint: make sure ubrowser is run with -i flag for inspector mode",
            message
        );
    }

    let mut sections = Vec::new();
    if let Some(target) = connect_target {
        sections.push(format!("CDP connect target: {}", target));
    }
    if !stdout_lines.is_empty() {
        let last_lines: Vec<&String> = stdout_lines.iter().rev().take(10).collect();
        sections.push(format!(
            "ubrowser stdout (last {} lines):\n  {}",
            last_lines.len(),
            last_lines
                .into_iter()
                .rev()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("\n  ")
        ));
    }
    if !stderr_lines.is_empty() {
        let last_lines: Vec<&String> = stderr_lines.iter().rev().take(10).collect();
        sections.push(format!(
            "ubrowser stderr (last {} lines):\n  {}",
            last_lines.len(),
            last_lines
                .into_iter()
                .rev()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join("\n  ")
        ));
    }

    format!("{}\n{}", message, sections.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_ws_url_devtools_prefix() {
        let line = "DevTools listening on ws://127.0.0.1:9222/devtools/browser/abc";
        assert_eq!(
            extract_ws_url(line),
            Some("ws://127.0.0.1:9222/devtools/browser/abc".to_string())
        );
    }

    #[test]
    fn extract_ws_url_raw() {
        assert_eq!(
            extract_ws_url("ws://127.0.0.1:9222/"),
            Some("ws://127.0.0.1:9222/".to_string())
        );
    }

    #[test]
    fn extract_ws_url_embedded_in_text() {
        let line = "Inspector started at ws://0.0.0.0:9222/devtools/browser/xyz";
        assert_eq!(
            extract_ws_url(line),
            Some("ws://0.0.0.0:9222/devtools/browser/xyz".to_string())
        );
    }

    #[test]
    fn extract_ws_url_wss() {
        assert_eq!(
            extract_ws_url("wss://localhost:9222/"),
            Some("wss://localhost:9222/".to_string())
        );
    }

    #[test]
    fn extract_ws_url_no_match() {
        assert_eq!(extract_ws_url("Some random log line"), None);
        assert_eq!(extract_ws_url("http://localhost:9222"), None);
        assert_eq!(extract_ws_url(""), None);
    }

    #[test]
    fn normalize_ws_url_rewrites_wildcard_host() {
        assert_eq!(
            normalize_ws_url("ws://0.0.0.0:7777/devtools/browser/abc"),
            "ws://127.0.0.1:7777/devtools/browser/abc"
        );
    }

    #[test]
    fn normalize_ws_url_keeps_concrete_host() {
        assert_eq!(
            normalize_ws_url("ws://127.0.0.1:7777/devtools/browser/abc"),
            "ws://127.0.0.1:7777/devtools/browser/abc"
        );
    }

    #[test]
    fn ubrowser_launch_error_no_output() {
        let msg = ubrowser_launch_error("ubrowser exited", &LaunchLogBuffer::default(), None);
        assert!(msg.contains("no stdout/stderr output"));
        assert!(msg.contains("-i flag"));
    }

    #[test]
    fn ubrowser_launch_error_with_output() {
        let logs = LaunchLogBuffer::default();
        logs.push(StreamKind::Stdout, "loading...".to_string());
        logs.push(StreamKind::Stderr, "ready".to_string());
        let msg = ubrowser_launch_error(
            "Timeout",
            &logs,
            Some("ws://127.0.0.1:7777/devtools/browser/abc"),
        );
        assert!(msg.contains("loading..."));
        assert!(msg.contains("ready"));
        assert!(msg.contains("CDP connect target"));
    }
}
