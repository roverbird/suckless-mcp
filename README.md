# suckless-mcp

**The suckless MCP gateway. Turn any CLI tool into a RUST MCP endpoint.**

One binary. Two configs. One dir of skills. No bloat self-hosted MCP host that runs on any VPS.

```bash
# Copy binary
cp suckless-mcp /usr/local/bin/

# Create config
cat > config.toml << EOF
listen_host = "127.0.0.1"
listen_port = 8080
skills_root = "/opt/skills"
max_concurrent_tools = 5
EOF

# Add a skill folder
mkdir -p /opt/skills/weather
cat > /opt/skills/weather/skill.toml << EOF
name = "weather"
version = "1.0.0"
description = "Get weather forecast"

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

# Run
./suckless-mcp --config config.toml
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
| MCP SDK requires rewriting tools | Keep your tools as-is |
| Need auth + rate limiting | Built into gateway |
| Each tool needs its own server | One gateway, many tools |
| Setup complexity | One binary, two configs |

## What you need

1. **suckless-mcp** binary (this)
2. **config.toml** - host, port, skills folder
3. **keys.toml** - API keys (auto-managed)
4. **Skills folder** - one subfolder per tool
5. **skill.toml** - describes your tool to AI
6. **Your CLI tool** - must use `--flags` and output JSON

## How a skill folder looks

```
/opt/skills/weather/
├── skill.toml      # Machine-readable manifest
├── weather.py      # Your CLI tool
└── SKILL.md        # Human/LLM instructions (optional)
```

## The contract: Your CLI tool MUST

1. **Use `--flags` only** - no positional args
2. **Output valid JSON** - nothing else to stdout
3. **Exit 0 on success, 1 on error**

That's it. Everything else (auth, rate limits, concurrency) is handled by suckless-mcp.

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

**skill.toml:**
```toml
name = "weather"
version = "1.0.0"
description = "Get weather forecast for a city"

[runtime]
entrypoint = "weather.py"
timeout_secs = 30

[inputs.city]
type = "string"
flag = "--city"
required = true
description = "City name"
max_length = 100
```

**Deploy:**
```bash
cp weather.py /opt/skills/weather/
suckless-mcp --config config.toml serve
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
```

## Safety note

suckless-mcp **does not** make unsafe tools safe. It only reads `skill.toml` to explain the tool to AI agents.

If your CLI tool deletes files, the AI can still delete files. The gateway adds:
- Authentication (API keys)
- Rate limiting
- Timeouts
- Concurrent execution limits

But the tool itself must be trusted.

## Deployment with Caddy

```Caddyfile
mcp.yourdomain.com {
    reverse_proxy localhost:8080
}
```

Then AI agents connect to `https://mcp.yourdomain.com/mcp`

## Commands

```bash
suckless-mcp serve                    # Start gateway
suckless-mcp skills list              # List all skills
suckless-mcp skills validate          # Check skill.toml files
suckless-mcp keys add <id> <key>      # Add API key
suckless-mcp status                   # Show server state
```

## Configuration

**config.toml**
```toml
listen_host = "127.0.0.1"
listen_port = 8080
skills_root = "/opt/skills"
max_concurrent_tools = 5
rate_limit_per_minute = 60
```

**keys.toml** (auto-generated via CLI)
```toml
[[keys]]
id = "admin"
key = "your-secret-key"
active = true
```

## Philosophy

suckless-mcp does one thing: **expose CLI tools as MCP endpoints**.

- No web dashboard
- No admin API
- No plugin system
- No database
- No hot reload
- No complexity

Just a binary, config, and a folder of skills.

## License

MIT

---

For full deploy and hosting of your AI-ready infrastructure, please [contact](mailto:kibervarnost@proton.me)

**suckless-mcp** — only your tools matter
