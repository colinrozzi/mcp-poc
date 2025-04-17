# MCP-POC

A Theater actor that integrates with the Model Context Protocol (MCP), specifically with the fs-mcp-server.

## Building

To build the actor:

```bash
cargo build --target wasm32-unknown-unknown --release
```

## Running

To run the actor with Theater:

```bash
theater start manifest.toml
```

## Features

This actor integrates with the fs-mcp-server and supports:

- Launching and initializing the MCP server
- Communicating with the server using JSON-RPC
- Handling MCP server responses
- Processing filesystem operations via MCP

## API

You can interact with this actor using the following commands:

- `initialize` - Initialize the MCP server
- `list_allowed_dirs` - List the directories allowed for filesystem operations
- More commands will be added as the implementation progresses

## Examples

```bash
# Start the actor and get its ID
ACTOR_ID=$(theater start manifest.toml)

# Initialize the MCP server
theater message $ACTOR_ID initialize

# List allowed directories
theater message $ACTOR_ID list_allowed_dirs
```

## Testing

To test the actor, use the provided scripts:

```bash
# Make all scripts executable
chmod +x ./make-executable.sh
./make-executable.sh

# Run the interactive test script
./test-mcp.sh

# In another terminal, monitor the logs with filtering options
./monitor-logs.sh
```

### Testing Tips

1. The MCP server requires a `protocolVersion` field in the initialization parameters, which should be set to `2024-11-05`.

2. After a successful initialization response, the client must send an `initialized` notification to the server.

3. To call MCP tools (like `list_allowed_dirs`), use the `tools/invoke` method with the tool name and parameters.
