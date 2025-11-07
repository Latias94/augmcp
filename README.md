# augmcp

Rust MCP server for codebase indexing and semantic search, inspired by acemcp, implemented in Rust with the rmcp SDK. It uploads incremental code blobs to an Augment Code backend and retrieves formatted, human‑readable context for your queries.

- Chinese docs: see [README_CN.md](README_CN.md)
- Reference (original concept): https://github.com/qy527145/acemcp

## Highlights

- Automatic incremental indexing before search (no manual step required)
- .gitignore + custom exclude patterns
- Multi‑encoding file reading (UTF‑8 → GBK → GB2312 → ISO‑8859‑1; fallback to UTF‑8 lossy)
- Large file splitting by max lines per blob (default 800)
- Batch upload with exponential backoff retries
- Non‑streaming retrieval (single formatted text)
- Transports: stdio and streamable HTTP (axum)
- Optional REST endpoints for “index + search” convenience
- All data under `~/.augmcp` (config, data, logs)



## Installation

Install via cargo-binstall (recommended):

`
# From GitHub Releases (no crates.io publish required)
cargo binstall --github Latias94/augmcp augmcp

# Once published to crates.io (optional)
cargo binstall augmcp
`

Or build from source:

```
# Clone
git clone <your repo url>
cd augmcp

# Build release binary
cargo build --release
# Optional install to cargo bin
cargo install --path .
```

## Configuration

First run auto‑creates `~/.augmcp/settings.toml` with defaults:

```
batch_size = 10
max_lines_per_blob = 800
base_url = "https://d5.api.augmentcode.com/"
token = "your-bearer-token-here"
text_extensions = [".py", ".js", ".ts", ...]
exclude_patterns = [
  ".venv", "venv", ".env", "env", "node_modules", ".git", "__pycache__",
  "*.pyc", "dist", "build", ".vscode", ".idea", "target", "bin", "obj",
]
```

Override via CLI (highest priority):

```
# Override only for this run
augmcp --base-url "https://d5.api.augmentcode.com/" --token "<TOKEN>" --transport stdio

# Persist overrides into settings.toml
augmcp --persist-config --base-url "https://d5.api.augmentcode.com/" --token "<TOKEN>"
```

Notes:
- Windows: use forward slashes (`/`) in paths (e.g., `C:/Users/name/project`).
- Do not commit personal tokens. They live under `~/.augmcp`.

## Quick Start

1) Persist backend config once (writes `~/.augmcp/settings.toml`):

```
augmcp --persist-config \
  --base-url "https://d5.api.augmentcode.com/" \
  --token "<TOKEN>"
```

2) Run as HTTP server (provides MCP at `/mcp` and REST helpers):

```
augmcp --bind 127.0.0.1:8888
```

3) Index a project and bind an alias (optional but recommended for convenience):

```
curl -X POST http://127.0.0.1:8888/api/index \
  -H "Content-Type: application/json" \
  -d '{
    "project_root_path": "C:/Users/name/projects/myproj",
    "alias": "myproj"
  }'
```

4) Search by alias (auto‑index if missing; otherwise use existing cache):

```
curl -X POST http://127.0.0.1:8888/api/search \
  -H "Content-Type: application/json" \
  -d '{
    "alias": "myproj",
    "query": "axum router definition and http handlers"
  }'
```

Tip: one‑shot local check without HTTP/MCP:

```
augmcp --oneshot-path "C:/Users/name/projects/myproj" \
       --oneshot-query "find logging configuration"
```

## MCP Configuration

Stdio (recommended):

```
{
  "mcpServers": {
    "augmcp": {
      "command": "C:/path/to/augmcp.exe",
      "args": ["--transport", "stdio"]
    }
  }
}
```

HTTP (streamable) with axum:

```
{
  "mcpServers": {
    "augmcp": {
      "command": "C:/path/to/augmcp.exe",
      "args": ["--transport", "http", "--bind", "127.0.0.1:8888"]
    }
  }
}
```

### Claude Desktop config notes

- Prefer `stdio` transport for Claude Desktop (most compatible).
- Ensure `~/.augmcp/settings.toml` has valid `base_url` and `token` (or pass CLI args in the config’s `args`).
- Example (Windows path):

```
{
  "mcpServers": {
    "augmcp": {
      "command": "C:/Users/name/.cargo/bin/augmcp.exe",
      "args": ["--transport", "stdio"]
    }
  }
}
```

## Tools

### search_context
Parameters:
- `project_root_path?` (string): absolute path to project root (use `/` on Windows)
- `alias?` (string): previously bound alias (optional)
- `skip_index_if_indexed?` (bool, default `true`): skip indexing if local cache exists
- `query` (string)

Behavior:
- If indexed and `skip_index_if_indexed=true`, query directly; otherwise perform incremental indexing then query.

### index_project
Parameters:
- `project_root_path?` (string)
- `alias?` (string): bind alias to path if provided with path or resolve path from alias
- `force_full?` (bool, default `false`): ignore cache and rebuild

Returns: a short stats string (`total_blobs/new_blobs/existing_blobs`).

## REST API (optional)

HTTP endpoints (default transport):

- `POST /api/search`
  - Body: `{ "project_root_path"?: "...", "alias"?: "...", "query": "...", "skip_index_if_indexed"?: true }`
  - Behavior mirrors MCP tool: auto index if needed

- `POST /api/index`
  - Supports `{"async": true}` for background indexing (returns `accepted`)
  - Stop task: `POST /api/index/stop` (by path or alias)
  - Task query: `GET /api/tasks?project_root_path=...` or `?alias=...` (returns running, progress, eta_secs)

- `GET /healthz`
  - Liveness/health check (200 OK, JSON `{ status: "ok", version: "..." }`)
  - Body: `{ "project_root_path"?: "...", "alias"?: "...", "force_full"?: false }`
  - Returns stats string

## Data & Logging

- Config: `~/.augmcp/settings.toml`
- Indexed projects: `~/.augmcp/data/projects.json`
- Aliases: `~/.augmcp/aliases.json`
- Logs: `~/.augmcp/log/augmcp.log` (daily rolling)

Logs include: entry, file collection/splitting, incremental stats, uploads, index persistence, retrieval start/end.

## How It Works

1. Collect text files (respect `.gitignore` and `exclude_patterns`).
2. Read with multi‑encoding; split by max lines; compute `sha256(path+content)`.
3. Compare against `projects.json` to find new blobs; upload only new blobs to `{base_url}/batch-upload`.
4. Retrieve context via `{base_url}/agents/codebase-retrieval` with all blob names; return `formatted_retrieval`.

## References

- acemcp (original Python server): https://github.com/qy527145/acemcp
- rmcp crate (0.8.5): Rust SDK for MCP.

## Environment Variables

You can override settings via environment variables (lower priority than CLI `--base-url/--token`):

- `AUGMCP_BASE_URL`, `AUGMCP_TOKEN`
- `AUGMCP_BATCH_SIZE`, `AUGMCP_MAX_LINES_PER_BLOB`
- `AUGMCP_TEXT_EXTENSIONS` (comma-separated), `AUGMCP_EXCLUDE_PATTERNS` (comma-separated)
- Retrieval tuning: `AUGMCP_MAX_OUTPUT_LENGTH`, `AUGMCP_DISABLE_CODEBASE_RETRIEVAL` (true/false), `AUGMCP_ENABLE_COMMIT_RETRIEVAL` (true/false)

## Async Indexing & Cancel

- Start async indexing via `POST /api/index` with body `{ "async": true, ... }`.
- Query progress and ETA via `GET /api/tasks?project_root_path=...` or `?alias=...`.
- Stop a running task via `POST /api/index/stop` (by path or alias). Cancellation is responsive at chunk boundaries.

