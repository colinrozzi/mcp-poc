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

    request_id: AtomicU32,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            server_pid: None,
            server_initialized: false,
            request_id: AtomicU32::new(0),
        }
    }
}

impl AppState {
    fn send_message(&self, method: &str, params: Option<Value>) -> Result<(), String> {
        let id = self.generate_request_id();
        let request = McpRequest {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.to_string(),
            params,
        };

        let request_json = serde_json::to_string(&request).map_err(|e| e.to_string())? + "\n";
        log(&format!("Sending message: {}", request_json));

        // Send the message to the server
        os_write_stdin(self.server_pid.unwrap(), request_json.as_bytes())
            .expect("Failed to write to stdin");
        Ok(())
    }

    fn generate_request_id(&self) -> u32 {
        let id = self.request_id.fetch_add(1, Ordering::SeqCst);
        log(&format!("Generated request ID: {}", id));
        id
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
                "/Users/colinrozzi/work/tmp".to_string(),
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

        let init_message = Request {
            method: "initialize".to_string(),
            params: None,
        };

        send(
            &self_id,
            &serde_json::to_vec(&init_message).expect("Failed to serialize init message"),
        )
        .map_err(|e| format!("Failed to send init message: {}", e))?;

        // Serialize the app state
        let state_bytes = serde_json::to_vec(&updated_state).map_err(|e| e.to_string())?;

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

        // Parse the current state
        let state_bytes = state.unwrap_or_default();
        if state_bytes.is_empty() {
            return Ok((None,));
        }

        let mut app_state: AppState = serde_json::from_slice(&state_bytes)
            .map_err(|e| format!("Failed to deserialize state: {}", e))?;

        // Try to parse the data as UTF-8
        if let Ok(stdout_data) = String::from_utf8(data.clone()) {
            log(&format!("Received stdout: [{}] {} ", pid, stdout_data));
            
            // Check if this looks like a JSON-RPC response
            if stdout_data.contains("jsonrpc") && (stdout_data.contains("result") || stdout_data.contains("error")) {
                // Try to parse as McpResponse
                match serde_json::from_str::<McpResponse>(&stdout_data) {
                    Ok(response) => {
                        log(&format!("Parsed MCP response with ID: {}", response.id));
                        
                        // Check if initialization was successful
                        if response.id == 0 && response.error.is_none() {
                            log("Server initialization successful");
                            app_state.server_initialized = true;
                        }
                    },
                    Err(e) => {
                        log(&format!("Failed to parse MCP response: {}", e));
                    }
                }
            }
        } else {
            log("Received non-UTF8 data on stdout");
            log(&format!("non-UTF8 data: [{}] {:?}", pid, data).to_string());
        }

        // Update state
        let updated_state = serde_json::to_vec(&app_state).map_err(|e| e.to_string())?;
        Ok((Some(updated_state),))
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
        log(&format!("Data: {:?}", data));

        // Try to make sense of the array-like data
        let request_str = String::from_utf8(data.clone())
            .map_err(|e| format!("Failed to convert data to string: {}", e))?;

        // Check if this is a process message (handle-stdout)
        if request_str.starts_with("[") && request_str.contains("handle-stdout") {
            log("Received process stdout data");
            // This is process stdout data, we should handle it specially
            return handle_process_stdout(state, request_str);
        }

        // Normal case - deserialize as a regular request
        let request: Request = serde_json::from_slice(&data)
            .map_err(|e| format!("Failed to deserialize request: {}", e))?;
        let app_state: AppState = serde_json::from_slice(&state.unwrap_or_default())
            .map_err(|e| format!("Failed to deserialize state: {}", e))?;

        match request.method.as_str() {
            "initialize" => {
                log("Received initialize request");
                
                // Include proper initialization parameters
                let init_params = json!({
                    "capabilities": {
                        "tools": {
                            "supportsFileOperations": true
                        }
                    },
                    "clientInfo": {
                        "name": "mcp-poc",
                        "version": "0.1.0"
                    }
                });
                
                log("Sending initialize request to MCP server with parameters");
                app_state.send_message("initialize", Some(init_params))?;
            },
            "list_allowed_dirs" => {
                log("Sending list_allowed_dirs request");
                app_state.send_message("list_allowed_dirs", None)?;
            },
            _ => {
                log(&format!("Unknown method: {}", request.method));
            }
        }

        // Serialize the app state
        let state_bytes = serde_json::to_vec(&app_state).map_err(|e| e.to_string())?;

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

#[derive(Serialize, Deserialize, Debug)]
struct Request {
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

bindings::export!(Actor with_types_in bindings);
