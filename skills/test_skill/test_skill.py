#!/usr/bin/env python3

"""
Agent-safe text processor for suckless-mcp.

No filesystem access. Pure memory operations.
Safety principles: bounded, scoped, interruptible, predictable, composable.
"""

import argparse
import json
import signal
import sys
import time
from typing import Any, Dict, List, Optional


class TimeoutError(Exception):
    """Raised when operation exceeds time limit."""
    pass


def timeout_handler(signum, frame):
    """Handle SIGALRM for interruptible operations."""
    raise TimeoutError("Operation exceeded time limit")


def parse_args() -> argparse.Namespace:
    """Parse CLI args with validation."""
    parser = argparse.ArgumentParser(
        description="Agent-safe text processor - no filesystem access",
        epilog="All operations are pure functions with bounded output."
    )
    
    parser.add_argument("--action", type=str, required=True,
                        choices=["echo", "count", "transform", "extract", "compare"],
                        help="Text operation to perform")
    parser.add_argument("--input", type=str, required=True,
                        help="Input text to process")
    parser.add_argument("--second", type=str, default="",
                        help="Second input for compare action")
    parser.add_argument("--limit", type=int, default=100,
                        help="Max chars/words/lines to return (1-1000)")
    parser.add_argument("--transform", type=str, default="upper",
                        choices=["upper", "lower", "reverse", "trim", "slugify"],
                        help="Transformation to apply")
    parser.add_argument("--scope", type=str, default="default",
                        help="Operation scope (for cache isolation)")
    parser.add_argument("--dry-run", action="store_true",
                        help="Preview without processing")
    parser.add_argument("--verbose", action="store_true",
                        help="Include debug info")
    parser.add_argument("--timeout", type=int, default=30,
                        help="Max execution time (1-60 seconds)")
    
    return parser.parse_args()


def validate_args(args: argparse.Namespace) -> Dict[str, Any]:
    """Validate all inputs against safety bounds."""
    errors = []
    
    # Bounded checks
    if args.limit < 1 or args.limit > 1000:
        errors.append("--limit must be between 1 and 1000")
    
    if args.timeout < 1 or args.timeout > 60:
        errors.append("--timeout must be between 1 and 60")
    
    if len(args.input) > 5000:
        errors.append("--input exceeds 5000 chars")
    
    if args.second and len(args.second) > 5000:
        errors.append("--second exceeds 5000 chars")
    
    # Scope validation
    import re
    if not re.match(r'^[a-z0-9_\-]+$', args.scope):
        errors.append("--scope must contain only alphanumeric, dash, underscore")
    
    # Compare action needs second input
    if args.action == "compare" and not args.second and not args.dry_run:
        errors.append("--action compare requires --second (or use --dry-run)")
    
    if errors:
        return {"ok": False, "error": " | ".join(errors), "hint": "Check argument bounds"}
    
    return {"ok": True}


def action_echo(input_text: str, limit: int, dry_run: bool, verbose: bool) -> Dict:
    """Echo input back with bounds."""
    
    if dry_run:
        return {
            "action": "echo",
            "dry_run": True,
            "would_echo": f"{len(input_text)} chars",
            "sample": input_text[:50] + "..." if len(input_text) > 50 else input_text
        }
    
    # Bounded output
    result = input_text[:limit]
    
    return {
        "action": "echo",
        "original_length": len(input_text),
        "output": result,
        "output_length": len(result),
        "truncated": len(result) < len(input_text)
    }


def action_count(input_text: str, limit: int, dry_run: bool, verbose: bool) -> Dict:
    """Count characters, words, lines, sentences."""
    
    if dry_run:
        return {
            "action": "count",
            "dry_run": True,
            "would_analyze": f"{len(input_text)} chars"
        }
    
    lines = input_text.splitlines()
    words = input_text.split()
    
    # Detect sentences (simple)
    sentences = input_text.replace('!', '.').replace('?', '.').split('.')
    sentences = [s.strip() for s in sentences if s.strip()]
    
    return {
        "action": "count",
        "chars": len(input_text),
        "words": len(words),
        "lines": len(lines),
        "sentences": len(sentences),
        "unique_words": len(set(words)),
        "avg_word_length": round(sum(len(w) for w in words) / max(len(words), 1), 2)
    }


def action_transform(input_text: str, transform: str, limit: int, dry_run: bool, verbose: bool) -> Dict:
    """Apply text transformation."""
    
    if dry_run:
        return {
            "action": "transform",
            "transform": transform,
            "dry_run": True,
            "would_transform": f"{len(input_text)} chars"
        }
    
    # Apply transformation
    if transform == "upper":
        result = input_text.upper()
    elif transform == "lower":
        result = input_text.lower()
    elif transform == "reverse":
        result = input_text[::-1]
    elif transform == "trim":
        result = " ".join(input_text.split())
    elif transform == "slugify":
        import re
        result = re.sub(r'[^a-z0-9]+', '-', input_text.lower().strip())
        result = result.strip('-')
    else:
        result = input_text
    
    # Bounded output
    result = result[:limit]
    
    return {
        "action": "transform",
        "transform": transform,
        "original_length": len(input_text),
        "result": result,
        "result_length": len(result),
        "truncated": len(result) < len(input_text) if transform not in ["trim", "slugify"] else False
    }


def action_extract(input_text: str, limit: int, dry_run: bool, verbose: bool) -> Dict:
    """Extract words, numbers, emails, URLs."""
    
    import re
    
    if dry_run:
        return {
            "action": "extract",
            "dry_run": True,
            "would_extract_from": f"{len(input_text)} chars"
        }
    
    # Extract patterns (bounded)
    words = re.findall(r'\b[a-zA-Z]+\b', input_text)[:limit]
    numbers = re.findall(r'\b\d+\b', input_text)[:limit]
    emails = re.findall(r'[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}', input_text)[:limit]
    urls = re.findall(r'https?://[^\s]+', input_text)[:limit]
    hashtags = re.findall(r'#[a-zA-Z0-9_]+', input_text)[:limit]
    mentions = re.findall(r'@[a-zA-Z0-9_]+', input_text)[:limit]
    
    return {
        "action": "extract",
        "words": words[:limit],
        "word_count": len(words),
        "numbers": numbers[:limit],
        "email_count": len(emails),
        "url_count": len(urls),
        "hashtags": hashtags[:limit],
        "mentions": mentions[:limit]
    }


def action_compare(first: str, second: str, limit: int, dry_run: bool, verbose: bool) -> Dict:
    """Compare two texts."""
    
    if dry_run:
        return {
            "action": "compare",
            "dry_run": True,
            "would_compare": f"{len(first)} vs {len(second)} chars"
        }
    
    # Simple comparison
    first_words = set(first.split())
    second_words = set(second.split())
    
    common = first_words & second_words
    only_first = first_words - second_words
    only_second = second_words - first_words
    
    # Similarity (Jaccard)
    similarity = len(common) / max(len(first_words | second_words), 1)
    
    # Length difference
    len_diff = abs(len(first) - len(second))
    
    return {
        "action": "compare",
        "similarity": round(similarity, 3),
        "common_words": len(common),
        "only_in_first": len(only_first),
        "only_in_second": len(only_second),
        "common_sample": list(common)[:min(10, limit)],
        "length_diff": len_diff,
        "first_length": len(first),
        "second_length": len(second)
    }


def main() -> None:
    """Main entry point with safety guarantees."""
    
    args = parse_args()
    
    # Setup timeout (interruptible)
    signal.signal(signal.SIGALRM, timeout_handler)
    signal.alarm(args.timeout)
    
    try:
        # Validate first
        validation = validate_args(args)
        if not validation["ok"]:
            print(json.dumps(validation))
            sys.exit(1)
        
        # Dispatch action
        action_map = {
            "echo": action_echo,
            "count": action_count,
            "transform": action_transform,
            "extract": action_extract,
            "compare": action_compare,
        }
        
        func = action_map[args.action]
        
        # Execute with safety boundary
        result = func(
            input_text=args.input,
            second=args.second,
            limit=args.limit,
            transform=args.transform,
            dry_run=args.dry_run,
            verbose=args.verbose
        )
        
        # Composable output
        output = {
            "ok": True,
            "data": result,
            "metadata": {
                "execution_time_ms": 0,
                "scope": args.scope,
                "dry_run": args.dry_run,
                "action": args.action
            },
            "message": f"{args.action} completed"
        }
        
        if args.verbose:
            output["metadata"]["input_length"] = len(args.input)
            if args.second:
                output["metadata"]["second_length"] = len(args.second)
        
        # Predictable JSON output
        print(json.dumps(output, indent=2))
        sys.exit(0)
        
    except TimeoutError:
        error_response = {
            "ok": False,
            "error": f"Timeout after {args.timeout}s",
            "hint": "Increase --timeout or reduce input size"
        }
        print(json.dumps(error_response))
        sys.exit(1)
        
    except Exception as e:
        error_response = {
            "ok": False,
            "error": str(e),
            "hint": "Check inputs and try --dry-run first"
        }
        print(json.dumps(error_response))
        sys.exit(1)
    
    finally:
        signal.alarm(0)


if __name__ == "__main__":
    main()
