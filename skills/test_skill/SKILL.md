# SKILL: test_skill

Text processing tool. No files. No side effects. Pure text operations.

## When to use

Use this tool when you need to:
- Count words/characters in text
- Transform text (upper/lower/reverse)
- Extract emails, URLs, hashtags from text
- Compare two texts

Do NOT use for: file operations, network calls, or persistent storage.

## Command

```bash
python3 -u skills/test-skill/test_skill_cli.py --action <op> --input "<text>" [options]
```

## Actions

| Action | What it does | Requires |
|--------|-------------|----------|
| `count` | Count chars, words, lines, sentences | `--input` |
| `transform` | Change text case or reverse it | `--input` + `--transform` |
| `extract` | Find emails, URLs, hashtags, mentions | `--input` |
| `compare` | Show similarity between two texts | `--input` + `--second` |
| `echo` | Return input as-is (bounded) | `--input` |

## Options

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `--input` | string | required | Text to process |
| `--second` | string | "" | Second text for compare |
| `--limit` | integer | 100 | Max chars/items to return (1-1000) |
| `--transform` | string | "upper" | upper/lower/reverse/trim/slugify |
| `--scope` | string | "default" | Isolate different users/contexts |
| `--dry-run` | flag | false | Preview without processing |
| `--verbose` | flag | false | Include debug info |

## Examples

**Count words:**
```bash
python3 -u skills/test-skill/test_skill_cli.py --action count --input "Hello world"
```

**Transform to uppercase:**
```bash
python3 -u skills/test-skill/test_skill_cli.py --action transform --input "hello" --transform upper
```

**Extract emails and URLs:**
```bash
python3 -u skills/test-skill/test_skill_cli.py --action extract --input "Email me@example.com"
```

**Compare two texts:**
```bash
python3 -u skills/test-skill/test_skill_cli.py --action compare --input "hello world" --second "hello there"
```

## Output Format

Always returns JSON:

```json
{
  "ok": true,
  "data": { ... },
  "message": "action completed"
}
```

Check `ok` field. If `false`, check `error` and `hint`.

