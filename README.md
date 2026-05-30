# suckless-mcp

**The suckless MCP gateway. Turn any CLI tool into an MCP endpoint.**

One binary. One config file. One key file. One dir of skills. Same `--flags` everywhere.


[![Crates.io](https://img.shields.io/crates/v/suckless-mcp.svg)](https://crates.io/crates/suckless-mcp)
[![MIT license](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

## Install

### Via cargo (for Rust developers)
```bash
cargo install suckless-mcp
```

### Quick Install (Linux x86_64)

```bash
# Install with one command
curl -fsSL https://raw.githubusercontent.com/roverbird/suckless-mcp/main/install.sh -o install.sh
chmod +x install.sh
./install.sh

# Check status
suckless-mcp --status
```

The installer does everything:
- Downloads the pre-compiled binary from GitHub Releases
- Copies skills from the repository to `/opt/skills`
- Creates `/etc/suckless-mcp/config.toml` with defaults
- Generates an API key in `/etc/suckless-mcp/keys.toml`
- Creates a `suckless` system user
- Installs a systemd service (auto-start on boot)

### Manual Installation

```bash
# Copy binary
cp suckless-mcp /usr/local/bin/

# Create config
mkdir -p /etc/suckless-mcp
cat > /etc/suckless-mcp/config.toml << EOF
listen_host = "127.0.0.1"
listen_port = 8080
max_concurrent_tools = 5
EOF

# Add a skill
mkdir -p /opt/skills/weather
cat > /opt/skills/weather/skill.toml << EOF
name = "weather"
description = "Get weather forecast for a city"
public = false   # true = no auth required

[runtime]
entrypoint = "weather.py"
timeout_secs = 30

[inputs.city]
type = "string"
flag = "--city"
required = true
description = "City name"
EOF

# Add your CLI tool
cp weather.py /opt/skills/weather/

# Add an API key
suckless-mcp --keys-add --id admin --key "your-secret-key"

# Run
suckless-mcp --serve
```

## Uninstall

```bash
./install.sh --uninstall
```

## What is this?

suckless-mcp is a **gateway** that exposes your existing CLI tools as MCP endpoints.

- AI agents connect to suckless-mcp over HTTP
- suckless-mcp reads `skill.toml` to understand your tool
- suckless-mcp calls your CLI tool with `--flags`
- Your tool outputs JSON back to the AI

```
AI Agent → Caddy → suckless-mcp → your CLI tool (--flags)
                ← suckless-mcp ← JSON output
```

## Why?

| Problem | Solution |
|---------|----------|
| MCP SDK requires rewriting tools | Keep your tools, add skill.toml |
| Need authentication | Built into gateway |
| Each tool needs its own server | One gateway, many tools |
| Setup complexity | One binary, two configs |

## What you need

1. **suckless-mcp** binary (this)
2. **config.toml** - host, port, concurrency limits
3. **keys.toml** - API keys (managed via CLI)
4. **/opt/skills/** - one subfolder per tool
5. **skill.toml** - describes your tool to AI
6. **Your CLI tool** - must use `--flags` and output JSON

## Authentication: Per-tool, not all-or-nothing

suckless-mcp supports **mixed auth** on a single endpoint:

| Tool type | `public` flag | `tools/list` visibility | Can be called without auth |
|-----------|---------------|------------------------|---------------------------|
| Public | `true` | Visible to everyone | ✓ Yes |
| Private | `false` (default) | Only visible to authenticated clients | ✗ No (returns 401) |

```toml
# Public tool example (weather data, public API)
name = "weather"
public = true

# Private tool example (database query)
name = "db_query"
public = false   # or omit the field entirely
```

This gives you:
- **One endpoint** (`/mcp`) for both public and private tools
- **Clean discovery** - agents only see tools they can actually call
- **No complex OAuth** - simple Bearer tokens for private tools

## How a skill folder looks

```
/opt/skills/weather/
├── skill.toml      # Machine-readable manifest
└── weather.py      # Your CLI tool
```

## The contract: Your CLI tool MUST

1. **Use `--flags` only** - no positional args, no shell string concatenation
2. **Output valid JSON** - nothing else to stdout
3. **Exit 0 on success, 1 on error**

Everything else (auth, concurrency, timeouts) is handled by suckless-mcp.

## The gateway follows the same rules

suckless-mcp itself uses `--flags` — no positional subcommands, no hidden state.

```bash
# All gateway commands are flags
suckless-mcp --help
suckless-mcp --status
suckless-mcp --skills
suckless-mcp --skills --name weather
suckless-mcp --keys-list
suckless-mcp --keys-add --id admin --key secret
suckless-mcp --keys-revoke --id admin
suckless-mcp --serve
```

Same pattern as your skills. LLMs learn once, apply everywhere.

## Example: Turn any Python script into an MCP tool

**Before (not compliant):**
```python
# old.py
import sys
city = sys.argv[1]  # positional arg
print(f"Weather in {city}: sunny")  # not JSON
```

**After (compliant):**
```python
#!/usr/bin/env python3
import argparse, json

parser = argparse.ArgumentParser()
parser.add_argument("--city", required=True)
args = parser.parse_args()

result = {"city": args.city, "forecast": "sunny", "temp": 22}
print(json.dumps(result))
```

**skill.toml (public tool):**
```toml
name = "weather"
description = "Get weather forecast for a city"
public = true

[runtime]
entrypoint = "weather.py"
timeout_secs = 30

[inputs.city]
type = "string"
flag = "--city"
required = true
description = "City name"
```

**Deploy:**
```bash
cp weather.py /opt/skills/weather/
suckless-mcp --serve
```

## Generate skill.toml for any CLI tool

Prompt any LLM:

```
Generate skill.toml for this CLI tool:

{paste your script}

Rules:
- One [inputs] section per flag
- Include: type, flag, required, description
- Use standard table syntax (no multi-line inline)
- Add public = true if this tool is safe for unauthenticated access
```

## Safety note

suckless-mcp **does not** make unsafe tools safe. It only reads `skill.toml` to explain the tool to AI agents.

If your CLI tool deletes files, the AI can still delete files. The gateway adds:
- Authentication (API keys for private tools)
- Timeouts
- Concurrent execution limits

But the tool itself must be trusted.

## Deployment with Caddy

```Caddyfile
mcp.yourdomain.com {
    # Rate limiting (recommended)
    rate_limit {
        zone dynamic {
            key {remote_host}
            events 30
            window 1m
        }
    }
    
    # Security headers
    header {
        Strict-Transport-Security "max-age=31536000; includeSubDomains; preload"
        X-Content-Type-Options "nosniff"
        X-Frame-Options "DENY"
        -Server
    }
    
    reverse_proxy localhost:8080
}
```

Then AI agents connect to `https://mcp.yourdomain.com/mcp`

## Commands

All commands use `--flags`. Output is always JSON. Exit 0 = success, 1 = error.

| Action | Command |
|--------|---------|
| Start gateway | `suckless-mcp --serve` |
| Show server state | `suckless-mcp --status` |
| List all skills | `suckless-mcp --skills` |
| Show one skill | `suckless-mcp --skills --name weather` |
| List key IDs | `suckless-mcp --keys-list` |
| Add a key | `suckless-mcp --keys-add --id admin --key secret` |
| Revoke a key | `suckless-mcp --keys-revoke --id admin` |
| Show help | `suckless-mcp --help` |

## Configuration

**config.toml** (`/etc/suckless-mcp/config.toml` by default)
```toml
listen_host = "127.0.0.1"
listen_port = 8080
max_concurrent_tools = 5
```

**keys.toml** (managed via CLI, never edit by hand)
```toml
[[keys]]
id = "admin"
key = "your-secret-key"
active = true
```

**Skills root** is hardcoded to `/opt/skills`. Each skill is a subdirectory containing `skill.toml` and your executable script.

## Philosophy

suckless-mcp does one thing: **expose CLI tools as MCP endpoints**.

- No web dashboard
- No admin API
- No plugin system
- No database
- No hot reload
- No rate limiting (delegate to Caddy)
- No transport negotiation (POST /mcp only)
- No positional subcommands (gateway uses `--flags` just like skills)
- No mandatory auth on all tools (per-tool `public` flag gives flexibility)

Just a binary, one config file, one key file, and `/opt/skills/`.

## License

MIT

---

For deployment support, contact: kibervarnost@proton.me

**suckless-mcp** — only your tools matter
```
