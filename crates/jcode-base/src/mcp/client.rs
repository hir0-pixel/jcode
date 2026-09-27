//! MCP Client - handles communication with a single MCP server

use super::protocol::*;
use anyhow::{Context, Result};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderName, HeaderValue};
use serde_json::Value;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, mpsc, oneshot};

/// Shared communication handle for an MCP server.
/// Multiple sessions can hold clones of this and send concurrent requests.
/// Request/response correlation by ID ensures no interference.
#[derive(Clone)]
pub struct McpHandle {
    pub(crate) name: String,
    request_id: Arc<AtomicU64>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>>,
    writer_tx: mpsc::Sender<String>,
    server_info: Arc<std::sync::RwLock<Option<ServerInfo>>>,
    capabilities: Arc<std::sync::RwLock<ServerCapabilities>>,
    tools: Arc<std::sync::RwLock<Vec<McpToolDef>>>,
    http_protocol_version: Option<Arc<std::sync::RwLock<String>>>,
    /// Reply timeout applied to every request on this server.
    request_timeout: std::time::Duration,
}

/// Default reply timeout when a server config does not set `timeout_secs`.
pub const DEFAULT_MCP_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Resolve the per-request reply timeout for a server config.
pub fn request_timeout_for(config: &McpServerConfig) -> std::time::Duration {
    config
        .timeout_secs
        .filter(|secs| *secs > 0)
        .map(std::time::Duration::from_secs)
        .unwrap_or(DEFAULT_MCP_REQUEST_TIMEOUT)
}

impl McpHandle {
    /// Send a request and wait for response
    pub async fn request(&self, method: &str, params: Option<Value>) -> Result<JsonRpcResponse> {
        let id = self.request_id.fetch_add(1, Ordering::SeqCst);
        let request = JsonRpcRequest::new(id, method, params);

        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(id, tx);
        }

        let msg = serde_json::to_string(&request)? + "\n";
        if let Err(error) = self.writer_tx.send(msg).await {
            self.pending.lock().await.remove(&id);
            return Err(error).context("Failed to send request");
        }

        let response = match tokio::time::timeout(self.request_timeout, rx).await {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                self.pending.lock().await.remove(&id);
                return Err(error).context("Channel closed");
            }
            Err(_) => {
                self.pending.lock().await.remove(&id);
                anyhow::bail!(
                    "Request timeout after {}s (raise `timeout_secs` for MCP server '{}' if its tools legitimately run longer)",
                    self.request_timeout.as_secs(),
                    self.name
                );
            }
        };

        if let Some(err) = &response.error {
            anyhow::bail!("MCP error {}: {}", err.code, err.message);
        }

        Ok(response)
    }

    /// Call a tool
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<ToolCallResult> {
        let arguments = if arguments.is_null() {
            Value::Object(serde_json::Map::new())
        } else {
            arguments
        };
        let params = ToolCallParams {
            name: name.to_string(),
            arguments,
        };

        let response = self
            .request("tools/call", Some(serde_json::to_value(params)?))
            .await?;

        let result = response.result.context("No result from tool call")?;
        let tool_result: ToolCallResult = serde_json::from_value(result)?;

        Ok(tool_result)
    }

    /// Get the server name
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get server info
    pub fn server_info(&self) -> Option<ServerInfo> {
        self.server_info
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Get available tools
    pub fn tools(&self) -> Vec<McpToolDef> {
        self.tools
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Refresh the list of available tools
    pub async fn refresh_tools(&self) -> Result<()> {
        let response = self.request("tools/list", None).await?;

        if let Some(result) = response.result {
            let tools_result: ToolsListResult = serde_json::from_value(result)?;
            *self
                .tools
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = tools_result.tools;
        }

        Ok(())
    }
}

/// MCP Client - owns the child process and provides shared handles.
/// Only one McpClient exists per MCP server process, but many McpHandle
/// clones can be distributed to different sessions.
pub struct McpClient {
    handle: McpHandle,
    child: Option<Child>,
}

impl McpClient {
    /// Connect to an MCP server, inheriting the current process working directory
    pub async fn connect(name: String, config: &McpServerConfig) -> Result<Self> {
        Self::connect_in_dir(name, config, None).await
    }

    /// Connect to an MCP server, optionally running it in `working_dir`.
    ///
    /// The working directory is only applied when it exists; otherwise the
    /// subprocess falls back to inheriting the current process cwd (issue #557).
    pub async fn connect_in_dir(
        name: String,
        config: &McpServerConfig,
        working_dir: Option<&std::path::Path>,
    ) -> Result<Self> {
        if !config.is_stdio() {
            return Self::connect_http(name, config).await;
        }
        let working_dir = working_dir.filter(|dir| dir.is_dir());
        crate::logging::info(&format!(
            "MCP: Connecting to '{}' ({} {:?}) cwd={:?}",
            name, config.command, config.args, working_dir
        ));

        // Credentials must be opted into an MCP server explicitly through its
        // config. The long-lived jcode daemon contains provider credentials in
        // its process environment, and blindly inheriting them exposes those
        // credentials to every configured MCP executable (issue #771).
        let inherited: HashMap<String, String> = std::env::vars().collect();
        let env = mcp_child_env(inherited, &config.env);

        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .envs(&env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = working_dir {
            command.current_dir(dir);
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("Failed to spawn MCP server: {}", config.command))?;

        let stdin = child.stdin.take().context("No stdin")?;
        let stdout = child.stdout.take().context("No stdout")?;
        let stderr = child.stderr.take().context("No stderr")?;

        // Spawn stderr reader
        let server_name = name.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            crate::logging::warn(&format!(
                                "MCP [{}] stderr: {}",
                                server_name, trimmed
                            ));
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        // Setup channels
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (writer_tx, mut writer_rx) = mpsc::channel::<String>(32);

        // Spawn writer task
        let mut stdin = stdin;
        tokio::spawn(async move {
            while let Some(msg) = writer_rx.recv().await {
                if stdin.write_all(msg.as_bytes()).await.is_err() {
                    break;
                }
                if stdin.flush().await.is_err() {
                    break;
                }
            }
        });

        // Spawn reader task
        let pending_clone = Arc::clone(&pending);
        let reader_name = name.clone();
        let mut reader = BufReader::new(stdout);
        tokio::spawn(async move {
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => {
                        crate::logging::debug(&format!("MCP [{}]: stdout EOF", reader_name));
                        break;
                    }
                    Ok(_) => {
                        if let Ok(response) = serde_json::from_str::<JsonRpcResponse>(&line) {
                            if let Some(id) = response.id {
                                let mut pending = pending_clone.lock().await;
                                if let Some(tx) = pending.remove(&id) {
                                    let _ = tx.send(response);
                                }
                            }
                        } else {
                            let trimmed = line.trim();
                            if !trimmed.is_empty() {
                                crate::logging::debug(&format!(
                                    "MCP [{}] non-JSON output: {}",
                                    reader_name, trimmed
                                ));
                            }
                        }
                    }
                    Err(e) => {
                        crate::logging::warn(&format!("MCP [{}] read error: {}", reader_name, e));
                        break;
                    }
                }
            }
        });

        let handle = McpHandle {
            name: name.clone(),
            request_id: Arc::new(AtomicU64::new(1)),
            pending,
            writer_tx,
            server_info: Arc::new(std::sync::RwLock::new(None)),
            capabilities: Arc::new(std::sync::RwLock::new(ServerCapabilities::default())),
            tools: Arc::new(std::sync::RwLock::new(Vec::new())),
            http_protocol_version: None,
            request_timeout: request_timeout_for(config),
        };

        let mut client = Self {
            handle,
            child: Some(child),
        };

        client
            .initialize()
            .await
            .with_context(|| format!("MCP server '{}' failed to initialize", name))?;

        client
            .handle
            .refresh_tools()
            .await
            .with_context(|| format!("MCP server '{}' failed to list tools", name))?;

        crate::logging::info(&format!(
            "MCP: Connected to '{}' with {} tools",
            name,
            client.handle.tools().len()
        ));

        Ok(client)
    }

    async fn connect_http(name: String, config: &McpServerConfig) -> Result<Self> {
        let url = config
            .url
            .as_deref()
            .context("remote MCP server URL is missing")?;
        let client = reqwest::Client::builder()
            .timeout(request_timeout_for(config))
            .build()?;
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (writer_tx, mut writer_rx) = mpsc::channel::<String>(32);
        let pending_writer = Arc::clone(&pending);
        let name_for_writer = name.clone();
        let url = url.to_string();
        let headers = config.headers.clone();
        let hermes_home = std::env::var_os("HERMES_HOME").map(std::path::PathBuf::from);
        let oauth = hermes_mcp_is_oauth(hermes_home.as_deref(), &name);
        let protocol_version = Arc::new(std::sync::RwLock::new("2024-11-05".to_string()));
        let protocol_version_writer = Arc::clone(&protocol_version);
        tokio::spawn(async move {
            let mut session_id: Option<String> = None;
            while let Some(message) = writer_rx.recv().await {
                let request: Value = match serde_json::from_str(message.trim()) {
                    Ok(request) => request,
                    Err(_) => continue,
                };
                let id = request["id"].as_u64();
                let mut builder = client
                    .post(&url)
                    .header(CONTENT_TYPE, "application/json")
                    .header(ACCEPT, "application/json, text/event-stream");
                if request["method"] != "initialize"
                    && let Ok(version) = protocol_version_writer.read()
                {
                    builder = builder.header("mcp-protocol-version", version.as_str());
                }
                if let Some(session_id) = &session_id {
                    builder = builder.header("mcp-session-id", session_id);
                }
                let mut invalid_header = None;
                for (key, value) in &headers {
                    match (
                        HeaderName::from_bytes(key.as_bytes()),
                        HeaderValue::from_str(value),
                    ) {
                        (Ok(key), Ok(value)) => builder = builder.header(key, value),
                        _ => invalid_header = Some(key.clone()),
                    }
                }
                if let Some(key) = invalid_header {
                    deliver_mcp_error(
                        &pending_writer,
                        id,
                        -32600,
                        &format!("invalid MCP header: {key}"),
                    )
                    .await;
                    continue;
                }
                if oauth {
                    match hermes_mcp_access_token(hermes_home.as_deref(), &name_for_writer) {
                        Ok(token) => builder = builder.bearer_auth(token),
                        Err(error) => {
                            deliver_mcp_error(&pending_writer, id, -32001, &error.to_string())
                                .await;
                            continue;
                        }
                    }
                }
                let response = match builder.body(message.trim().to_string()).send().await {
                    Ok(response) => response,
                    Err(error) => {
                        deliver_mcp_error(&pending_writer, id, -32000, &error.to_string()).await;
                        continue;
                    }
                };
                if let Some(value) = response.headers().get("mcp-session-id")
                    && let Ok(value) = value.to_str()
                {
                    session_id = Some(value.to_string());
                }
                let status = response.status();
                let body = match response.text().await {
                    Ok(body) => body,
                    Err(error) => {
                        deliver_mcp_error(&pending_writer, id, -32000, &error.to_string()).await;
                        continue;
                    }
                };
                if !status.is_success() {
                    deliver_mcp_error(
                        &pending_writer,
                        id,
                        -32000,
                        &format!(
                            "HTTP {status}: {}",
                            body.chars().take(512).collect::<String>()
                        ),
                    )
                    .await;
                    continue;
                }
                if let Some(id) = id {
                    match parse_mcp_http_response(&body) {
                        Ok(response) => {
                            if let Some(tx) = pending_writer.lock().await.remove(&id) {
                                let _ = tx.send(response);
                            }
                        }
                        Err(error) => {
                            deliver_mcp_error(&pending_writer, Some(id), -32700, &error.to_string())
                                .await
                        }
                    }
                }
            }
        });

        let handle = McpHandle {
            name: name.clone(),
            request_id: Arc::new(AtomicU64::new(1)),
            pending,
            writer_tx,
            server_info: Arc::new(std::sync::RwLock::new(None)),
            capabilities: Arc::new(std::sync::RwLock::new(ServerCapabilities::default())),
            tools: Arc::new(std::sync::RwLock::new(Vec::new())),
            http_protocol_version: Some(protocol_version),
            request_timeout: request_timeout_for(config),
        };
        let mut client = Self {
            handle,
            child: None,
        };
        client
            .initialize()
            .await
            .with_context(|| format!("MCP server '{name}' failed to initialize"))?;
        client
            .handle
            .refresh_tools()
            .await
            .with_context(|| format!("MCP server '{name}' failed to list tools"))?;
        crate::logging::info(&format!(
            "MCP: Connected to remote '{}' with {} tools",
            name,
            client.handle.tools().len()
        ));
        Ok(client)
    }

    /// Get a shareable handle to this client
    pub fn handle(&self) -> McpHandle {
        self.handle.clone()
    }

    /// Initialize the MCP connection
    async fn initialize(&mut self) -> Result<()> {
        let params = InitializeParams {
            protocol_version: "2024-11-05".to_string(),
            capabilities: ClientCapabilities::default(),
            client_info: ClientInfo {
                name: "jcode".to_string(),
                version: jcode_build_meta::pkg_version().to_string(),
            },
        };

        let response = self
            .handle
            .request("initialize", Some(serde_json::to_value(params)?))
            .await?;

        if let Some(result) = response.result {
            let init_result: InitializeResult = serde_json::from_value(result)?;
            if let Some(version) = &self.handle.http_protocol_version
                && let Ok(mut version) = version.write()
            {
                *version = init_result.protocol_version.clone();
            }
            *self
                .handle
                .server_info
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = init_result.server_info;
            *self
                .handle
                .capabilities
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = init_result.capabilities;
        }

        // Send initialized notification
        let notif = JsonRpcNotification::new("notifications/initialized", None);
        let msg = serde_json::to_string(&notif)? + "\n";
        self.handle.writer_tx.send(msg).await?;

        Ok(())
    }

    /// Check if server is still running
    pub fn is_running(&mut self) -> bool {
        let Some(child) = &mut self.child else {
            return !self.handle.writer_tx.is_closed();
        };
        match child.try_wait() {
            Ok(None) => true,
            Ok(Some(_)) => false,
            Err(_) => false,
        }
    }

    /// Shutdown the server
    pub async fn shutdown(&mut self) {
        let _ = self
            .handle
            .writer_tx
            .send("{\"jsonrpc\":\"2.0\",\"method\":\"shutdown\"}\n".to_string())
            .await;

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        if let Some(child) = &mut self.child {
            let _ = child.kill().await;
        }
    }

    // === Legacy compatibility methods that delegate to handle ===

    pub fn name(&self) -> &str {
        &self.handle.name
    }

    pub fn server_info(&self) -> Option<ServerInfo> {
        self.handle.server_info()
    }

    pub fn tools(&self) -> Vec<McpToolDef> {
        self.handle.tools()
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<ToolCallResult> {
        self.handle.call_tool(name, arguments).await
    }

    pub async fn refresh_tools(&self) -> Result<()> {
        self.handle.refresh_tools().await
    }
}

/// Secrets that an MCP child must not receive merely because jcode has them.
///
/// This intentionally applies only to inherited values. A server can still be
/// given any of these names through `McpServerConfig::env`.
fn is_sensitive_inherited_env_key(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    key.ends_with("_API_KEY")
        || key.ends_with("_ACCESS_TOKEN")
        || key.ends_with("_AUTH_TOKEN")
        || matches!(
            key.as_str(),
            "AWS_ACCESS_KEY_ID"
                | "AWS_SECRET_ACCESS_KEY"
                | "AWS_SESSION_TOKEN"
                | "AZURE_CLIENT_SECRET"
                | "GOOGLE_APPLICATION_CREDENTIALS"
        )
}

fn mcp_child_env(
    mut inherited: HashMap<String, String>,
    explicit: &HashMap<String, String>,
) -> HashMap<String, String> {
    inherited.retain(|key, _| !is_sensitive_inherited_env_key(key));
    inherited.extend(explicit.clone());
    inherited
}

impl Drop for McpClient {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
        }
    }
}

async fn deliver_mcp_error(
    pending: &Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>>,
    id: Option<u64>,
    code: i64,
    message: &str,
) {
    let Some(id) = id else { return };
    if let Some(tx) = pending.lock().await.remove(&id) {
        if let Ok(response) = serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0", "id": id,
            "error": { "code": code, "message": message }
        })) {
            let _ = tx.send(response);
        }
    }
}

fn parse_mcp_http_response(body: &str) -> Result<JsonRpcResponse> {
    if let Ok(response) = serde_json::from_str(body) {
        return Ok(response);
    }
    for event in body.split("\n\n") {
        let data = event
            .lines()
            .filter_map(|line| line.trim().strip_prefix("data:"))
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n");
        if let Ok(response) = serde_json::from_str(&data) {
            return Ok(response);
        }
    }
    anyhow::bail!("remote MCP response was neither JSON nor an MCP event stream")
}

fn hermes_mcp_access_token(home: Option<&std::path::Path>, server: &str) -> Result<String> {
    let home = home.context("HERMES_HOME is required for OAuth MCP")?;
    let safe_name: String = server
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(128)
        .collect();
    let tokens: Value = serde_json::from_slice(
        &std::fs::read(home.join("mcp-tokens").join(format!("{safe_name}.json")))
            .context("Hermes OAuth token is missing; authenticate this MCP server in Settings")?,
    )?;
    if let Some(expiry) = tokens["expires_at"].as_f64()
        && expiry
            <= std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs_f64()
    {
        anyhow::bail!("Hermes OAuth token expired; reauthenticate this MCP server in Settings");
    }
    tokens["access_token"]
        .as_str()
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .context("Hermes OAuth token has no access token")
}

fn hermes_mcp_is_oauth(home: Option<&std::path::Path>, server: &str) -> bool {
    let Some(home) = home else { return false };
    let Ok(contents) = std::fs::read_to_string(home.join("config.yaml")) else {
        return false;
    };
    let Ok(config) = serde_yaml::from_str::<Value>(&contents) else {
        return false;
    };
    config["mcp_servers"][server]["auth"].as_str() == Some("oauth")
}

#[cfg(all(test, unix))]
mod tests {
    use super::{
        McpClient, McpHandle, is_sensitive_inherited_env_key, mcp_child_env,
        parse_mcp_http_response,
    };
    use crate::mcp::protocol::McpServerConfig;
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Read, Write};

    #[test]
    fn streamable_http_parses_multiline_sse_data() {
        let response = parse_mcp_http_response(
            "event: message\ndata: {\ndata: \"jsonrpc\":\"2.0\",\ndata: \"id\":1,\ndata: \"result\":{}}\n\n",
        )
        .expect("parse SSE JSON split across data lines");
        assert_eq!(response.id, Some(1));
        assert!(response.result.is_some());
    }

    #[tokio::test]
    async fn timed_out_mcp_request_removes_pending_sender() {
        let (writer_tx, _writer_rx) = tokio::sync::mpsc::channel(1);
        let pending =
            std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
        let handle = McpHandle {
            name: "timeout-test".into(),
            request_id: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
            pending: std::sync::Arc::clone(&pending),
            writer_tx,
            server_info: std::sync::Arc::new(std::sync::RwLock::new(None)),
            capabilities: std::sync::Arc::new(std::sync::RwLock::new(
                crate::mcp::protocol::ServerCapabilities::default(),
            )),
            tools: std::sync::Arc::new(std::sync::RwLock::new(Vec::new())),
            http_protocol_version: None,
            request_timeout: std::time::Duration::from_millis(1),
        };

        assert!(handle.request("tools/list", None).await.is_err());
        assert!(pending.lock().await.is_empty());
    }

    #[test]
    fn inherited_mcp_env_scrubs_provider_credentials() {
        for key in [
            "ANTHROPIC_API_KEY",
            "openai_api_key",
            "CURSOR_ACCESS_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "GOOGLE_APPLICATION_CREDENTIALS",
        ] {
            assert!(is_sensitive_inherited_env_key(key), "must scrub {key}");
        }
        for key in ["PATH", "HOME", "RUST_LOG", "JCODE_OPENROUTER_API_KEY_NAME"] {
            assert!(!is_sensitive_inherited_env_key(key), "must preserve {key}");
        }
    }

    #[test]
    fn explicit_mcp_env_can_opt_a_credential_back_in() {
        let inherited = HashMap::from([
            ("PATH".to_string(), "/bin".to_string()),
            ("ANTHROPIC_API_KEY".to_string(), "daemon-secret".to_string()),
        ]);
        let explicit = HashMap::from([(
            "ANTHROPIC_API_KEY".to_string(),
            "server-specific-secret".to_string(),
        )]);

        let env = mcp_child_env(inherited, &explicit);
        assert_eq!(env.get("PATH").map(String::as_str), Some("/bin"));
        assert_eq!(
            env.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("server-specific-secret")
        );
    }

    /// A minimal fake stdio MCP server (shell script) that reports its own
    /// process cwd as the serverInfo name.
    fn fake_server_config() -> McpServerConfig {
        let script = r#"
while IFS= read -r line; do
  case "$line" in
    *'"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"%s","version":"0"}}}\n' "$PWD"
      ;;
    *'"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}\n'
      ;;
  esac
done
"#;
        McpServerConfig {
            command: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            env: Default::default(),
            shared: false,
            transport: None,
            url: None,
            headers: std::collections::HashMap::new(),
            enabled: None,
            disabled: None,
            timeout_secs: None,
        }
    }

    #[tokio::test]
    async fn connect_in_dir_sets_subprocess_cwd() {
        // Issue #557: owned MCP servers must run in the session project dir.
        let dir = tempfile::tempdir().expect("tempdir");
        let expected = dir.path().canonicalize().expect("canonicalize");

        let client = McpClient::connect_in_dir(
            "cwd-test".to_string(),
            &fake_server_config(),
            Some(dir.path()),
        )
        .await
        .expect("connect");

        let reported = client.server_info().expect("server info").name;
        assert_eq!(
            std::path::Path::new(&reported)
                .canonicalize()
                .expect("canonicalize reported"),
            expected
        );
    }

    #[tokio::test]
    async fn connect_in_dir_missing_dir_falls_back_to_inherited_cwd() {
        let client = McpClient::connect_in_dir(
            "cwd-fallback-test".to_string(),
            &fake_server_config(),
            Some(std::path::Path::new("/nonexistent/jcode-557")),
        )
        .await
        .expect("connect should fall back to inherited cwd");

        let reported = client.server_info().expect("server info").name;
        assert!(!reported.is_empty());
    }

    #[tokio::test]
    async fn streamable_http_uses_hermes_oauth_token_for_discovery_and_calls() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test MCP");
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let mut methods = Vec::new();
            for _ in 0..4 {
                let (stream, _) = listener.accept().expect("accept MCP request");
                let mut reader = BufReader::new(stream);
                let mut first_line = String::new();
                reader.read_line(&mut first_line).unwrap();
                let mut headers = String::new();
                let mut content_length = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    headers.push_str(&line);
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; content_length];
                reader.read_exact(&mut body).unwrap();
                let request: Value = serde_json::from_slice(&body).unwrap();
                methods.push(request["method"].as_str().unwrap().to_string());
                assert!(
                    headers
                        .to_ascii_lowercase()
                        .contains("authorization: bearer probe-token")
                );
                if request["method"] == "initialize" {
                    assert!(
                        !headers
                            .to_ascii_lowercase()
                            .contains("mcp-protocol-version:")
                    );
                } else if request["method"] != "notifications/initialized" {
                    assert!(
                        headers
                            .to_ascii_lowercase()
                            .contains("mcp-protocol-version: 2025-03-26")
                    );
                }
                let response = match request["method"].as_str().unwrap() {
                    "initialize" => Some(
                        json!({"jsonrpc":"2.0","id":request["id"],"result":{"protocolVersion":"2025-03-26","capabilities":{},"serverInfo":{"name":"test-http","version":"1"}}}),
                    ),
                    "tools/list" => Some(
                        json!({"jsonrpc":"2.0","id":request["id"],"result":{"tools":[{"name":"probe","description":"OAuth probe","inputSchema":{"type":"object"}}]}}),
                    ),
                    "tools/call" => Some(
                        json!({"jsonrpc":"2.0","id":request["id"],"result":{"content":[{"type":"text","text":"oauth-ok"}],"isError":false}}),
                    ),
                    "notifications/initialized" => None,
                    method => panic!("unexpected MCP method: {method}"),
                };
                let mut stream = reader.into_inner();
                if let Some(response) = response {
                    let body = response.to_string();
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: test-session\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
                } else {
                    write!(
                        stream,
                        "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                }
            }
            methods
        });

        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("mcp-tokens")).unwrap();
        std::fs::write(
            home.path().join("config.yaml"),
            "mcp_servers:\n  oauth-probe:\n    auth: oauth\n",
        )
        .unwrap();
        std::fs::write(home.path().join("mcp-tokens/oauth-probe.json"), json!({"access_token":"probe-token","expires_at": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 3600}).to_string()).unwrap();
        let previous = std::env::var_os("HERMES_HOME");
        unsafe {
            std::env::set_var("HERMES_HOME", home.path());
        }
        let config = McpServerConfig {
            command: String::new(),
            args: vec![],
            env: HashMap::new(),
            shared: false,
            transport: Some("http".into()),
            url: Some(format!("http://{address}/mcp")),
            headers: HashMap::new(),
            enabled: None,
            disabled: None,
            timeout_secs: None,
        };
        let result = async {
            let client = McpClient::connect("oauth-probe".into(), &config).await?;
            assert_eq!(client.tools()[0].name, "probe");
            let result = client.call_tool("probe", json!({})).await?;
            assert!(matches!(result.content.first(), Some(crate::mcp::protocol::ContentBlock::Text { text }) if text == "oauth-ok"));
            anyhow::Ok(())
        }.await;
        unsafe {
            if let Some(previous) = previous {
                std::env::set_var("HERMES_HOME", previous);
            } else {
                std::env::remove_var("HERMES_HOME");
            }
        }
        result.expect("OAuth MCP HTTP transport");
        assert_eq!(
            server.join().unwrap(),
            [
                "initialize",
                "notifications/initialized",
                "tools/list",
                "tools/call"
            ]
        );
    }
}
