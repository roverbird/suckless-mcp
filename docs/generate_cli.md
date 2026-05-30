```markdown
# Agent-Safe Skill Generator for suckless-mcp

Generate suckless-mcp compliant Python CLI tools and skill.toml manifests.

## Core Requirements

### Python CLI (`{name}.py`)
- **Shebang**: `#!/usr/bin/env python3`
- **Argument parsing**: `argparse` with `--flag` style only (no positional args)
- **Output**: Valid JSON to stdout only (no debug prints, no logging)
- **Exit codes**: `0` on success, `1` on error

### Safety Features (MANDATORY for agent safety)
```python
parser.add_argument("--limit", type=int, default=10, help="Max items to process")
parser.add_argument("--dry-run", action="store_true", help="Preview without side effects")
parser.add_argument("--scope", type=str, default="default", help="Operation scope")
parser.add_argument("--timeout", type=int, default=30, help="Max seconds (1-60)")
```

### Implementation Patterns

**Bounded loops** (no infinite iteration):
```python
for i, item in enumerate(items):
    if i >= args.limit:
        break
    # process item
```

**Dry-run guard** (no side effects):
```python
if args.dry_run:
    print(json.dumps({"dry_run": True, "would_do": actions}))
    return

# execute actual changes
```

**Timeout enforcement**:
```python
signal.signal(signal.SIGALRM, lambda *_: sys.exit(1))
signal.alarm(args.timeout)
```

### skill.toml Requirements
- **Required fields**: `name`, `description`, `runtime`
- **Per-input**: `type`, `flag`, `required`, `description`
- **Optional**: `default`, `enum`, `max_length`
- **Syntax**: Standard table `[inputs.field]` (NO multi-line inline)

## Example 1: URL Fetcher Tool

**User request:**
> Generate a tool that fetches URLs safely with bounded size limits.

**Generated Python (`fetch.py`):**
```python
#!/usr/bin/env python3
"""Safe URL fetcher with size and iteration limits"""

import argparse
import json
import signal
import sys
import urllib.request

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--url", required=True)
    parser.add_argument("--limit", type=int, default=10)
    parser.add_argument("--max-bytes", type=int, default=1048576)
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--scope", type=str, default="default")
    parser.add_argument("--timeout", type=int, default=30)
    args = parser.parse_args()

    signal.alarm(args.timeout)

    if args.dry_run:
        print(json.dumps({"url": args.url, "dry_run": True}))
        return

    try:
        with urllib.request.urlopen(args.url, timeout=args.timeout) as resp:
            data = resp.read(args.max_bytes)
            print(json.dumps({
                "url": args.url,
                "size": len(data),
                "status": resp.status
            }))
    except Exception as e:
        print(json.dumps({"error": str(e)}))
        sys.exit(1)

if __name__ == "__main__":
    main()
```

**Generated skill.toml:**
```toml
name = "fetch"
description = "Fetch URL content with size limits"

[runtime]
entrypoint = "fetch.py"
timeout_secs = 30

[inputs.url]
type = "string"
flag = "--url"
required = true
description = "URL to fetch"

[inputs.max_bytes]
type = "integer"
flag = "--max-bytes"
required = false
default = 1048576
description = "Maximum bytes to read"

[inputs.limit]
type = "integer"
flag = "--limit"
required = false
default = 10
description = "Max redirects to follow"
```

## Example 2: Refactor Existing Script

**User request:**
> Refactor this script for suckless-mcp.

**Original script:**
```python
# old.py - has positional args, prints text
import sys
city = sys.argv[1]
print(f"Weather in {city}: sunny")
```

**Generated refactored Python (`weather.py`):**
```python
#!/usr/bin/env python3
"""Weather tool with safe defaults"""

import argparse
import json
import signal
import sys

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--city", required=True)
    parser.add_argument("--limit", type=int, default=10)
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--scope", type=str, default="default")
    parser.add_argument("--timeout", type=int, default=30)
    args = parser.parse_args()

    signal.alarm(args.timeout)

    if args.dry_run:
        print(json.dumps({"city": args.city, "dry_run": True}))
        return

    # Your logic here
    result = {"city": args.city, "forecast": "sunny", "temp": 22}
    print(json.dumps(result))

if __name__ == "__main__":
    main()
```

**Generated skill.toml:**
```toml
name = "weather"
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

## What NOT to Include

- ❌ Positional arguments (use `--flag` only)
- ❌ `print()` debug statements (use JSON output only)
- ❌ Unlimited loops (must have `--limit`)
- ❌ Side effects without `--dry-run` guard
- ❌ Multi-line inline TOML tables
- ❌ `version` field (not required)
- ❌ Complex validation (scripts validate themselves)

## Validation Checklist

Before deploying to `/opt/skills/<name>/`:

- [ ] Script has `#!/usr/bin/env python3` shebang
- [ ] All inputs use `--flag` style
- [ ] Output is valid JSON (`json.loads()` succeeds)
- [ ] `--limit` bounds all iterations
- [ ] `--dry-run` prevents all side effects
- [ ] skill.toml uses `[inputs.field]` syntax
- [ ] `max_output_bytes` in runtime (default 32768)
- [ ] Script executable: `chmod +x script.py`

## Quick Generate Command

Prompt any LLM:

```
Generate suckless-mcp compliant Python CLI and skill.toml for:

[Describe your tool]

Rules:
- Use --flags, no positional args
- Output JSON only
- Include --limit, --dry-run, --scope, --timeout
- skill.toml uses [inputs.field] table syntax
```
