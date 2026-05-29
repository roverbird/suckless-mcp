#!/usr/bin/env python3

"""
Agent-safe text processor for suckless-mcp.

No filesystem access. Pure memory operations.
Safety principles: bounded, scoped, interruptible, predictable.
"""

import argparse
import json
import signal
import sys
import re
from typing import Any, Dict

# --- Safety Constants ---
VALID_ACTIONS = {"echo", "count", "transform", "extract", "compare"}
VALID_TRANSFORMS = {"upper", "lower", "reverse", "trim", "slugify"}

class TimeoutError(Exception):
    """Raised when operation exceeds time limit."""
    pass

def timeout_handler(signum, frame):
    """Handle SIGALRM for interruptible operations."""
    raise TimeoutError("Operation exceeded time limit")

def parse_args() -> argparse.Namespace:
    """Parse CLI args."""
    parser = argparse.ArgumentParser()
    parser.add_argument("--action", required=True)
    parser.add_argument("--input", required=True)
    parser.add_argument("--second", default="")
    parser.add_argument("--limit", type=int, default=100)
    parser.add_argument("--transform", default="upper")
    parser.add_argument("--scope", default="default")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--verbose", action="store_true")
    parser.add_argument("--timeout", type=int, default=30)
    return parser.parse_args()

def validate_args(args: argparse.Namespace) -> Dict[str, Any]:
    """Validate inputs against safety bounds."""
    errors = []
    if args.action not in VALID_ACTIONS:
        errors.append(f"--action must be one of {VALID_ACTIONS}")
    if args.transform not in VALID_TRANSFORMS:
        errors.append(f"--transform must be one of {VALID_TRANSFORMS}")
    if not (1 <= args.limit <= 1000):
        errors.append("--limit must be between 1 and 1000")
    if not (1 <= args.timeout <= 60):
        errors.append("--timeout must be between 1 and 60")
    if len(args.input) > 5000:
        errors.append("--input exceeds 5000 chars")
    if args.second and len(args.second) > 5000:
        errors.append("--second exceeds 5000 chars")
    if not re.match(r'^[a-z0-9_\-]+$', args.scope):
        errors.append("--scope must contain only alphanumeric, dash, underscore")
    if args.action == "compare" and not args.second and not args.dry_run:
        errors.append("--action compare requires --second")
    
    return {"ok": not errors, "error": " | ".join(errors)}

# --- Action Implementations ---

def action_echo(args: argparse.Namespace) -> Dict:
    if args.dry_run: return {"action": "echo", "dry_run": True}
    res = args.input[:args.limit]
    return {"output": res, "truncated": len(res) < len(args.input)}

def action_count(args: argparse.Namespace) -> Dict:
    if args.dry_run: return {"action": "count", "dry_run": True}
    words = args.input.split()
    return {
        "chars": len(args.input),
        "words": len(words),
        "lines": len(args.input.splitlines())
    }

def action_transform(args: argparse.Namespace) -> Dict:
    if args.dry_run: return {"action": "transform", "transform": args.transform, "dry_run": True}
    text = args.input
    if args.transform == "upper": res = text.upper()
    elif args.transform == "lower": res = text.lower()
    elif args.transform == "reverse": res = text[::-1]
    elif args.transform == "trim": res = " ".join(text.split())
    elif args.transform == "slugify": res = re.sub(r'[^a-z0-9]+', '-', text.lower()).strip('-')
    else: res = text
    res = res[:args.limit]
    return {"result": res}

def action_extract(args: argparse.Namespace) -> Dict:
    if args.dry_run: return {"action": "extract", "dry_run": True}
    return {
        "words": re.findall(r'\b[a-zA-Z]+\b', args.input)[:args.limit],
        "emails": re.findall(r'[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}', args.input)[:args.limit]
    }

def action_compare(args: argparse.Namespace) -> Dict:
    if args.dry_run: return {"action": "compare", "dry_run": True}
    set1, set2 = set(args.input.split()), set(args.second.split())
    return {"similarity": round(len(set1 & set2) / max(len(set1 | set2), 1), 3)}

# --- Execution ---

def main():
    args = parse_args()
    signal.signal(signal.SIGALRM, timeout_handler)
    signal.alarm(args.timeout)
    
    try:
        val = validate_args(args)
        if not val["ok"]:
            print(json.dumps({"ok": False, "error": val["error"]}))
            sys.exit(1)
        
        funcs = {
            "echo": action_echo, "count": action_count, "transform": action_transform,
            "extract": action_extract, "compare": action_compare
        }
        
        result = funcs[args.action](args)
        print(json.dumps({"ok": True, "data": result, "action": args.action}, indent=2))
        
    except TimeoutError:
        print(json.dumps({"ok": False, "error": "Timeout"}))
        sys.exit(1)
    except Exception as e:
        print(json.dumps({"ok": False, "error": str(e)}))
        sys.exit(1)
    finally:
        signal.alarm(0)

if __name__ == "__main__":
    main()
