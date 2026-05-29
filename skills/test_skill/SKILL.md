# SKILL: test_skill

Text processing tool. No files. No side effects. Pure text operations.

## When to use

Use this tool when you need to:

* Count words/characters in text
* Transform text (upper, lower, reverse, trim, slugify)
* Extract entities (emails, URLs, hashtags, mentions)
* Compare the similarity between two texts

## Command

```bash
python3 -u /opt/skills/test_skill/test_skill.py --action <op> --input "<text>" [options]

```

## Actions

| Action | What it does | Required Args |
| --- | --- | --- |
| `count` | Count chars, words, and lines | `--input` |
| `transform` | Apply casing/string transformations | `--input` |
| `extract` | Find words, numbers, and emails | `--input` |
| `compare` | Show similarity (Jaccard) between texts | `--input`, `--second` |
| `echo` | Return input (subject to `--limit`) | `--input` |

## Options

| Option | Type | Default | Description |
| --- | --- | --- | --- |
| `--action` | string | required | Operation to perform |
| `--input` | string | required | Primary text to process |
| `--second` | string | "" | Second text (required for `compare`) |
| `--limit` | integer | 100 | Max items/chars to return (1-1000) |
| `--transform` | string | "upper" | upper/lower/reverse/trim/slugify |
| `--scope` | string | "default" | Namespace for caching |
| `--timeout` | integer | 30 | Execution timeout in seconds (1-60) |
| `--dry-run` | flag | false | Preview without full processing |
| `--verbose` | flag | false | Include debug metadata |

## Examples

**Count words:**

```bash
python3 -u /opt/skills/test_skill/test_skill.py --action count --input "Hello world"

```

**Transform to uppercase:**

```bash
python3 -u /opt/skills/test_skill/test_skill.py --action transform --input "hello" --transform upper

```

**Extract entities:**

```bash
python3 -u /opt/skills/test_skill/test_skill.py --action extract --input "Contact me@example.com"

```

**Compare two texts:**

```bash
python3 -u /opt/skills/test_skill/test_skill.py --action compare --input "hello world" --second "hello there"

```

## Output Format

Always returns JSON:

```json
{
  "ok": true,
  "action": "...",
  "data": { ... }
}

```

If `ok` is `false`, check the `error` field for details.
