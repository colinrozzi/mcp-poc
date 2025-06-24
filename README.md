# MCP-POC

A Theater actor that translates actor requests to a model context protocol server.

## Downloading wit depedencies

To download the WIT dependencies, run:

```bash
wkg wit fetch
```

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
