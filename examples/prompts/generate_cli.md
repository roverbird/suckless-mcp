Generate a compliant agent-safe Python CLI tool and matching skill.toml for suckless-mcp.

## Requirements

### Python CLI (`{name}.py`)
- Use `argparse` with `--flag` style (no positional args)
- Output ONLY valid JSON to stdout (no debug prints)
- Implement these safety principles:
  - `--limit` flag (default 10-100) - bound all iterations
  - `--dry-run` flag (default false) - preview without side effects
  - `--scope` flag (default "default") - isolate operations
  - `--timeout` flag (default 30, max 60) - interruptible
- Exit code 0 on success, 1 on error
- Include shebang: `#!/usr/bin/env python3`

### skill.toml
- One input per CLI flag
- Required fields: `name`, `version`, `description`, `runtime`, `inputs.*.type`, `inputs.*.flag`
- Use standard table syntax `[inputs.field]` (no multi-line inline tables)
- Include: `enum` for fixed values, `max_length` for strings, `default` for optional flags

## Example Output

```python
#!/usr/bin/env python3
"""Agent-safe tool for {description}"""

import argparse, json, signal, sys

# ... implementation with bounded loops, dry-run, scope isolation
```

```toml
name = "{name}"
version = "1.0.0"
description = "{description}"

[runtime]
entrypoint = "{name}.py"
timeout_secs = 30
max_output_bytes = 32768

[inputs]
# One section per flag
{input_name} = { type = "string", flag = "--{flag}", required = true, description = "..." }
```

Now generate a tool that: {describe your specific use case}

---

## Example 1: Generate a URL Fetcher Tool

```
Generate a compliant agent-safe Python CLI tool and matching skill.toml for suckless-mcp.

Now generate a tool that fetches URLs safely with bounded size limits.

```
---

## Example 2: Refactor existing py script to Agent-Safe CLI and write skill.toml for it


```
Refactor this Python script into a suckless-mcp compliant agent-safe CLI tool.

## Requirements for the Python script:
- Use --flags (no positional args)
- Output JSON only
- Add --limit (default 10), --dry-run, --scope (default "default"), --timeout (default 30)
- Bounded loops, no side effects without --dry-run

## Requirements for skill.toml:
- One [inputs] section per flag
- Include: type, flag, required, description
- Use standard table syntax (no multi-line inline tables)

## Original script:
```python
{PASTE YOUR SCRIPT HERE}
```

Generate: (1) refactored Python script (2) skill.toml

