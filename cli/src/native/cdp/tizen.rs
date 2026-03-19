use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const UBROWSER_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

pub struct TizenProcess {
    child: Child,
    pub ws_url: String,
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

pub fn launch_ubrowser(executable: Option<&str>) -> Result<TizenProcess, String> {
    let binary = match executable {
        Some(p) => PathBuf::from(p),
        None => find_ubrowser().ok_or(
            "ubrowser not found. Install ubrowser or use --executable-path.",
        )?,
    };

    // -i: inspector mode (opens CDP port)
    // -l 0: no window decoration
    let mut child = Command::new(&binary)
        .args(["-i", "-l", "0"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to launch ubrowser at {:?}: {}", binary, e))?;

    let stdout = child.stdout.take().ok_or_else(|| {
        let _ = child.kill();
        "Failed to capture ubrowser stdout".to_string()
    })?;

    match wait_for_ws_url(stdout, &mut child) {
        Ok(ws_url) => Ok(TizenProcess { child, ws_url }),
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(e)
        }
    }
}

/// Read stdout line by line looking for a ws:// URL that ubrowser prints
/// when inspector mode is ready.
fn wait_for_ws_url(
    stdout: std::process::ChildStdout,
    child: &mut Child,
) -> Result<String, String> {
    let reader = BufReader::new(stdout);
    let deadline = std::time::Instant::now() + UBROWSER_STARTUP_TIMEOUT;
    let mut stdout_lines: Vec<String> = Vec::new();

    for line in reader.lines() {
        if std::time::Instant::now() > deadline {
            return Err(ubrowser_launch_error(
                "Timeout waiting for ubrowser CDP URL",
                &stdout_lines,
            ));
        }

        if let Ok(Some(_)) = child.try_wait() {
            return Err(ubrowser_launch_error(
                "ubrowser exited before providing CDP URL",
                &stdout_lines,
            ));
        }

        let line = line.map_err(|e| format!("Failed to read ubrowser stdout: {}", e))?;

        // Look for a ws:// or wss:// URL anywhere in the line
        if let Some(ws_url) = extract_ws_url(&line) {
            return Ok(ws_url);
        }

        stdout_lines.push(line);
    }

    Err(ubrowser_launch_error(
        "ubrowser closed stdout before providing CDP URL",
        &stdout_lines,
    ))
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

fn ubrowser_launch_error(message: &str, stdout_lines: &[String]) -> String {
    if stdout_lines.is_empty() {
        return format!(
            "{} (no stdout output from ubrowser)\n\
             Hint: make sure ubrowser is run with -i flag for inspector mode",
            message
        );
    }

    let last_lines: Vec<&String> = stdout_lines.iter().rev().take(10).collect();
    format!(
        "{}\nubrowser stdout (last {} lines):\n  {}",
        message,
        last_lines.len(),
        last_lines
            .into_iter()
            .rev()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n  ")
    )
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
    fn ubrowser_launch_error_no_output() {
        let msg = ubrowser_launch_error("ubrowser exited", &[]);
        assert!(msg.contains("no stdout output"));
        assert!(msg.contains("-i flag"));
    }

    #[test]
    fn ubrowser_launch_error_with_output() {
        let lines = vec!["loading...".to_string(), "ready".to_string()];
        let msg = ubrowser_launch_error("Timeout", &lines);
        assert!(msg.contains("loading..."));
        assert!(msg.contains("ready"));
        assert!(msg.contains("last 2 lines"));
    }
}
