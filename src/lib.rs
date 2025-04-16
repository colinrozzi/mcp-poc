mod bindings;

use crate::bindings::exports::ntwk::theater::actor::Guest;
use crate::bindings::exports::ntwk::theater::message_server_client::Guest as MessageServerClient;
use crate::bindings::exports::ntwk::theater::process_handlers::Guest as ProcessHandlers;
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
        let (param,) = params;
        log(&format!("Init parameter: {}", param));

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

// Helper functions for MCP protocol interaction

// Extract complete JSON objects from a buffer string
fn extract_json_messages(buffer: &mut String) -> Option<Vec<String>> {
    let mut messages = Vec::new();
    let mut start_index = 0;

    // Find JSON objects in the buffer
    while let Some(start) = buffer[start_index..].find('{') {
        let start_pos = start_index + start;
        let mut depth = 0;
        let mut found_end = false;

        // Find the matching closing brace
        for (i, ch) in buffer[start_pos..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        let end_pos = start_pos + i + 1;
                        messages.push(buffer[start_pos..end_pos].to_string());
                        start_index = end_pos;
                        found_end = true;
                        break;
                    }
                }
                _ => {}
            }
        }

        if !found_end {
            break; // Incomplete JSON object, wait for more data
        }
    }

    // Remove processed messages from the buffer
    if !messages.is_empty() {
        *buffer = buffer[start_index..].to_string();
        Some(messages)
    } else {
        None
    }
}

// Process an MCP response message
fn process_mcp_response(app_state: &mut AppState, response_text: &str) -> Result<(), String> {
    log(&format!("Processing MCP response: {}", response_text));

    // Parse the response JSON
    let response: McpResponse = serde_json::from_str(response_text)
        .map_err(|e| format!("Failed to parse MCP response: {}", e))?;

    // Find the matching request
    let request_index = app_state
        .pending_requests
        .iter()
        .position(|req| req.id == response.id);

    if let Some(index) = request_index {
        let request = app_state.pending_requests.remove(index);
        log(&format!("Found matching request: {:?}", request));

        // Handle specific MCP responses based on the request method
        match request.method.as_str() {
            "initialize" => {
                log("Received initialize response");
                if response.error.is_none() {
                    app_state.server_initialized = true;

                    // Send a request to list tools
                    let list_tools_request = create_list_tools_request()?;
                    send_mcp_request(app_state, list_tools_request)?;
                } else {
                    log(&format!("Initialize error: {:?}", response.error));
                }
            }
            "listTools" => {
                log("Received listTools response");
                if let Some(result) = response.result {
                    if let Some(tools) = result.get("tools") {
                        if let Some(tools_array) = tools.as_array() {
                            app_state.available_tools = tools_array
                                .iter()
                                .filter_map(|tool| tool.get("name")?.as_str().map(String::from))
                                .collect();
                            log(&format!("Available tools: {:?}", app_state.available_tools));
                        }
                    }
                } else {
                    log(&format!("listTools error: {:?}", response.error));
                }
            }
            "callTool" => {
                log("Received callTool response");
                if let Some(result) = response.result {
                    // Check if this was a list_allowed_dirs call
                    if let Some(params) = &request.params {
                        if let Some(tool_name) = params.get("name").and_then(|n| n.as_str()) {
                            if tool_name == "list_allowed_dirs" {
                                // Parse allowed directories from the response
                                if let Some(content) = result.get("content") {
                                    if let Some(content_array) = content.as_array() {
                                        if let Some(first_content) = content_array.first() {
                                            if let Some(text) =
                                                first_content.get("text").and_then(|t| t.as_str())
                                            {
                                                if let Ok(dirs_value) =
                                                    serde_json::from_str::<Value>(text)
                                                {
                                                    if let Some(dirs_array) = dirs_value.as_array()
                                                    {
                                                        app_state.allowed_directories = dirs_array
                                                            .iter()
                                                            .filter_map(|dir| {
                                                                dir.as_str().map(String::from)
                                                            })
                                                            .collect();
                                                        log(&format!(
                                                            "Allowed directories: {:?}",
                                                            app_state.allowed_directories
                                                        ));
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                } else {
                    log(&format!("callTool error: {:?}", response.error));
                }
            }
            _ => {
                log(&format!("Received response for method: {}", request.method));
            }
        }
    } else {
        log(&format!(
            "No matching request found for response ID: {}",
            response.id
        ));
    }

    Ok(())
}

// Create an initialize request
fn create_initialize_request() -> Result<McpRequest, String> {
    let id = next_request_id();
    Ok(McpRequest {
        jsonrpc: "2.0".to_string(),
        id,
        method: "initialize".to_string(),
        params: Some(json!({
            "clientInfo": {
                "name": "mcp-poc",
                "version": "0.1.0"
            }
        })),
    })
}

// Create a listTools request
fn create_list_tools_request() -> Result<McpRequest, String> {
    let id = next_request_id();
    Ok(McpRequest {
        jsonrpc: "2.0".to_string(),
        id,
        method: "listTools".to_string(),
        params: None,
    })
}

// Create a list_allowed_dirs tool call request
fn create_list_allowed_dirs_request() -> Result<McpRequest, String> {
    let id = next_request_id();
    Ok(McpRequest {
        jsonrpc: "2.0".to_string(),
        id,
        method: "callTool".to_string(),
        params: Some(json!({
            "name": "list_allowed_dirs",
            "arguments": {}
        })),
    })
}

// Create a list tool call request
fn create_list_request(path: &str) -> Result<McpRequest, String> {
    let id = next_request_id();
    Ok(McpRequest {
        jsonrpc: "2.0".to_string(),
        id,
        method: "callTool".to_string(),
        params: Some(json!({
            "name": "list",
            "arguments": {
                "path": path,
                "recursive": false,
                "include_hidden": false,
                "metadata": true
            }
        })),
    })
}

// Send an MCP request to the server
fn send_mcp_request(app_state: &mut AppState, request: McpRequest) -> Result<(), String> {
    if let Some(pid) = app_state.server_pid {
        // Convert request to JSON
        let request_json = serde_json::to_string(&request)
            .map_err(|e| format!("Failed to serialize request: {}", e))?;

        log(&format!("Sending MCP request: {}", request_json));

        // Add newline to the request
        let request_bytes = format!("{}\n", request_json).into_bytes();

        // Send the request to the server
        os_write_stdin(pid, &request_bytes)
            .map_err(|e| format!("Failed to write to server stdin: {}", e))?;

        // Store the pending request
        app_state.pending_requests.push(PendingRequest {
            id: request.id,
            method: request.method,
            params: request.params,
        });

        Ok(())
    } else {
        Err("No server process is running".to_string())
    }
}

bindings::export!(Actor with_types_in bindings);
