mod bindings;

use crate::bindings::exports::ntwk::theater::actor::Guest;
use crate::bindings::exports::ntwk::theater::message_server_client::Guest as MessageServerClient;
use crate::bindings::exports::ntwk::theater::process_handlers::Guest as ProcessHandlers;
use crate::bindings::ntwk::theater::message_server_host::{respond_to_request, send};
use crate::bindings::ntwk::theater::process::{os_spawn, os_write_stdin, OutputMode};
use crate::bindings::ntwk::theater::runtime::log;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;

#[derive(Serialize, Deserialize, Debug, Clone)]
struct McpRequest {
    jsonrpc: String,
    id: String,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

// Actor API request structures
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
enum McpActorRequest {
    ToolsList {},
    ToolsCall { name: String, args: Value },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct McpResponse {
    jsonrpc: String,
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<McpError>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct McpError {
    code: i32,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

#[derive(Serialize, Deserialize, Debug)]
struct InitState {
    command: String,
    args: Vec<String>,
}

const INITIALIZE_REQUEST_ID: &str = "initialize";

#[derive(Serialize, Deserialize, Debug)]
struct AppState {
    id: String,

    // Process management
    server_pid: Option<u64>,
    server_initialized: bool,

    unsent_requests: Vec<McpRequest>,

    // Request tracking
    pending_requests: HashMap<String, McpRequest>,

    // Server capabilities
    server_capabilities: Option<Value>,
}

impl AppState {
    fn send_message(
        &self,
        request_id: &str,
        method: &str,
        params: Option<Value>,
    ) -> Result<(), String> {
        let request = McpRequest {
            jsonrpc: "2.0".to_string(),
            id: request_id.to_string(),
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

    fn send_next_unsent(&mut self) -> Result<(), String> {
        if let Some(request) = self.unsent_requests.pop() {
            log(&format!("Sending next unsent request: {:?}", request));
            let request_json = serde_json::to_string(&request).map_err(|e| e.to_string())? + "\n";
            os_write_stdin(self.server_pid.unwrap(), request_json.as_bytes())
                .map_err(|e| format!("Failed to write to stdin: {}", e))?;

            // If there are more unsent requests, send ourselves a message to send the next one
            if !self.unsent_requests.is_empty() {
                log("Scheduling next unsent request");
                let send_request = SendUnsentRequests {};
                let send_request_json = serde_json::to_string(&send_request)
                    .map_err(|e| format!("Failed to serialize send request: {}", e))?;
                send(&self.id, send_request_json.as_bytes())
                    .map_err(|e| format!("Failed to send message: {}", e))?;
            } else {
                log("No more unsent requests");
            }
        }

        Ok(())
    }
}

struct Actor;

impl Guest for Actor {
    fn init(init_state: Option<Vec<u8>>, params: (String,)) -> Result<(Option<Vec<u8>>,), String> {
        log("Initializing mcp-poc actor");
        let (self_id,) = params;
        log(&format!("self id: {}", self_id));

        let app_state = AppState {
            id: self_id,
            server_pid: None,
            server_initialized: false,
            unsent_requests: vec![],
            pending_requests: HashMap::new(),
            server_capabilities: None,
        };

        log("Created default app state");

        log("Parsing init state");
        log(&format!("Init state: {:?}", init_state));

        let init_state_bytes = init_state.unwrap();
        let init_state: InitState = serde_json::from_slice(&init_state_bytes)
            .map_err(|e| format!("Failed to deserialize init state: {}", e))?;
        log(&format!(
            "Parsed init state: command: {}, args: {:?}",
            init_state.command, init_state.args
        ));

        // Start the fs-mcp-server process
        let config = bindings::ntwk::theater::process::ProcessConfig {
            program: init_state.command,
            args: init_state.args,
            env: vec![],
            cwd: None,
            // an absurdly large buffer size to avoid truncation
            buffer_size: 1024 * 1024 * 1024,
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

        // intialize the server
        log("Sending initialize request to MCP server");
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
        updated_state.send_message(INITIALIZE_REQUEST_ID, "initialize", Some(init_params))?;

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
        let state_bytes = state.unwrap();

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
        log(&format!("Process {} stdout: {:?}", pid, data));

        // Try to parse the data as UTF-8
        if let Ok(stdout_data) = String::from_utf8(data.clone()) {
            log(&format!("Received stdout: [{}] {} ", pid, stdout_data));

            // Process the stdout data using our specialized handler
            return handle_process_stdout(state, stdout_data);
        } else {
            log("Received non-UTF8 data on stdout");
            log(&format!("non-UTF8 data: [{}] {:?}", pid, data).to_string());
        }

        // If we can't parse as UTF-8, just return the state unchanged
        Ok((state,))
    }

    fn handle_stderr(
        state: Option<Vec<u8>>,
        params: (u64, Vec<u8>),
    ) -> Result<(Option<Vec<u8>>,), String> {
        let (pid, data) = params;

        // Only log stderr output
        if let Ok(stderr_data) = String::from_utf8(data.clone()) {
            log(&format!("Process {} stderr: {}", pid, stderr_data));
        } else {
            log(&format!(
                "Process {} stderr: [non-UTF8 data] {:?}",
                pid, data
            ));
        }

        Ok((state,))
    }
}

#[derive(Serialize, Deserialize, Debug)]
struct SendUnsentRequests;

impl MessageServerClient for Actor {
    fn handle_send(
        state: Option<Vec<u8>>,
        params: (Vec<u8>,),
    ) -> Result<(Option<Vec<u8>>,), String> {
        log("Handling send message");

        let mut app_state: AppState = match state {
            Some(state_bytes) if !state_bytes.is_empty() => serde_json::from_slice(&state_bytes)
                .map_err(|e| format!("Failed to deserialize state: {}", e))?,
            _ => return Err("Invalid state".to_string()),
        };

        // Parse the request
        match serde_json::from_slice::<SendUnsentRequests>(&params.0) {
            Ok(_) => {
                log("Received send unsent requests message");
                // Send the next unsent request
                let result = app_state.send_next_unsent();
                if let Err(e) = result {
                    log(&format!("Failed to send next unsent request: {}", e));
                    return Err("Failed to send next unsent request".to_string());
                }
            }
            Err(e) => {
                log(&format!("Failed to parse request: {}", e));
                return Err("Unknown request format".to_string());
            }
        };

        let state_bytes = serde_json::to_vec(&app_state).map_err(|e| e.to_string())?;
        Ok((Some(state_bytes),))
    }

    fn handle_request(
        state: Option<Vec<u8>>,
        params: (String, Vec<u8>),
    ) -> Result<(Option<Vec<u8>>, (Option<Vec<u8>>,)), String> {
        log("Handling request message");
        let (request_id, request) = params;
        log(&format!("Request ID: {}", request_id));
        log(&format!("Request data: {:?}", request));

        // Parse the current state
        let mut app_state: AppState = match state {
            Some(state_bytes) if !state_bytes.is_empty() => serde_json::from_slice(&state_bytes)
                .map_err(|e| format!("Failed to deserialize state: {}", e))?,
            _ => return Err("Invalid state".to_string()),
        };

        // Parse the request
        let request = match serde_json::from_slice::<McpActorRequest>(&request) {
            Ok(req) => req,
            Err(e) => {
                log(&format!("Failed to parse request: {}", e));
                return Err("Unknown request format".to_string());
            }
        };

        // Process the request based on its type
        let mcp_request = match request {
            McpActorRequest::ToolsList {} => {
                log("Received tools_list request");

                // Send tools/list request
                McpRequest {
                    jsonrpc: "2.0".to_string(),
                    id: request_id,
                    method: "tools/list".to_string(),
                    params: None,
                }
            }

            McpActorRequest::ToolsCall { name, args } => {
                log("Received tools_call request");

                // Check if server is initialized
                if !app_state.server_initialized {
                    return Err("MCP server not initialized".to_string());
                }

                // Send tools/call request
                McpRequest {
                    jsonrpc: "2.0".to_string(),
                    id: request_id,
                    method: "tools/call".to_string(),
                    params: Some(json!({
                        "name": name,
                        "arguments": args
                    })),
                }
            }
        };

        // Add the request to the unsent requests
        app_state.unsent_requests.push(mcp_request);

        // If the server is initialized, send the next unsent request
        if app_state.server_initialized {
            log("Server initialized, sending next unsent request");
            app_state.send_next_unsent()?;
        } else {
            log("Server not initialized, deferring request");
        }

        // Serialize the app state
        let updated_state = serde_json::to_vec(&app_state).map_err(|e| e.to_string())?;

        // Return updated state
        Ok((Some(updated_state), (None,)))
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

// Helper function to handle process stdout data in a message
fn handle_process_stdout(
    state: Option<Vec<u8>>,
    stdout_data: String,
) -> Result<(Option<Vec<u8>>,), String> {
    log("Handling process stdout data");

    // Parse the current state
    let state_bytes = state.unwrap_or_default();
    if state_bytes.is_empty() {
        return Ok((None,));
    }

    let mut app_state: AppState = serde_json::from_slice(&state_bytes)
        .map_err(|e| format!("Failed to deserialize state: {}", e))?;

    match serde_json::from_str::<McpResponse>(&stdout_data) {
        Ok(response) => {
            log(&format!("Parsed MCP response: {:?}", response));

            if response.id == INITIALIZE_REQUEST_ID {
                log("Received initialization response");
                if response.error.is_none() {
                    log("Server initialization successful");
                    app_state.server_initialized = true;

                    // Send the initialized notification after initializing
                    let pid = app_state.server_pid.expect("Missing server PID");
                    let init_notification = json!({
                                "jsonrpc": "2.0",
                    "method": "notifications/initialized"
                            });

                    let notification_json = serde_json::to_string(&init_notification)
                        .map_err(|e| format!("Failed to serialize notification: {}", e))?
                        + "\n";

                    log("Sending initialized notification");
                    os_write_stdin(pid, notification_json.as_bytes())
                        .map_err(|e| format!("Failed to send notification: {}", e))?;

                    log("MCP connection established. Ready for commands.");
                    app_state.server_initialized = true;
                    if app_state.unsent_requests.is_empty() {
                        log("No unsent requests to send");
                    } else {
                        log("Sending next unsent request");
                        app_state.send_next_unsent()?;
                    }
                } else if let Some(error) = &response.error {
                    log(&format!(
                        "Server initialization failed: {} (code: {})",
                        error.message, error.code
                    ));
                }
            } else {
                respond_to_request(
                    &response.id,
                    &serde_json::to_vec(&response).map_err(|e| e.to_string())?,
                )
                .map_err(|e| format!("Failed to respond to request: {}", e))?;
            }

            return Ok((Some(state_bytes),));
        }
        Err(e) => {
            log(&format!("Failed to parse MCP response: {}", e));
        }
    }

    // Serialize and return the updated state
    let updated_state = serde_json::to_vec(&app_state)
        .map_err(|e| format!("Failed to serialize updated state: {}", e))?;

    Ok((Some(updated_state),))
}

bindings::export!(Actor with_types_in bindings);
