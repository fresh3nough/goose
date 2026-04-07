use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use indoc::indoc;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Content, ErrorCode, ErrorData, Implementation, InitializeResult,
        ServerCapabilities, ServerInfo,
    },
    schemars::JsonSchema,
    tool, tool_handler, tool_router, ServerHandler,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

struct SshHandler;

impl russh::client::Handler for SshHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        // Accept all host keys for now.
        // Known-hosts verification can be layered in later.
        Ok(true)
    }
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

async fn connect_and_auth(
    host: &str,
    port: u16,
    username: &str,
    password: Option<&str>,
    key_path: Option<&str>,
) -> Result<russh::client::Handle<SshHandler>, ErrorData> {
    if password.is_none() && key_path.is_none() {
        return Err(ErrorData::new(
            ErrorCode::INVALID_PARAMS,
            "Provide password or key_path".to_string(),
            None,
        ));
    }

    let config = russh::client::Config {
        inactivity_timeout: Some(Duration::from_secs(60)),
        ..Default::default()
    };

    let mut session = russh::client::connect(Arc::new(config), (host, port), SshHandler)
        .await
        .map_err(|e| {
            ErrorData::new(
                ErrorCode::INTERNAL_ERROR,
                format!("SSH connection failed: {e}"),
                None,
            )
        })?;

    let authenticated = if let Some(kp) = key_path {
        let expanded = expand_tilde(kp);
        let key = russh::keys::load_secret_key(&expanded, password).map_err(|e| {
            ErrorData::new(
                ErrorCode::INTERNAL_ERROR,
                format!("Failed to load key: {e}"),
                None,
            )
        })?;
        let hash_alg = session
            .best_supported_rsa_hash()
            .await
            .map_err(|e| ErrorData::new(ErrorCode::INTERNAL_ERROR, format!("{e}"), None))?
            .flatten();
        let auth = session
            .authenticate_publickey(
                username,
                russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), hash_alg),
            )
            .await
            .map_err(|e| {
                ErrorData::new(
                    ErrorCode::INTERNAL_ERROR,
                    format!("Key auth failed: {e}"),
                    None,
                )
            })?;
        auth.success()
    } else {
        // safe: checked at top
        let pw = password.unwrap();
        let auth = session
            .authenticate_password(username, pw)
            .await
            .map_err(|e| {
                ErrorData::new(
                    ErrorCode::INTERNAL_ERROR,
                    format!("Password auth failed: {e}"),
                    None,
                )
            })?;
        auth.success()
    };

    if !authenticated {
        return Err(ErrorData::new(
            ErrorCode::INTERNAL_ERROR,
            "Authentication rejected by server".to_string(),
            None,
        ));
    }

    Ok(session)
}

fn default_port() -> u16 {
    22
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct SshExecuteParams {
    /// SSH server hostname or IP address
    pub host: String,
    /// Username for SSH authentication
    pub username: String,
    /// Command to execute on the remote host
    pub command: String,
    /// Password for authentication, or passphrase for encrypted private keys
    pub password: Option<String>,
    /// Path to private key file (supports ~ expansion)
    pub key_path: Option<String>,
    /// SSH port (default 22)
    #[serde(default = "default_port")]
    pub port: u16,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionAction {
    /// Connect and start an interactive shell session
    Open,
    /// Send a command to an existing session
    Run,
    /// Read accumulated output from a session
    Read,
    /// Close and disconnect a session
    Close,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct SshSessionParams {
    /// Action to perform
    pub action: SessionAction,
    /// Session identifier (returned by open; required for run/read/close)
    pub session_id: Option<String>,
    /// SSH server hostname or IP (required for open)
    pub host: Option<String>,
    /// SSH username (required for open)
    pub username: Option<String>,
    /// Password for auth or key passphrase
    pub password: Option<String>,
    /// Path to private key file
    pub key_path: Option<String>,
    /// SSH port (default 22)
    pub port: Option<u16>,
    /// Command to send to the remote shell (required for run)
    pub command: Option<String>,
}

struct PersistedSession {
    session_handle: russh::client::Handle<SshHandler>,
    write_half: russh::ChannelWriteHalf<russh::client::Msg>,
    output_buffer: Arc<Mutex<String>>,
    reader_handle: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
pub struct SshServer {
    tool_router: ToolRouter<Self>,
    instructions: String,
    sessions: Arc<Mutex<HashMap<String, PersistedSession>>>,
    next_id: Arc<AtomicU64>,
}

impl Default for SshServer {
    fn default() -> Self {
        Self::new()
    }
}

#[tool_router(router = tool_router)]
impl SshServer {
    pub fn new() -> Self {
        let instructions = indoc! {r#"
            SSH extension for remote command execution.

            **ssh_execute** — stateless: connect, run one command, return output, disconnect.
            **ssh_session** — persistent terminal:
              open  → start interactive shell (returns session_id)
              run   → send a command
              read  → retrieve accumulated output
              close → disconnect

            Either password or key_path must be provided.
            Both together means key auth with password as passphrase.
        "#}
        .to_string();

        Self {
            tool_router: Self::tool_router(),
            instructions,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    #[tool(
        name = "ssh_execute",
        description = "Run a single SSH command over a fresh connection. Connects, runs the command, returns output, disconnects. No session state persists between calls."
    )]
    pub async fn ssh_execute(
        &self,
        params: Parameters<SshExecuteParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let p = params.0;

        let session = connect_and_auth(
            &p.host,
            p.port,
            &p.username,
            p.password.as_deref(),
            p.key_path.as_deref(),
        )
        .await?;

        let mut channel = session.channel_open_session().await.map_err(|e| {
            ErrorData::new(
                ErrorCode::INTERNAL_ERROR,
                format!("Channel open failed: {e}"),
                None,
            )
        })?;

        channel.exec(true, p.command).await.map_err(|e| {
            ErrorData::new(ErrorCode::INTERNAL_ERROR, format!("Exec failed: {e}"), None)
        })?;

        let mut stdout = String::new();
        let mut stderr = String::new();
        let mut exit_code: Option<u32> = None;

        while let Some(msg) = channel.wait().await {
            match msg {
                russh::ChannelMsg::Data { data } => {
                    stdout.push_str(&String::from_utf8_lossy(&data));
                }
                russh::ChannelMsg::ExtendedData { data, .. } => {
                    stderr.push_str(&String::from_utf8_lossy(&data));
                }
                russh::ChannelMsg::ExitStatus { exit_status } => {
                    exit_code = Some(exit_status);
                }
                _ => {}
            }
        }

        let _ = session
            .disconnect(russh::Disconnect::ByApplication, "", "en")
            .await;

        let output = [stdout.trim(), stderr.trim()]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join("\n");

        if let Some(code) = exit_code {
            if code != 0 {
                let msg = if output.is_empty() {
                    format!("Command exited with code {code}")
                } else {
                    format!("Command exited with code {code}\n{output}")
                };
                return Err(ErrorData::new(ErrorCode::INTERNAL_ERROR, msg, None));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    #[tool(
        name = "ssh_session",
        description = "Manage a persistent SSH terminal session.\n\nActions:\n- open: connect and start a shell (returns session_id)\n- run: send a command to the session\n- read: retrieve accumulated output\n- close: disconnect and clean up"
    )]
    pub async fn ssh_session(
        &self,
        params: Parameters<SshSessionParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let p = params.0;
        match p.action {
            SessionAction::Open => self.session_open(p).await,
            SessionAction::Run => self.session_run(p).await,
            SessionAction::Read => self.session_read(p).await,
            SessionAction::Close => self.session_close(p).await,
        }
    }

    async fn session_open(&self, p: SshSessionParams) -> Result<CallToolResult, ErrorData> {
        let host = p.host.as_deref().ok_or_else(|| {
            ErrorData::new(
                ErrorCode::INVALID_PARAMS,
                "host required for open".to_string(),
                None,
            )
        })?;
        let username = p.username.as_deref().ok_or_else(|| {
            ErrorData::new(
                ErrorCode::INVALID_PARAMS,
                "username required for open".to_string(),
                None,
            )
        })?;
        let port = p.port.unwrap_or(22);

        let session = connect_and_auth(
            host,
            port,
            username,
            p.password.as_deref(),
            p.key_path.as_deref(),
        )
        .await?;

        let channel = session.channel_open_session().await.map_err(|e| {
            ErrorData::new(
                ErrorCode::INTERNAL_ERROR,
                format!("Channel open failed: {e}"),
                None,
            )
        })?;

        channel
            .request_pty(false, "xterm-256color", 120, 40, 0, 0, &[])
            .await
            .map_err(|e| {
                ErrorData::new(
                    ErrorCode::INTERNAL_ERROR,
                    format!("PTY request failed: {e}"),
                    None,
                )
            })?;

        channel.request_shell(false).await.map_err(|e| {
            ErrorData::new(
                ErrorCode::INTERNAL_ERROR,
                format!("Shell request failed: {e}"),
                None,
            )
        })?;

        let (mut read_half, write_half) = channel.split();
        let output_buffer = Arc::new(Mutex::new(String::new()));
        let buf = output_buffer.clone();

        let reader_handle = tokio::spawn(async move {
            while let Some(msg) = read_half.wait().await {
                match msg {
                    russh::ChannelMsg::Data { data } => {
                        buf.lock().await.push_str(&String::from_utf8_lossy(&data));
                    }
                    russh::ChannelMsg::ExtendedData { data, .. } => {
                        buf.lock().await.push_str(&String::from_utf8_lossy(&data));
                    }
                    russh::ChannelMsg::ExitStatus { exit_status } => {
                        buf.lock()
                            .await
                            .push_str(&format!("\n[exited with code {exit_status}]\n"));
                    }
                    _ => {}
                }
            }
        });

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let session_id = format!("ssh-{id}");

        self.sessions.lock().await.insert(
            session_id.clone(),
            PersistedSession {
                session_handle: session,
                write_half,
                output_buffer,
                reader_handle,
            },
        );

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Session opened: {session_id}"
        ))]))
    }

    async fn session_run(&self, p: SshSessionParams) -> Result<CallToolResult, ErrorData> {
        let sid = p.session_id.as_deref().ok_or_else(|| {
            ErrorData::new(
                ErrorCode::INVALID_PARAMS,
                "session_id required".to_string(),
                None,
            )
        })?;
        let cmd = p.command.as_deref().ok_or_else(|| {
            ErrorData::new(
                ErrorCode::INVALID_PARAMS,
                "command required".to_string(),
                None,
            )
        })?;

        {
            let sessions = self.sessions.lock().await;
            let s = sessions.get(sid).ok_or_else(|| {
                ErrorData::new(
                    ErrorCode::INVALID_PARAMS,
                    format!("No session: {sid}"),
                    None,
                )
            })?;
            s.write_half
                .data(format!("{cmd}\n").as_bytes())
                .await
                .map_err(|e| {
                    ErrorData::new(ErrorCode::INTERNAL_ERROR, format!("Send failed: {e}"), None)
                })?;
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
        self.drain_output(sid).await
    }

    async fn session_read(&self, p: SshSessionParams) -> Result<CallToolResult, ErrorData> {
        let sid = p.session_id.as_deref().ok_or_else(|| {
            ErrorData::new(
                ErrorCode::INVALID_PARAMS,
                "session_id required".to_string(),
                None,
            )
        })?;

        tokio::time::sleep(Duration::from_millis(200)).await;
        self.drain_output(sid).await
    }

    async fn session_close(&self, p: SshSessionParams) -> Result<CallToolResult, ErrorData> {
        let sid = p.session_id.as_deref().ok_or_else(|| {
            ErrorData::new(
                ErrorCode::INVALID_PARAMS,
                "session_id required".to_string(),
                None,
            )
        })?;

        let s = self.sessions.lock().await.remove(sid).ok_or_else(|| {
            ErrorData::new(
                ErrorCode::INVALID_PARAMS,
                format!("No session: {sid}"),
                None,
            )
        })?;

        let _ = s.write_half.eof().await;
        let _ = s.write_half.close().await;
        let _ = s
            .session_handle
            .disconnect(russh::Disconnect::ByApplication, "", "en")
            .await;
        s.reader_handle.abort();

        Ok(CallToolResult::success(vec![Content::text(format!(
            "Session closed: {sid}"
        ))]))
    }

    async fn drain_output(&self, session_id: &str) -> Result<CallToolResult, ErrorData> {
        let buf = {
            let sessions = self.sessions.lock().await;
            sessions
                .get(session_id)
                .ok_or_else(|| {
                    ErrorData::new(
                        ErrorCode::INVALID_PARAMS,
                        format!("No session: {session_id}"),
                        None,
                    )
                })?
                .output_buffer
                .clone()
        };

        let output = std::mem::take(&mut *buf.lock().await);

        if output.is_empty() {
            Ok(CallToolResult::success(vec![Content::text(
                "(no new output)",
            )]))
        } else {
            Ok(CallToolResult::success(vec![Content::text(output)]))
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for SshServer {
    fn get_info(&self) -> ServerInfo {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("goose-ssh", env!("CARGO_PKG_VERSION")))
            .with_instructions(self.instructions.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::handler::server::wrapper::Parameters;

    #[tokio::test]
    async fn server_creation() {
        let server = SshServer::new();
        assert!(!server.instructions.is_empty());
    }

    #[tokio::test]
    async fn get_info_metadata() {
        let server = SshServer::new();
        let info = server.get_info();
        assert_eq!(info.server_info.name, "goose-ssh");
        assert!(info.instructions.unwrap().contains("ssh_execute"));
    }

    #[tokio::test]
    async fn execute_missing_auth() {
        let server = SshServer::new();
        let params = SshExecuteParams {
            host: "example.com".into(),
            username: "alice".into(),
            command: "whoami".into(),
            password: None,
            key_path: None,
            port: 22,
        };
        let result = server.ssh_execute(Parameters(params)).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code, ErrorCode::INVALID_PARAMS);
        assert!(err.message.contains("password or key_path"));
    }

    #[tokio::test]
    async fn session_open_missing_host() {
        let server = SshServer::new();
        let params = SshSessionParams {
            action: SessionAction::Open,
            session_id: None,
            host: None,
            username: Some("alice".into()),
            password: Some("secret".into()),
            key_path: None,
            port: None,
            command: None,
        };
        let result = server.ssh_session(Parameters(params)).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("host required"));
    }

    #[tokio::test]
    async fn session_open_missing_username() {
        let server = SshServer::new();
        let params = SshSessionParams {
            action: SessionAction::Open,
            session_id: None,
            host: Some("example.com".into()),
            username: None,
            password: Some("secret".into()),
            key_path: None,
            port: None,
            command: None,
        };
        let result = server.ssh_session(Parameters(params)).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("username required"));
    }

    #[tokio::test]
    async fn session_run_missing_session_id() {
        let server = SshServer::new();
        let params = SshSessionParams {
            action: SessionAction::Run,
            session_id: None,
            host: None,
            username: None,
            password: None,
            key_path: None,
            port: None,
            command: Some("ls".into()),
        };
        let result = server.ssh_session(Parameters(params)).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("session_id required"));
    }

    #[tokio::test]
    async fn session_run_not_found() {
        let server = SshServer::new();
        let params = SshSessionParams {
            action: SessionAction::Run,
            session_id: Some("nonexistent".into()),
            host: None,
            username: None,
            password: None,
            key_path: None,
            port: None,
            command: Some("ls".into()),
        };
        let result = server.ssh_session(Parameters(params)).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("No session"));
    }

    #[tokio::test]
    async fn session_read_not_found() {
        let server = SshServer::new();
        let params = SshSessionParams {
            action: SessionAction::Read,
            session_id: Some("nonexistent".into()),
            host: None,
            username: None,
            password: None,
            key_path: None,
            port: None,
            command: None,
        };
        let result = server.ssh_session(Parameters(params)).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("No session"));
    }

    #[tokio::test]
    async fn session_close_not_found() {
        let server = SshServer::new();
        let params = SshSessionParams {
            action: SessionAction::Close,
            session_id: Some("nonexistent".into()),
            host: None,
            username: None,
            password: None,
            key_path: None,
            port: None,
            command: None,
        };
        let result = server.ssh_session(Parameters(params)).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("No session"));
    }

    #[tokio::test]
    async fn session_run_missing_command() {
        let server = SshServer::new();
        let params = SshSessionParams {
            action: SessionAction::Run,
            session_id: Some("ssh-1".into()),
            host: None,
            username: None,
            password: None,
            key_path: None,
            port: None,
            command: None,
        };
        let result = server.ssh_session(Parameters(params)).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("command required"));
    }

    #[test]
    fn expand_tilde_with_home() {
        let path = expand_tilde("~/.ssh/id_ed25519");
        if let Ok(home) = std::env::var("HOME") {
            assert_eq!(path, PathBuf::from(home).join(".ssh/id_ed25519"));
        }
    }

    #[test]
    fn expand_tilde_absolute() {
        let path = expand_tilde("/etc/ssh/key");
        assert_eq!(path, PathBuf::from("/etc/ssh/key"));
    }
}
