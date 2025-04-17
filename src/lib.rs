mod bindings;

use crate::bindings::exports::ntwk::theater::actor::Guest;
use crate::bindings::exports::ntwk::theater::message_server_client::Guest as MessageServerClient;
use crate::bindings::exports::ntwk::theater::process_handlers::Guest as ProcessHandlers;
use crate::bindings::ntwk::theater::message_server_host::send;
use crate::bindings::ntwk::theater::process::{os_spawn, os_write_stdin, OutputMode};
use crate::bindings::ntwk::theater::runtime::log;
use crate::bindings::ntwk::theater::timing::now;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};

#[derive(Serialize, Deserialize, Clone, Debug)]
struct PendingRequest {
    id: u32,               // MCP request ID
    caller_id: String,     // Actor ID that made the request
    method: String,        // Original method requested
    timestamp: u64,        // When the request was made (for timeouts)
    params: Option<Value>, // Original parameters
}

#[derive(Serialize, Deserialize, Debug)]
struct AppState {
    // Process management
    server_pid: Option<u64>,
    server_initialized: bool,

    // Request tracking
    request_id: AtomicU32,
    pending_requests: HashMap<u32, PendingRequest>,

    // Server capabilities
    server_capabilities: Option<Value>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            server_pid: None,
            server_initialized: false,
            request_id: AtomicU32::new(0),
            pending_requests: HashMap::new(),
            server_capabilities: None,
        }
    }
}

impl AppState {
    // Get the current timestamp in seconds using the Theater timing interface
    fn now() -> Result<u64, String> {
        let timestamp_ms = now();
        // Convert milliseconds to seconds
        Ok(timestamp_ms / 1000)
    }
    // Check if the server supports a specific capability
    fn can_use_capability(&self, capability: &str) -> bool {
        if let Some(capabilities) = &self.server_capabilities {
            // Check if the capability exists in the server's capabilities
            capabilities.get(capability).is_some()
        } else {
            false
        }
    }

    // Check for and handle timed out requests
    fn check_timeouts(&mut self) -> Result<(), String> {
        let now = Self::now()?;

        let timeout_threshold = 30; // 30 seconds timeout

        // Find timed out requests
        let timed_out: Vec<u32> = self
            .pending_requests
            .iter()
            .filter(|(_, req)| now - req.timestamp > timeout_threshold)
            .map(|(id, _)| *id)
            .collect();

        // Send timeout errors to callers
        for id in &timed_out {
            if let Some(req) = self.pending_requests.get(id) {
                let error_message = json!({
                    "request_id": id,
                    "method": req.method.clone(),
                    "result": null,
                    "error": {
                        "code": -32000,
                        "message": "Request timed out"
                    }
                });

                // Best effort send - ignore errors
                let _ = send(
                    &req.caller_id,
                    &serde_json::to_vec(&error_message).unwrap_or_default(),
                );

                log(&format!(
                    "Request {} timed out, sent error to {}",
                    id, req.caller_id
                ));
            }
        }

        // Remove timed out requests
        for id in timed_out {
            self.pending_requests.remove(&id);
        }

        Ok(())
    }
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

// Actor API request structures

#[derive(Serialize, Deserialize, Debug)]
struct McpRequest {
    jsonrpc: String,
    id: u32,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

// Actor API request structures

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
enum McpActorRequest {
    Initialize {
        caller_id: String,
    },
    ToolsList {
        caller_id: String,
    },
    ToolsCall {
        caller_id: String,
        name: String,
        args: Value,
    },
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

#[derive(Serialize, Deserialize, Debug)]
struct InitState {
    server_path: String,
    args: Vec<String>,
}

struct Actor;

impl Guest for Actor {
    fn init(init_state: Option<Vec<u8>>, params: (String,)) -> Result<(Option<Vec<u8>>,), String> {
        log("Initializing mcp-poc actor");
        let (self_id,) = params;
        log(&format!("self id: {}", self_id));

        let app_state = AppState::default();
        log("Created default app state");

        log("Parsing init state");
        log(&format!("Init state: {:?}", init_state));

        let init_state_bytes = init_state.unwrap();
        let init_state: InitState = serde_json::from_slice(&init_state_bytes)
            .map_err(|e| format!("Failed to deserialize init state: {}", e))?;
        log(&format!(
            "Parsed init state: server_path: {}, args: {:?}",
            init_state.server_path, init_state.args
        ));

        // Start the fs-mcp-server process
        let config = bindings::ntwk::theater::process::ProcessConfig {
            program: init_state.server_path,
            args: init_state.args,
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

        let init_message = McpActorRequest::Initialize {
            caller_id: self_id.clone(),
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
            if stdout_data.contains("jsonrpc")
                && (stdout_data.contains("result") || stdout_data.contains("error"))
            {
                // Try to parse as McpResponse
                match serde_json::from_str::<McpResponse>(&stdout_data) {
                    Ok(response) => {
                        log(&format!("Parsed MCP response with ID: {}", response.id));

                        // Check if initialization was successful
                        if response.id == 0 && response.error.is_none() {
                            log("Server initialization successful");
                            app_state.server_initialized = true;
                        }
                    }
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

        // Parse the current state
        let mut app_state: AppState = match state {
            Some(state_bytes) if !state_bytes.is_empty() => serde_json::from_slice(&state_bytes)
                .map_err(|e| format!("Failed to deserialize state: {}", e))?,
            _ => AppState::default(),
        };

        // Check for timed out requests
        if let Err(e) = app_state.check_timeouts() {
            log(&format!("Error checking timeouts: {}", e));
        }

        // Parse the request
        let request = match serde_json::from_slice::<McpActorRequest>(&data) {
            Ok(req) => req,
            Err(e) => {
                log(&format!("Failed to parse request: {}", e));
                return Err("Unknown request format".to_string());
            }
        };

        // Process the request based on its type
        match request {
            McpActorRequest::Initialize { caller_id } => {
                log("Received initialize request");

                // Include proper initialization parameters with protocol version
                let init_params = json!({
                    "protocolVersion": "2024-11-05",
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
            }

            McpActorRequest::ToolsList { caller_id } => {
                log("Received tools_list request");

                // Check if server is initialized
                if !app_state.server_initialized {
                    return Err("MCP server not initialized".to_string());
                }

                // Check if server supports tools capability
                if !app_state.can_use_capability("tools") {
                    return Err("Server does not support tools capability".to_string());
                }

                // Generate request ID
                let id = app_state.generate_request_id();

                // Create pending request record
                let pending = PendingRequest {
                    id,
                    caller_id,
                    method: "tools/list".to_string(),
                    timestamp: AppState::now()?,
                    params: None,
                };

                // Add to pending requests map
                app_state.pending_requests.insert(id, pending);

                // Send tools/list request
                let mcp_request = McpRequest {
                    jsonrpc: "2.0".to_string(),
                    id,
                    method: "tools/list".to_string(),
                    params: None,
                };

                let request_json =
                    serde_json::to_string(&mcp_request).map_err(|e| e.to_string())? + "\n";
                log(&format!("Sending tools/list request: {}", request_json));

                os_write_stdin(app_state.server_pid.unwrap(), request_json.as_bytes())
                    .map_err(|e| format!("Failed to write to stdin: {}", e))?;
            }

            McpActorRequest::ToolsCall {
                caller_id,
                name,
                args,
            } => {
                log("Received tools_call request");

                // Check if server is initialized
                if !app_state.server_initialized {
                    return Err("MCP server not initialized".to_string());
                }

                // Check if server supports tools capability
                if !app_state.can_use_capability("tools") {
                    return Err("Server does not support tools capability".to_string());
                }

                // Generate request ID
                let id = app_state.generate_request_id();

                // Create pending request record
                let pending = PendingRequest {
                    id,
                    caller_id,
                    method: "tools/call".to_string(),
                    timestamp: AppState::now()?,
                    params: Some(json!({
                        "name": name,
                        "arguments": args
                    })),
                };

                // Add to pending requests map
                app_state.pending_requests.insert(id, pending);

                // Send tools/call request
                let mcp_request = McpRequest {
                    jsonrpc: "2.0".to_string(),
                    id,
                    method: "tools/call".to_string(),
                    params: Some(json!({
                        "name": name,
                        "arguments": args
                    })),
                };

                let request_json =
                    serde_json::to_string(&mcp_request).map_err(|e| e.to_string())? + "\n";
                log(&format!("Sending tools/call request: {}", request_json));

                os_write_stdin(app_state.server_pid.unwrap(), request_json.as_bytes())
                    .map_err(|e| format!("Failed to write to stdin: {}", e))?;
            }
        }

        // Serialize the app state
        let updated_state = serde_json::to_vec(&app_state).map_err(|e| e.to_string())?;

        // Return updated state
        Ok((Some(updated_state),))
    }

    fn handle_request(
        state: Option<Vec<u8>>,
        _params: (Vec<u8>,),
    ) -> Result<(Option<Vec<u8>>, (Vec<u8>,)), String> {
        log("Handling request message");
        Ok((state, (vec![],)))
    }

    fn handle_channel_open(
        state: Option<bindings::exports::ntwk::theater::message_server_client::Json>,
        _params: (bindings::exports::ntwk::theater::message_server_client::Json,),
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
        _params: (String,),
    ) -> Result<(Option<bindings::exports::ntwk::theater::message_server_client::Json>,), String>
    {
        Ok((state,))
    }

    fn handle_channel_message(
        state: Option<bindings::exports::ntwk::theater::message_server_client::Json>,
        _params: (
            String,
            bindings::exports::ntwk::theater::message_server_client::Json,
        ),
    ) -> Result<(Option<bindings::exports::ntwk::theater::message_server_client::Json>,), String>
    {
        log("mcp-poc: Received channel message");
        Ok((state,))
    }
}

// This struct is no longer used as we now use the McpActorRequest enum

// Helper function to handle process stdout data in a message
fn handle_process_stdout(
    state: Option<Vec<u8>>,
    request_str: String,
) -> Result<(Option<Vec<u8>>,), String> {
    log("Handling process stdout data");

    // Parse the current state
    let state_bytes = state.unwrap_or_default();
    if state_bytes.is_empty() {
        return Ok((None,));
    }

    let mut app_state: AppState = serde_json::from_slice(&state_bytes)
        .map_err(|e| format!("Failed to deserialize state: {}", e))?;

    // Log the original request string for debugging
    log(&format!("Processing request string: {}", request_str));

    // Extract the data from the request string manually
    let parts: Vec<&str> = request_str
        .trim_matches(|c| c == '[' || c == ']')
        .split(',')
        .collect();
    log(&format!("Split into {} parts", parts.len()));

    if parts.len() >= 3 && parts[0].contains("handle-stdout") {
        // Get process ID (usually the second element)
        let process_id_str = parts[1].trim();
        log(&format!("Process ID string: {}", process_id_str));

        // Extract the remaining parts as the data array
        let data_parts = &parts[2..];
        log(&format!("Data parts count: {}", data_parts.len()));

        // Convert the numeric values to bytes
        let mut bytes = Vec::new();
        for part in data_parts {
            let part = part.trim();
            if let Ok(byte_val) = part.parse::<u8>() {
                bytes.push(byte_val);
            }
        }

        log(&format!("Extracted {} bytes", bytes.len()));

        if !bytes.is_empty() {
            // Try to convert to a string
            if let Ok(stdout_data) = String::from_utf8(bytes.clone()) {
                log(&format!("Converted to string: {}", stdout_data));

                // Check if this is a JSON-RPC response
                // Try to fix malformed JSON if needed
                let json_data = if !stdout_data.starts_with('{') && stdout_data.contains("jsonrpc")
                {
                    log("Attempting to fix malformed JSON response");
                    format!("{{{}", stdout_data)
                } else {
                    stdout_data
                };

                if json_data.contains("jsonrpc") {
                    log(&format!("Processing JSON-RPC response: {}", json_data));

                    match serde_json::from_str::<McpResponse>(&json_data) {
                        Ok(response) => {
                            log(&format!("Parsed MCP response ID: {}", response.id));

                            // Update app state
                            let mut app_state: AppState = serde_json::from_slice(&state_bytes)
                                .map_err(|e| format!("Failed to deserialize state: {}", e))?;

                            // Look up the pending request
                            if let Some(pending) = app_state.pending_requests.remove(&response.id) {
                                log(&format!(
                                    "Found pending request for ID {}: {}",
                                    response.id, pending.method
                                ));

                                // Format a response for the caller
                                let result_message = json!({
                                    "request_id": response.id,
                                    "method": pending.method,
                                    "result": response.result,
                                    "error": response.error
                                });

                                // Send the response back to the calling actor
                                send(
                                    &pending.caller_id,
                                    &serde_json::to_vec(&result_message)
                                        .map_err(|e| e.to_string())?,
                                )
                                .map_err(|e| format!("Failed to send response: {}", e))?;

                                log(&format!("Sent response to caller {}", pending.caller_id));
                            }

                            // Special handling for initialization response
                            if response.id == 0 {
                                if response.error.is_none() {
                                    log("Server initialization successful");
                                    app_state.server_initialized = true;

                                    // Extract server info and capabilities if available
                                    if let Some(result) = &response.result {
                                        log(&format!("Initialization result: {}", result));

                                        // Store the server capabilities for later reference
                                        app_state.server_capabilities = Some(result.clone());
                                    }

                                    // Send the initialized notification after initializing
                                    let pid = app_state.server_pid.expect("Missing server PID");
                                    let init_notification = json!({
                                        "jsonrpc": "2.0",
                                        "method": "notifications/initialized"
                                    });

                                    let notification_json =
                                        serde_json::to_string(&init_notification).map_err(|e| {
                                            format!("Failed to serialize notification: {}", e)
                                        })? + "\n";

                                    log("Sending initialized notification");
                                    os_write_stdin(pid, notification_json.as_bytes()).map_err(
                                        |e| format!("Failed to send notification: {}", e),
                                    )?;

                                    log("MCP connection established. Ready for commands.");
                                } else if let Some(error) = &response.error {
                                    log(&format!(
                                        "Server initialization failed: {} (code: {})",
                                        error.message, error.code
                                    ));
                                }
                            }

                            // Serialize and return the updated state
                            let updated_state = serde_json::to_vec(&app_state)
                                .map_err(|e| format!("Failed to serialize updated state: {}", e))?;

                            return Ok((Some(updated_state),));
                        }
                        Err(e) => {
                            log(&format!("Failed to parse MCP response: {}", e));

                            // Try parsing with a more lenient approach
                            if json_data.contains("error") {
                                log("Attempting to extract error information");
                                if let Ok(value) =
                                    serde_json::from_str::<serde_json::Value>(&json_data)
                                {
                                    if let Some(error) = value.get("error") {
                                        log(&format!("Error in response: {}", error));
                                    }

                                    // Try to extract the ID to match with pending requests
                                    if let Some(id) = value.get("id").and_then(|id| id.as_u64()) {
                                        let id = id as u32;
                                        if let Some(pending) =
                                            app_state.pending_requests.get(&id).cloned()
                                        {
                                            // Remove from pending requests
                                            app_state.pending_requests.remove(&id);
                                            // Send error response to the caller
                                            let error_message = json!({
                                                "request_id": id,
                                                "method": pending.method,
                                                "result": null,
                                                "error": {
                                                    "code": -32603,
                                                    "message": "Internal server error: malformed response"
                                                }
                                            });

                                            // Best effort - ignore errors
                                            let _ = send(
                                                &pending.caller_id,
                                                &serde_json::to_vec(&error_message)
                                                    .unwrap_or_default(),
                                            );

                                            log(&format!(
                                                "Sent error response to caller {}",
                                                pending.caller_id
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            } else {
                log("Not valid UTF-8 data");
            }
        }
    }

    // Serialize and return the updated state
    let updated_state = serde_json::to_vec(&app_state)
        .map_err(|e| format!("Failed to serialize updated state: {}", e))?;

    Ok((Some(updated_state),))
}

bindings::export!(Actor with_types_in bindings);
