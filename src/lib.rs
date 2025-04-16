mod bindings;

use crate::bindings::exports::ntwk::theater::actor::Guest;
use crate::bindings::exports::ntwk::theater::message_server_client::Guest as MessageServerClient;
use crate::bindings::exports::ntwk::theater::process_handlers::Guest as ProcessHandlers;
use crate::bindings::ntwk::theater::message_server_host::send;
use crate::bindings::ntwk::theater::process::{
    os_kill, os_signal, os_spawn, os_status, os_write_stdin, OutputMode,
};
use crate::bindings::ntwk::theater::runtime::log;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU32, Ordering};

// Counter for generating unique request IDs
static REQUEST_ID: AtomicU32 = AtomicU32::new(1);

// Get the next request ID
fn next_request_id() -> u32 {
    REQUEST_ID.fetch_add(1, Ordering::SeqCst)
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct PendingRequest {
    id: u32,
    method: String,
    params: Option<Value>,
}

#[derive(Serialize, Deserialize, Debug)]
struct AppState {
    // Process management
    server_pid: Option<u64>,
    server_initialized: bool,

    // MCP protocol state
    pending_requests: Vec<PendingRequest>,
    response_buffer: String,

    // Server capabilities
    available_tools: Vec<String>,
    allowed_directories: Vec<String>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            server_pid: None,
            server_initialized: false,
            pending_requests: Vec::new(),
            response_buffer: String::new(),
            available_tools: Vec::new(),
            allowed_directories: Vec::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
struct McpRequest {
    jsonrpc: String,
    id: u32,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

#[derive(Serialize, Deserialize, Debug)]
struct McpResponse {
    jsonrpc: String,
    id: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<McpError>,
}

#[derive(Serialize, Deserialize, Debug)]
struct McpError {
    code: i32,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

struct Actor;

impl Guest for Actor {
    fn init(_state: Option<Vec<u8>>, params: (String,)) -> Result<(Option<Vec<u8>>,), String> {
        log("Initializing mcp-poc actor");
        let (self_id,) = params;
        log(&format!("self id: {}", self_id));

        let app_state = AppState::default();
        log("Created default app state");

        // Path to the fs-mcp-server executable
        let server_path =
            "/Users/colinrozzi/work/mcp-servers/fs-mcp-server/target/debug/fs-mcp-server";

        // Start the fs-mcp-server process
        let config = bindings::ntwk::theater::process::ProcessConfig {
            program: server_path.to_string(),
            args: vec![
                "--allowed-dirs".to_string(),
                "/Users/colinrozzi/work".to_string(),
            ],
            env: vec![],
            cwd: None,
            buffer_size: 4096, // Larger buffer for JSON messages
            chunk_size: None,
            stdout_mode: OutputMode::Raw,
            stderr_mode: OutputMode::Raw,
        };

        log("Creating MCP server process config");
        log(&format!("Process config: {:?}", config));

        // Spawn the MCP server process
        log("Spawning fs-mcp-server process");
        let pid = os_spawn(&config).map_err(|e| e.to_string())?;
        log(&format!("fs-mcp-server process spawned with pid: {}", pid));

        // Update the app state with the server PID
        let mut updated_state = app_state;
        updated_state.server_pid = Some(pid);

        // Serialize the app state
        let state_bytes = serde_json::to_vec(&updated_state).map_err(|e| e.to_string())?;

        send(&self_id, b"init");

        // Return the updated state
        Ok((Some(state_bytes),))
    }
}

impl ProcessHandlers for Actor {
    fn handle_exit(
        state: Option<Vec<u8>>,
        params: (u64, i32),
    ) -> Result<(Option<Vec<u8>>,), String> {
        let (pid, exit_code) = params;
        log(&format!("Process {} exited with code {}", pid, exit_code));

        // Parse the current state
        let state_bytes = state.unwrap_or_default();
        if state_bytes.is_empty() {
            return Ok((None,));
        }

        let app_state: AppState = serde_json::from_slice(&state_bytes)
            .map_err(|e| format!("Failed to deserialize state: {}", e))?;

        // If this is our server process, crash the actor so the supervisor can restart it
        if app_state.server_pid == Some(pid) {
            // Only intentional shutdown (exit code 0) is considered normal
            if exit_code != 0 {
                log("MCP server process has crashed - crashing actor to trigger restart");
                return Err(format!(
                    "MCP server process {} terminated unexpectedly with exit code {}",
                    pid, exit_code
                ));
            } else {
                log("MCP server process has shutdown normally");
            }
        }

        // For normal shutdown or other processes, just return the state unchanged
        Ok((Some(state_bytes),))
    }

    fn handle_stdout(
        state: Option<Vec<u8>>,
        params: (u64, Vec<u8>),
    ) -> Result<(Option<Vec<u8>>,), String> {
        let (pid, data) = params;

        // Try to parse the data as UTF-8
        if let Ok(stdout_data) = String::from_utf8(data.clone()) {
            log(&format!("Received stdout: [{}] {} ", pid, stdout_data));
        } else {
            log("Received non-UTF8 data on stdout");
            log(&format!("non-UTF8 data: [{}] {:?}", pid, data).to_string());
        }

        Ok((state,))
    }

    fn handle_stderr(
        state: Option<Vec<u8>>,
        params: (u64, Vec<u8>),
    ) -> Result<(Option<Vec<u8>>,), String> {
        let (pid, data) = params;

        // Parse the current state
        let state_bytes = state.unwrap_or_default();
        if state_bytes.is_empty() {
            return Ok((None,));
        }

        // Only log stderr output
        if let Ok(stderr_data) = String::from_utf8(data.clone()) {
            log(&format!("Process {} stderr: {}", pid, stderr_data));
        } else {
            log(&format!(
                "Process {} stderr: [non-UTF8 data] {:?}",
                pid, data
            ));
        }

        Ok((Some(state_bytes),))
    }
}

impl MessageServerClient for Actor {
    fn handle_send(
        state: Option<Vec<u8>>,
        params: (Vec<u8>,),
    ) -> Result<(Option<Vec<u8>>,), String> {
        log("Handling send message");
        let (data,) = params;

        // Parse the current state
        let state_bytes = state.unwrap_or_default();
        if state_bytes.is_empty() {
            return Ok((None,));
        }

        let app_state: AppState = serde_json::from_slice(&state_bytes)
            .map_err(|e| format!("Failed to deserialize state: {}", e))?;

        // Return state unchanged
        Ok((Some(state_bytes),))
    }

    fn handle_request(
        state: Option<Vec<u8>>,
        params: (Vec<u8>,),
    ) -> Result<(Option<Vec<u8>>, (Vec<u8>,)), String> {
        log("Handling request message");
        Ok((state, (vec![],)))
    }

    fn handle_channel_open(
        state: Option<bindings::exports::ntwk::theater::message_server_client::Json>,
        params: (bindings::exports::ntwk::theater::message_server_client::Json,),
    ) -> Result<
        (
            Option<bindings::exports::ntwk::theater::message_server_client::Json>,
            (bindings::exports::ntwk::theater::message_server_client::ChannelAccept,),
        ),
        String,
    > {
        Ok((
            state,
            (
                bindings::exports::ntwk::theater::message_server_client::ChannelAccept {
                    accepted: true,
                    message: None,
                },
            ),
        ))
    }

    fn handle_channel_close(
        state: Option<bindings::exports::ntwk::theater::message_server_client::Json>,
        params: (String,),
    ) -> Result<(Option<bindings::exports::ntwk::theater::message_server_client::Json>,), String>
    {
        Ok((state,))
    }

    fn handle_channel_message(
        state: Option<bindings::exports::ntwk::theater::message_server_client::Json>,
        params: (
            String,
            bindings::exports::ntwk::theater::message_server_client::Json,
        ),
    ) -> Result<(Option<bindings::exports::ntwk::theater::message_server_client::Json>,), String>
    {
        log("mcp-poc: Received channel message");
        Ok((state,))
    }
}

bindings::export!(Actor with_types_in bindings);
