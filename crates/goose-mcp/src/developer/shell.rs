use std::{env, ffi::OsString, process::Stdio};

#[cfg(unix)]
#[allow(unused_imports)] // False positive: trait is used for process_group method
use std::os::unix::process::CommandExt;

#[cfg(windows)]
use std::os::windows::process::CommandExt as _;

#[derive(Debug, Clone)]
pub struct ShellConfig {
    pub executable: String,
    pub args: Vec<String>,
    pub envs: Vec<(OsString, OsString)>,
}

impl Default for ShellConfig {
    fn default() -> Self {
        #[cfg(windows)]
        {
            Self::detect_windows_shell()
        }
        #[cfg(not(windows))]
        {
            let shell = env::var("SHELL").unwrap_or_else(|_| "bash".to_string());
            Self {
                executable: shell,
                args: vec!["-c".to_string()], // -c is standard across bash/zsh/fish
                envs: vec![],
            }
        }
    }
}

impl ShellConfig {
    #[cfg(windows)]
    fn detect_windows_shell() -> Self {
        // Check for PowerShell first (more modern)
        if let Ok(ps_path) = which::which("pwsh") {
            // PowerShell 7+ (cross-platform PowerShell)
            Self {
                executable: ps_path.to_string_lossy().to_string(),
                args: vec![
                    "-NoProfile".to_string(),
                    "-NonInteractive".to_string(),
                    "-Command".to_string(),
                ],
                envs: vec![],
            }
        } else if let Ok(ps_path) = which::which("powershell") {
            // Windows PowerShell 5.1
            Self {
                executable: ps_path.to_string_lossy().to_string(),
                args: vec![
                    "-NoProfile".to_string(),
                    "-NonInteractive".to_string(),
                    "-Command".to_string(),
                ],
                envs: vec![],
            }
        } else {
            // Fall back to cmd.exe
            Self {
                executable: "cmd".to_string(),
                args: vec!["/c".to_string()],
                envs: vec![],
            }
        }
    }
}

pub fn expand_path(path_str: &str) -> String {
    if cfg!(windows) {
        // Expand Windows environment variables (%VAR%)
        let with_userprofile = path_str.replace(
            "%USERPROFILE%",
            &env::var("USERPROFILE").unwrap_or_default(),
        );
        // Add more Windows environment variables as needed
        with_userprofile.replace("%APPDATA%", &env::var("APPDATA").unwrap_or_default())
    } else {
        // Unix-style expansion
        shellexpand::tilde(path_str).into_owned()
    }
}

pub fn is_absolute_path(path_str: &str) -> bool {
    if cfg!(windows) {
        // Check for Windows absolute paths (drive letters and UNC)
        path_str.contains(":\\") || path_str.starts_with("\\\\")
    } else {
        // Unix absolute paths start with /
        path_str.starts_with('/')
    }
}

pub fn normalize_line_endings(text: &str) -> String {
    if cfg!(windows) {
        // Ensure CRLF line endings on Windows
        text.replace("\r\n", "\n").replace("\n", "\r\n")
    } else {
        // Ensure LF line endings on Unix
        text.replace("\r\n", "\n")
    }
}

/// Split a shell command string into tokens, respecting double and single quotes.
///
/// Quoted substrings are kept as single tokens with the quotes stripped.
/// This is intentionally simple — it only needs to be good enough for
/// `.gooseignore` validation, not a full shell parser.
pub fn split_shell_args(command: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_single_quote = false;
    let mut in_double_quote = false;

    for c in command.chars() {
        match c {
            '\'' if !in_double_quote => {
                in_single_quote = !in_single_quote;
            }
            '"' if !in_single_quote => {
                in_double_quote = !in_double_quote;
            }
            c if c.is_whitespace() && !in_single_quote && !in_double_quote => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => {
                current.push(c);
            }
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Configure a shell command with process group support for proper child process tracking.
///
/// On Unix systems, creates a new process group so child processes can be killed together.
/// On Windows, the default behavior already supports process tree termination.
pub fn configure_shell_command(
    shell_config: &ShellConfig,
    command: &str,
    working_dir: Option<&std::path::Path>,
) -> tokio::process::Command {
    let mut command_builder = tokio::process::Command::new(&shell_config.executable);

    if let Some(dir) = working_dir {
        command_builder.current_dir(dir);
    }

    command_builder
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .env("GOOSE_TERMINAL", "1")
        .env("AGENT", "goose")
        .env("GIT_EDITOR", "sh -c 'echo \"Interactive Git commands are not supported in this environment.\" >&2; exit 1'")
        .env("GIT_SEQUENCE_EDITOR", "sh -c 'echo \"Interactive Git commands are not supported in this environment.\" >&2; exit 1'")
        .env("VISUAL", "sh -c 'echo \"Interactive editor not available in this environment.\" >&2; exit 1'")
        .env("EDITOR", "sh -c 'echo \"Interactive editor not available in this environment.\" >&2; exit 1'")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .args(&shell_config.args);

    for (key, value) in &shell_config.envs {
        command_builder.env(key, value);
    }

    // On Windows, use raw_arg to pass the command string verbatim to the shell.
    // Rust's arg() auto-escapes double quotes with backslashes (e.g. \" ),
    // which cmd.exe and PowerShell do not understand, causing commands with
    // quoted arguments (e.g. findstr /n "class MainForm" *.cs) to break.
    #[cfg(windows)]
    {
        command_builder.raw_arg(command);
    }
    #[cfg(not(windows))]
    {
        command_builder.arg(command);
    }

    // On Unix systems, create a new process group so we can kill child processes
    #[cfg(unix)]
    {
        command_builder.process_group(0);
    }

    command_builder
}

/// Kill a process and all its child processes using platform-specific approaches.
///
/// On Unix systems, kills the entire process group.
/// On Windows, kills the process tree.
pub async fn kill_process_group(
    child: &mut tokio::process::Child,
    pid: Option<u32>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    #[cfg(unix)]
    {
        if let Some(pid) = pid {
            // Try SIGTERM first
            let _sigterm_result = unsafe { libc::kill(-(pid as i32), libc::SIGTERM) };

            // Wait a brief moment for graceful shutdown
            tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

            // Force kill with SIGKILL
            let _sigkill_result = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        }

        // Last fallback, return the result of tokio's kill
        child.kill().await.map_err(|e| e.into())
    }

    #[cfg(windows)]
    {
        if let Some(pid) = pid {
            // Use taskkill to kill the process tree on Windows
            let _kill_result = tokio::process::Command::new("taskkill")
                .args(&["/F", "/T", "/PID", &pid.to_string()])
                .output()
                .await;
        }

        // Return the result of tokio's kill
        child.kill().await.map_err(|e| e.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_shell_args_simple() {
        assert_eq!(split_shell_args("ls -la /tmp"), vec!["ls", "-la", "/tmp"]);
    }

    #[test]
    fn split_shell_args_double_quoted() {
        assert_eq!(
            split_shell_args(r#"findstr /n "class MainForm" *.cs"#),
            vec!["findstr", "/n", "class MainForm", "*.cs"]
        );
    }

    #[test]
    fn split_shell_args_single_quoted() {
        assert_eq!(
            split_shell_args("grep 'hello world' file.txt"),
            vec!["grep", "hello world", "file.txt"]
        );
    }

    #[test]
    fn split_shell_args_mixed_quotes() {
        assert_eq!(
            split_shell_args(r#"echo "it's" 'a "test"'"#),
            vec!["echo", "it's", r#"a "test""#]
        );
    }

    #[test]
    fn split_shell_args_empty() {
        assert!(split_shell_args("").is_empty());
        assert!(split_shell_args("   ").is_empty());
    }

    #[test]
    fn split_shell_args_extra_whitespace() {
        assert_eq!(
            split_shell_args("  cmd   arg1   arg2  "),
            vec!["cmd", "arg1", "arg2"]
        );
    }
}
