// ============================================================================
//  suckless-mcp  v0.1.0
//  Suckless MCP remote gateway — Rust + Axum + Tokio
// ============================================================================
//
//  WHAT IT IS:
//    A minimal, secured MCP server that turns CLI scripts into
//    MCP tools behind a Caddy reverse proxy. Skills are declared in
//    skill.toml files.
//
//  WHAT IT IS NOT:
//    A mesh router, plugin system, admin API, or framework. One binary,
//    two config files, one folder of skills. Nothing more.
//
//  ENDPOINTS:
//    POST /mcp        — MCP JSON-RPC 2.0 (Streamable HTTP, 2025-11-25 spec)
//    GET  /mcp        — SSE transport (backward compat with old aggregators)
//    GET  /health     — liveness probe, no auth
//
//  CLI SUBCOMMANDS (agentic-ready, all output JSON to stdout):
//    serve            — start the gateway (default)
//    status           — report loaded state as JSON
//    skills list      — list all discovered skills as JSON
//    skills validate  — run preflight on all skills, report as JSON
//    skills check     — validate one skill by name
//    keys list        — list key ids (never raw values)
//    keys add         — append a key to keys.toml
//    keys revoke      — mark a key inactive
//    schema skill     — emit skill.toml JSON Schema
//    schema config    — emit config.toml JSON Schema
//
//  FILES:
//    /etc/suckless-mcp/config.toml   — server settings
//    /etc/suckless-mcp/keys.toml     — api keys
//    /opt/skills/<name>/skill.toml    — one per skill, auto-discovered
// ============================================================================

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use dashmap::DashMap;
use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::{net::TcpListener, process::Command, sync::Semaphore, time::timeout};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{error, info, warn};

// ============================================================================
// CONFIG  (/etc/suckless-mcp/config.toml)
// ============================================================================

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct Config {
    listen_host:           String,
    listen_port:           u16,
    skills_root:           String,
    max_concurrent_tools:  usize,
    #[serde(default = "default_rate_limit")]
    rate_limit_per_minute: u32,
}

fn default_rate_limit() -> u32 { 60 }

fn load_config(path: &str) -> Result<Config, String> {
    let raw = fs::read_to_string(path)
        .map_err(|e| format!("cannot read {path}: {e}"))?;
    toml::from_str(&raw)
        .map_err(|e| format!("bad config.toml: {e}"))
}

// ============================================================================
// KEYS  (/etc/suckless-mcp/keys.toml)
// ============================================================================

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
struct KeysFile {
    #[serde(default)]
    keys: Vec<ApiKey>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
struct ApiKey {
    id:     String,
    key:    String,
    #[serde(default = "bool_true")]
    active: bool,
    /// RFC3339 UTC expiry. None = never expires.
    #[serde(default)]
    expires_at:    Option<String>,
    /// Tool name allowlist. None = all tools. Empty vec = no tools.
    /// Use "*" as a single entry to explicitly grant all tools.
    #[serde(default)]
    allowed_tools: Option<Vec<String>>,
}

impl ApiKey {
    /// Returns true if the key is currently valid (active and not expired).
    fn is_valid(&self) -> bool {
        if !self.active { return false; }
        if let Some(ref exp) = self.expires_at {
            match exp.parse::<DateTime<Utc>>() {
                Ok(dt) => return Utc::now() < dt,
                Err(_) => return false,   // unparseable expiry = treat as expired
            }
        }
        true
    }

    /// Returns true if this key may call the named tool.
    fn allows_tool(&self, tool: &str) -> bool {
        match &self.allowed_tools {
            None       => true,                          // no allowlist = all tools
            Some(list) => list.iter().any(|t| t == "*" || t == tool),
        }
    }
}

fn bool_true() -> bool { true }

fn load_keys(path: &str) -> Result<KeysFile, String> {
    let raw = fs::read_to_string(path)
        .map_err(|e| format!("cannot read {path}: {e}"))?;
    toml::from_str(&raw)
        .map_err(|e| format!("bad keys.toml: {e}"))
}

fn save_keys(path: &str, kf: &KeysFile) -> Result<(), String> {
    let raw = toml::to_string_pretty(kf)
        .map_err(|e| format!("cannot serialize keys: {e}"))?;
    fs::write(path, raw)
        .map_err(|e| format!("cannot write {path}: {e}"))
}

// ============================================================================
// SKILL MANIFEST  (skill.toml)
// ============================================================================
//
// deny_unknown_fields on every struct means a typo like "entripoint"
// produces a clear parse error naming the bad field, instead of silently
// loading a broken skill.

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct SkillToml {
    name:        String,
    version:     String,
    description: String,
    runtime:     Runtime,
    #[serde(default)]
    inputs:      HashMap<String, InputSpec>,
    #[serde(default)]
    permissions: Permissions,
    /// public = true: tools/list visibility and tools/call bypass auth.
    /// Use for informational or low-sensitivity tools only.
    #[serde(default)]
    public:      bool,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct Runtime {
    entrypoint:                    String,
    timeout_secs:                  u64,
    #[serde(default = "default_max_output_bytes")]
    max_output_bytes:              usize,
}

fn default_max_output_bytes() -> usize { 32_768 }

/// One parameter in [inputs].
/// Maps to one MCP inputSchema property and one --flag value pair in subprocess argv.
/// All inputs must declare a flag. Boolean flags emit --flag only (no value) when true.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct InputSpec {
    #[serde(rename = "type")]
    kind:        InputType,
    /// CLI flag passed to the script, e.g. "--city". Required.
    flag:        String,
    /// Description shown to the LLM in the tool's inputSchema.
    #[serde(default)]
    description: String,
    #[serde(default)]
    required:    bool,
    /// Allowlist of valid string values.
    #[serde(default, rename = "enum")]
    enum_values: Vec<String>,
    #[serde(default)]
    default:     Option<Value>,
    /// Optional regex pattern for string validation (compiled at startup).
    #[serde(default)]
    pattern:     Option<String>,
    /// Max byte length for string inputs (default: 512).
    #[serde(default = "default_max_length")]
    max_length:  usize,
}

fn default_max_length() -> usize { 512 }

impl InputSpec {
    fn to_json_schema(&self) -> Value {
        let mut prop = json!({
            "type":        self.kind.as_str(),
            "description": self.description,
        });
        if !self.enum_values.is_empty() { prop["enum"]    = json!(self.enum_values); }
        if let Some(ref d) = self.default { prop["default"] = d.clone(); }
        if let Some(ref p) = self.pattern  { prop["pattern"] = json!(p); }
        if self.kind == InputType::String  { prop["maxLength"] = json!(self.max_length); }
        prop
    }
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
enum InputType {
    String,
    Integer,
    Boolean,
    Number,
}

impl InputType {
    fn as_str(&self) -> &'static str {
        match self {
            InputType::String  => "string",
            InputType::Integer => "integer",
            InputType::Boolean => "boolean",
            InputType::Number  => "number",
        }
    }
}

#[derive(Debug, Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
struct Permissions {
    #[serde(default)]
    secrets: Vec<String>,   // env var names injected into subprocess
    #[serde(default)]
    network: Vec<String>,   // declared, not enforced in v1
}

// ============================================================================
// INTERNAL SKILL RECORD
// ============================================================================

#[derive(Clone)]
struct Skill {
    toml:     SkillToml,
    script:   PathBuf,
    env:      Vec<(String, String)>,
    /// Pre-compiled regexes keyed by input name. Compiled once at startup.
    patterns: HashMap<String, Regex>,
}

impl Skill {
    fn input_schema(&self) -> Value {
        let mut properties = json!({});
        let mut required: Vec<&str> = Vec::new();

        for (name, spec) in &self.toml.inputs {
            properties[name] = spec.to_json_schema();
            if spec.required { required.push(name.as_str()); }
        }

        json!({ "type": "object", "properties": properties, "required": required })
    }
}

// ============================================================================
// SKILL DISCOVERY
// ============================================================================

struct DiscoveryResult {
    skills: Vec<Skill>,
    errors: Vec<(String, String)>,   // (folder name, error message)
}

fn discover_skills(root: &str, host_env: &HashMap<String, String>) -> DiscoveryResult {
    let mut result = DiscoveryResult { skills: Vec::new(), errors: Vec::new() };
    walk_skills(Path::new(root), &mut result, host_env);
    result
}

fn walk_skills(dir: &Path, out: &mut DiscoveryResult, host_env: &HashMap<String, String>) {
    let entries = match fs::read_dir(dir) {
        Ok(e)  => e,
        Err(e) => {
            out.errors.push((dir.display().to_string(), format!("cannot read dir: {e}")));
            return;
        }
    };

    let mut subdirs   = Vec::new();
    let mut toml_path = None;

    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            subdirs.push(p);
        } else if p.file_name().map(|n| n == "skill.toml").unwrap_or(false) {
            toml_path = Some(p);
        }
    }

    if let Some(path) = toml_path {
        let folder_name = path.parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());

        match load_skill(&path, host_env) {
            Ok(skill) => out.skills.push(skill),
            Err(e)    => {
                error!("skill rejected [{}]: {e}", folder_name);
                out.errors.push((folder_name, e));
            }
        }
        return;
    }

    for sub in subdirs { walk_skills(&sub, out, host_env); }
}

fn load_skill(toml_path: &Path, host_env: &HashMap<String, String>) -> Result<Skill, String> {
    let raw  = fs::read_to_string(toml_path)
        .map_err(|e| format!("read error: {e}"))?;
    let toml: SkillToml = ::toml::from_str(&raw).map_err(|e| {
        let hint = if e.to_string().contains("newline") || e.to_string().contains("inline") {
            " (hint: multi-line inline tables are invalid TOML — use [inputs.field] table syntax instead)"
        } else {
            " (hint: run 'skills validate' or check https://toml.io for syntax reference)"
        };
        format!("parse error in skill.toml: {e}{hint}")
    })?;

    let folder = toml_path.parent().ok_or("no parent dir")?;
    let script = folder.join(&toml.runtime.entrypoint);
    if !script.exists() {
        return Err(format!("entrypoint not found: {}", script.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&script)
            .map_err(|e| format!("cannot stat entrypoint: {e}"))?
            .permissions().mode();
        if mode & 0o111 == 0 {
            return Err(format!(
                "entrypoint not executable: {} — run: chmod +x {}",
                script.display(), script.display()
            ));
        }
    }

    // Resolve secrets from host env — fail loud at startup, not at call time
    let mut env = Vec::new();
    for name in &toml.permissions.secrets {
        match host_env.get(name) {
            Some(v) => env.push((name.clone(), v.clone())),
            None    => return Err(format!("declared secret '{name}' not in host environment")),
        }
    }

    // Pre-compile all regex patterns — fail loud on bad pattern
    let mut patterns = HashMap::new();
    for (input_name, spec) in &toml.inputs {
        if let Some(ref pat) = spec.pattern {
            let re = Regex::new(pat).map_err(|e| {
                format!("input '{input_name}' has invalid regex pattern '{pat}': {e}")
            })?;
            patterns.insert(input_name.clone(), re);
        }
    }

    Ok(Skill { toml, script, env, patterns })
}

// ============================================================================
// PREFLIGHT VALIDATION
// ============================================================================

#[derive(Debug, Serialize, Clone)]
struct ValidationIssue {
    severity: &'static str,   // "error" | "warn"
    code:     &'static str,   // machine-parseable, stable across versions
    field:    String,
    message:  String,
    fix:      String,
}

fn validate_skill(s: &Skill) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();
    let name = &s.toml.name;

    // ── Hard errors ───────────────────────────────────────────────────────────

    // Every input must declare a flag starting with --
    for (iname, spec) in &s.toml.inputs {
        if !spec.flag.starts_with("--") {
            issues.push(ValidationIssue {
                severity: "error",
                code:     "INVALID_FLAG",
                field:    format!("inputs.{iname}.flag"),
                message:  format!("Input '{iname}' flag '{}' must start with '--'.", spec.flag),
                fix:      "Use long-form flags, e.g. flag = \"--city\".".into(),
            });
        }
    }

    // ── Soft warnings ─────────────────────────────────────────────────────────

    if s.toml.description.trim().len() < 10 {
        issues.push(ValidationIssue {
            severity: "warn",
            code:     "SHORT_DESCRIPTION",
            field:    "description".into(),
            message:  format!("Description is only {} chars.", s.toml.description.trim().len()),
            fix:      "Write at least one sentence describing when an LLM should use this tool.".into(),
        });
    }

    if s.toml.runtime.timeout_secs > 60 {
        issues.push(ValidationIssue {
            severity: "warn",
            code:     "HIGH_TIMEOUT",
            field:    "runtime.timeout_secs".into(),
            message:  format!("timeout_secs={} exceeds 60s.", s.toml.runtime.timeout_secs),
            fix:      "High timeouts hold semaphore slots. Reduce or document the reason.".into(),
        });
    }

    if s.toml.inputs.is_empty() {
        issues.push(ValidationIssue {
            severity: "warn",
            code:     "NO_INPUTS",
            field:    "inputs".into(),
            message:  format!("Skill '{name}' has no [inputs] defined."),
            fix:      "Add at least one input so agents know what to pass.".into(),
        });
    }

    for (iname, spec) in &s.toml.inputs {
        if spec.required && spec.description.trim().len() < 5 {
            issues.push(ValidationIssue {
                severity: "warn",
                code:     "SHORT_INPUT_DESCRIPTION",
                field:    format!("inputs.{iname}.description"),
                message:  format!("Required input '{iname}' has a very short description."),
                fix:      "Describe what value to provide, e.g. 'Full Slovenian address'.".into(),
            });
        }
    }

    issues
}

fn preflight(skills: Vec<Skill>) -> Vec<Skill> {
    info!("━━━ Preflight ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    let mut valid    = Vec::new();
    let mut rejected = 0usize;

    for skill in skills {
        let issues    = validate_skill(&skill);
        let has_error = issues.iter().any(|i| i.severity == "error");

        for i in &issues {
            match i.severity {
                "error" => error!("  ✗ [{}] {} — {}", i.code, skill.toml.name, i.message),
                _       => warn! ("  ⚠ [{}] {} — {}", i.code, skill.toml.name, i.message),
            }
        }

        if has_error {
            error!("  ✗ {} REJECTED", skill.toml.name);
            rejected += 1;
        } else {
            if issues.is_empty() {
                info!("  ✓ {} v{}", skill.toml.name, skill.toml.version);
            } else {
                info!("  ✓ {} v{} (with warnings)", skill.toml.name, skill.toml.version);
            }
            valid.push(skill);
        }
    }

    info!("━━━ {}/{} skills registered ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━",
        valid.len(), valid.len() + rejected);
    valid
}

/// Warn about key policy issues at startup — warnings only, never fatal.
fn warn_key_policies(keys: &[ApiKey], skills: &[Skill]) {
    let known_tools: Vec<&str> = skills.iter().map(|s| s.toml.name.as_str()).collect();
    let now = Utc::now();

    for k in keys {
        if !k.active { continue; }

        // Warn if key is already expired
        if let Some(ref exp) = k.expires_at {
            match exp.parse::<DateTime<Utc>>() {
                Err(_) => warn!("key '{}': expires_at='{}' is not valid RFC3339 — treated as expired", k.id, exp),
                Ok(dt) if dt <= now => warn!("key '{}': expired at {} — will be rejected", k.id, exp),
                Ok(dt) => info!("key '{}': expires {}", k.id, dt.format("%Y-%m-%d")),
            }
        }

        // Warn if allowed_tools references unknown skill names
        if let Some(ref tools) = k.allowed_tools {
            for t in tools {
                if t == "*" { continue; }
                if !known_tools.contains(&t.as_str()) {
                    warn!("key '{}': allowed_tools references unknown skill '{}' — will never match", k.id, t);
                }
            }
        }
    }
}

// ============================================================================
// RATE LIMITER  (in-memory, per client_id, sliding 60s window)
// ============================================================================

struct RateBucket {
    count:        u32,
    window_start: Instant,
}

fn rate_check(map: &DashMap<String, RateBucket>, client_id: &str, limit: u32) -> bool {
    let now    = Instant::now();
    let window = Duration::from_secs(60);

    let mut bucket = map.entry(client_id.to_string()).or_insert(RateBucket {
        count:        0,
        window_start: now,
    });

    if bucket.window_start.elapsed() >= window {
        bucket.count        = 0;
        bucket.window_start = now;
    }

    if bucket.count >= limit { return false; }
    bucket.count += 1;
    true
}

// ============================================================================
// JSON-RPC 2.0
// ============================================================================

#[derive(Debug, Deserialize)]
struct RpcRequest {
    jsonrpc: String,
    id:      Option<Value>,
    method:  String,
    #[serde(default)]
    params:  Value,
}

#[derive(Debug, Serialize)]
struct RpcResponse {
    jsonrpc: &'static str,
    id:      Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result:  Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error:   Option<RpcError>,
}

#[derive(Debug, Serialize)]
struct RpcError {
    code:    i32,
    message: String,
}

fn rpc_ok(id: Value, result: Value) -> Response {
    Json(RpcResponse { jsonrpc: "2.0", id, result: Some(result), error: None })
        .into_response()
}

fn rpc_err(id: Value, code: i32, message: impl Into<String>) -> Response {
    Json(RpcResponse {
        jsonrpc: "2.0",
        id,
        result:  None,
        error:   Some(RpcError { code, message: message.into() }),
    })
    .into_response()
}

// ============================================================================
// APP STATE
// ============================================================================

#[derive(Clone)]
struct AppState {
    keys:       Arc<Vec<ApiKey>>,
    skills:     Arc<Vec<Skill>>,
    sem:        Arc<Semaphore>,
    buckets:    Arc<DashMap<String, RateBucket>>,
    rate_limit: u32,
}

// ============================================================================
// AUTH
// ============================================================================

/// Try to authenticate from Bearer token.
/// Returns Ok(Some(key)) if valid token found.
/// Returns Ok(None) if no token present (anonymous — caller decides if ok).
/// Returns Err(401) if token present but invalid or expired.
fn try_authenticate<'a>(headers: &HeaderMap, keys: &'a [ApiKey]) -> Result<Option<&'a ApiKey>, Response> {
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    let Some(token) = bearer else {
        return Ok(None);   // no header at all — anonymous
    };

    match keys.iter().find(|k| k.key == token) {
        None => Err((StatusCode::UNAUTHORIZED, "Unauthorized").into_response()),
        Some(k) if !k.is_valid() => {
            warn!("rejected expired/inactive key id={}", k.id);
            Err((StatusCode::UNAUTHORIZED, "Key expired or inactive").into_response())
        }
        Some(k) => Ok(Some(k)),
    }
}

/// Convenience: require a valid authenticated key (non-public paths).
fn authenticate<'a>(headers: &HeaderMap, keys: &'a [ApiKey]) -> Result<&'a ApiKey, Response> {
    try_authenticate(headers, keys)?
        .ok_or_else(|| (StatusCode::UNAUTHORIZED, "Unauthorized").into_response())
}

/// Extract optional MCP session ID for echo (stateless — we just reflect it).
fn session_id(headers: &HeaderMap) -> Option<String> {
    headers.get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

// ============================================================================
// GET /mcp  — SSE transport (backward compat with older aggregators)
// ============================================================================

async fn sse_handler(
    headers:      HeaderMap,
    State(state): State<AppState>,
) -> Result<Sse<ReceiverStream<Result<Event, std::convert::Infallible>>>, Response> {
    authenticate(&headers, &state.keys)?;  // SSE always requires auth

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, std::convert::Infallible>>(8);

    tokio::spawn(async move {
        // Announce the POST endpoint per old SSE transport spec
        let ev = Event::default()
            .event("endpoint")
            .data(json!({ "mcp_endpoint": "/mcp" }).to_string());

        if tx.send(Ok(ev)).await.is_err() { return; }

        // Keepalive — comment frames every 15s
        let mut ticker = tokio::time::interval(Duration::from_secs(15));
        loop {
            ticker.tick().await;
            if tx.send(Ok(Event::default().comment("keepalive"))).await.is_err() { break; }
        }
    });

    Ok(Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default()))
}

// ============================================================================
// POST /mcp  — MCP JSON-RPC handler
// ============================================================================

async fn mcp_handler(
    headers:      HeaderMap,
    State(state): State<AppState>,
    Json(req):    Json<RpcRequest>,
) -> Response {
    // try_authenticate: Ok(Some) = authed, Ok(None) = anonymous, Err = bad token
    let authed_key = match try_authenticate(&headers, &state.keys) {
        Ok(k)  => k,
        Err(r) => return r,
    };

    if req.jsonrpc != "2.0" {
        return rpc_err(Value::Null, -32600, "jsonrpc must be \"2.0\"");
    }

    let id  = req.id.clone().unwrap_or(Value::Null);
    let sid = session_id(&headers);

    let response = dispatch(req, id, authed_key, &state).await;

    // Echo Mcp-Session-Id if present — stateless reflection for aggregator compat
    if let Some(s) = sid {
        let mut r = response;
        r.headers_mut().insert(
            "mcp-session-id",
            s.parse().unwrap_or_else(|_| "invalid".parse().unwrap()),
        );
        return r;
    }

    response
}

async fn dispatch(
    req:        RpcRequest,
    id:         Value,
    authed_key: Option<&ApiKey>,   // None = anonymous (only public tools allowed)
    state:      &AppState,
) -> Response {
    let client_id = authed_key.map(|k| k.id.as_str()).unwrap_or("anonymous");
    match req.method.as_str() {

        // ── initialize ────────────────────────────────────────────────────────
        "initialize" => {
            info!("initialize client={client_id}");
            rpc_ok(id, json!({
                "protocolVersion": "2024-11-05",
                "serverInfo": {
                    "name":    "suckless-mcp",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "capabilities": {
                    "tools": {}
                }
            }))
        }

        // ── notifications/initialized — no response required ──────────────────
        "notifications/initialized" => {
            (StatusCode::NO_CONTENT, "").into_response()
        }

        // ── tools/list ────────────────────────────────────────────────────────
        "tools/list" => {
            // Unauthenticated callers see only public tools.
            // Authenticated callers see all tools they are allowed to call.
            let tools: Vec<Value> = state.skills.iter()
                .filter(|s| {
                    s.toml.public || authed_key.map(|k| k.allows_tool(&s.toml.name)).unwrap_or(false)
                })
                .map(|s| json!({
                    "name":        s.toml.name,
                    "description": format!("{} (v{})", s.toml.description, s.toml.version),
                    "inputSchema": s.input_schema(),
                }))
                .collect();

            info!("tools/list client={client_id} count={}", tools.len());
            rpc_ok(id, json!({ "tools": tools }))
        }

        // ── tools/call ────────────────────────────────────────────────────────
        "tools/call" => {
            // Rate limit
            if !rate_check(&state.buckets, client_id, state.rate_limit) {
                warn!("rate_limit client={client_id}");
                return rpc_err(id, -32029, "Rate limit exceeded. Retry in 60s.");
            }

            let tool_name = match req.params["name"].as_str() {
                Some(n) => n.to_string(),
                None    => return rpc_err(id, -32602, "params.name is required"),
            };

            let skill = match state.skills.iter().find(|s| s.toml.name == tool_name) {
                Some(s) => s.clone(),
                None    => return rpc_err(id, -32601, format!("unknown tool: {tool_name}")),
            };

            // Auth check: public tools bypass auth; private tools require a valid key
            // with the tool in its allowlist (or no allowlist = all allowed).
            if !skill.toml.public {
                match authed_key {
                    None => {
                        warn!("anon_call_private tool={tool_name}");
                        return rpc_err(id, -32001, format!(
                            "tool '{tool_name}' requires authentication"
                        ));
                    }
                    Some(k) if !k.allows_tool(&tool_name) => {
                        warn!("tool_denied client={} tool={tool_name}", k.id);
                        return rpc_err(id, -32001, format!(
                            "key '{}' is not permitted to call '{tool_name}'", k.id
                        ));
                    }
                    Some(_) => {}   // authenticated and allowed — proceed
                }
            }

            let args = &req.params["arguments"];

            // ── Input validation ──────────────────────────────────────────────
            for (name, spec) in &skill.toml.inputs {
                let val = &args[name];

                if spec.required && val.is_null() {
                    return rpc_err(id, -32602, format!("missing required argument: '{name}'"));
                }
                if val.is_null() { continue; }

                // Type check
                let type_ok = match spec.kind {
                    InputType::String  => val.is_string(),
                    InputType::Integer => val.is_i64() || val.is_u64(),
                    InputType::Boolean => val.is_boolean(),
                    InputType::Number  => val.is_number(),
                };
                if !type_ok {
                    return rpc_err(id, -32602,
                        format!("argument '{name}' must be type {}", spec.kind.as_str()));
                }

                if let Some(s) = val.as_str() {
                    // Strip whitespace before further validation
                    let s = s.trim();

                    // Max length
                    if s.len() > spec.max_length {
                        return rpc_err(id, -32602,
                            format!("argument '{name}' exceeds max_length={}", spec.max_length));
                    }

                    // Enum check
                    if !spec.enum_values.is_empty() && !spec.enum_values.iter().any(|e| e == s) {
                        return rpc_err(id, -32602,
                            format!("argument '{name}' must be one of: {}",
                                spec.enum_values.join(", ")));
                    }

                    // Pattern check (pre-compiled at startup)
                    if let Some(re) = skill.patterns.get(name) {
                        if !re.is_match(s) {
                            return rpc_err(id, -32602,
                                format!("argument '{name}' does not match required pattern"));
                        }
                    }
                }
            }

            // ── Concurrency slot ──────────────────────────────────────────────
            let _permit = match state.sem.clone().acquire_owned().await {
                Ok(p)  => p,
                Err(_) => return rpc_err(id, -32603, "semaphore closed"),
            };

            // ── Build argv ────────────────────────────────────────────────────
            // <entrypoint> --flag value ...
            // Entrypoint must be executable with a shebang (#!/usr/bin/env python3 etc).
            // Boolean flags emit --flag only (no value) when true.
            // Never shell=true, never string concatenation.
            let mut cmd = Command::new(&skill.script);

            for (name, spec) in &skill.toml.inputs {
                let val = &args[name];
                if val.is_null() {
                    // Use default if declared, otherwise skip optional arg
                    if let Some(ref d) = spec.default {
                        if spec.kind == InputType::Boolean {
                            if d.as_bool().unwrap_or(false) { cmd.arg(&spec.flag); }
                        } else {
                            cmd.arg(&spec.flag);
                            cmd.arg(d.as_str().unwrap_or(&d.to_string()));
                        }
                    }
                    continue;
                }
                match spec.kind {
                    InputType::Boolean => {
                        if val.as_bool().unwrap_or(false) { cmd.arg(&spec.flag); }
                    }
                    InputType::String => {
                        cmd.arg(&spec.flag);
                        cmd.arg(val.as_str().unwrap_or("").trim());
                    }
                    InputType::Integer => {
                        cmd.arg(&spec.flag);
                        cmd.arg(val.as_i64().unwrap_or(0).to_string());
                    }
                    InputType::Number => {
                        cmd.arg(&spec.flag);
                        cmd.arg(val.as_f64().unwrap_or(0.0).to_string());
                    }
                }
            }

            // Wipe env — inject only declared secrets
            cmd.env_clear();
            for (k, v) in &skill.env { cmd.env(k, v); }

            cmd.stdout(std::process::Stdio::piped());
            cmd.stderr(std::process::Stdio::piped());

            let start = Instant::now();

            let child = match cmd.spawn() {
                Ok(c)  => c,
                Err(e) => {
                    error!("spawn failed skill={tool_name}: {e}");
                    return rpc_err(id, -32603, format!("spawn failed: {e}"));
                }
            };

            // ── Watchdog ──────────────────────────────────────────────────────
            let output = match timeout(
                Duration::from_secs(skill.toml.runtime.timeout_secs),
                child.wait_with_output(),
            ).await {
                Err(_)     => {
                    warn!("timeout skill={tool_name}");
                    return rpc_err(id, -32603,
                        format!("timed out after {}s", skill.toml.runtime.timeout_secs));
                }
                Ok(Err(e)) => {
                    error!("wait error skill={tool_name}: {e}");
                    return rpc_err(id, -32603, format!("execution error: {e}"));
                }
                Ok(Ok(o))  => o,
            };

            let duration_ms = start.elapsed().as_millis();

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                warn!("skill_failed skill={tool_name}: {stderr}");
                return rpc_err(id, -32603, format!("script error: {stderr}"));
            }

            if output.stdout.len() > skill.toml.runtime.max_output_bytes {
                warn!("output_overflow skill={tool_name} bytes={}", output.stdout.len());
                return rpc_err(id, -32603,
                    format!("output exceeded {} bytes", skill.toml.runtime.max_output_bytes));
            }

            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            if serde_json::from_str::<Value>(&stdout).is_err() {
                warn!("invalid_json skill={tool_name}");
                return rpc_err(id, -32603, "script output is not valid JSON");
            }

            info!("ok skill={tool_name} client={client_id} duration_ms={duration_ms}");

            rpc_ok(id, json!({
                "content": [{ "type": "text", "text": stdout }]
            }))
        }

        other => {
            warn!("unknown_method={other} client={client_id}");
            rpc_err(id, -32601, format!("method not found: {other}"))
        }
    }
}

// ============================================================================
// GET /health
// ============================================================================

async fn health_handler() -> impl IntoResponse { (StatusCode::OK, "ok") }

// ============================================================================
// CLI OUTPUT  (structured JSON — both human and LLM readable)
// ============================================================================
//
// Every response follows this envelope:
//   status:  "ok" | "warn" | "error" | "degraded"
//   summary: one plain-English sentence for humans
//   hint:    imperative instruction for agents / small LLMs
//   data:    structured payload for programmatic use

fn cli_out(status: &str, summary: &str, hint: &str, data: Value) {
    let out = json!({
        "status":  status,
        "summary": summary,
        "hint":    hint,
        "data":    data,
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
}

fn cli_err(summary: &str, hint: &str, data: Value) {
    let out = json!({
        "status":  "error",
        "summary": summary,
        "hint":    hint,
        "data":    data,
    });
    eprintln!("{}", serde_json::to_string_pretty(&out).unwrap());
    std::process::exit(1);
}

// ── skills list ───────────────────────────────────────────────────────────────

fn cmd_skills_list(discovered: &DiscoveryResult) {
    let list: Vec<Value> = discovered.skills.iter().map(|s| json!({
        "name":         s.toml.name,
        "version":      s.toml.version,
        "description":  s.toml.description,
        "script":       s.script.display().to_string(),
        "inputs":       s.toml.inputs.keys().collect::<Vec<_>>(),
        "timeout_secs": s.toml.runtime.timeout_secs,
        "status":       "registered",
    })).collect();

    let load_errors: Vec<Value> = discovered.errors.iter()
        .map(|(folder, e)| json!({ "folder": folder, "error": e }))
        .collect();

    let n        = list.len();
    let n_errors = load_errors.len();
    let status   = if n_errors > 0 && n == 0 { "error" } else if n_errors > 0 { "warn" } else { "ok" };
    let summary  = format!("{n} skill(s) loaded. {n_errors} rejected (skill.toml errors).");
    let hint     = if n_errors > 0 {
        "Fix the rejected skills. Run 'skills validate' after correcting skill.toml."
    } else {
        "These are the tools exposed via POST /mcp. Clients calling tools/list will see exactly these."
    };

    cli_out(status, &summary, hint, json!({
        "skills":      list,
        "load_errors": load_errors,
    }));
}

// ── skills validate ───────────────────────────────────────────────────────────

fn cmd_skills_validate(discovered: &DiscoveryResult) {
    let mut results  = Vec::new();
    let mut n_ok     = 0usize;
    let mut n_warn   = 0usize;
    let mut n_error  = 0usize;

    // Load errors are hard failures — surface them first
    for (folder, e) in &discovered.errors {
        n_error += 1;
        results.push(json!({
            "skill":  folder,
            "status": "error",
            "issues": [{ "severity": "error", "code": "LOAD_FAILED",
                         "field": "skill.toml", "message": e,
                         "fix": "Fix the skill.toml parse error and re-run validate." }],
        }));
    }

    for skill in &discovered.skills {
        let issues    = validate_skill(skill);
        let has_error = issues.iter().any(|i| i.severity == "error");
        let has_warn  = issues.iter().any(|i| i.severity == "warn");

        let status = if has_error { n_error += 1; "error" }
                     else if has_warn { n_warn += 1; "warn" }
                     else { n_ok += 1; "ok" };

        results.push(json!({
            "skill":  skill.toml.name,
            "status": status,
            "issues": issues,
        }));
    }

    let total  = discovered.skills.len() + discovered.errors.len();
    let status = if n_error > 0 { "error" } else if n_warn > 0 { "warn" } else { "ok" };
    let summary = format!("{total} skill(s) checked. {n_ok} ok, {n_warn} warn, {n_error} error.");
    let hint = if n_error > 0 {
        "Fix all items where severity='error' before restarting. \
         Items where severity='warn' are optional improvements."
    } else if n_warn > 0 {
        "All skills will register. Review warnings to improve LLM tool selection accuracy."
    } else {
        "All skills passed validation. Safe to restart the gateway."
    };

    if n_error > 0 {
        let out = json!({
            "status":  status,
            "summary": summary,
            "hint":    hint,
            "data":    { "results": results },
        });
        eprintln!("{}", serde_json::to_string_pretty(&out).unwrap());
        std::process::exit(1);
    } else {
        cli_out(status, &summary, hint, json!({ "results": results }));
    }
}

// ── skills check <name> ───────────────────────────────────────────────────────

fn cmd_skills_check(discovered: &DiscoveryResult, name: &str) {
    let skill = match discovered.skills.iter().find(|s| s.toml.name == name) {
        Some(s) => s,
        None    => {
            cli_err(
                &format!("Skill '{name}' not found."),
                "Run 'suckless-mcp skills list' to see available skills. \
                 Check that the skill folder exists under skills_root and has a valid skill.toml.",
                json!({ "name": name }),
            );
            return;
        }
    };

    let issues    = validate_skill(skill);
    let has_error = issues.iter().any(|i| i.severity == "error");
    let status    = if has_error { "error" } else if !issues.is_empty() { "warn" } else { "ok" };
    let summary   = if has_error {
        format!("Skill '{}' has hard errors and will be rejected at startup.", name)
    } else if !issues.is_empty() {
        format!("Skill '{}' will register, but has warnings.", name)
    } else {
        format!("Skill '{}' passed all checks.", name)
    };
    let hint = if has_error {
        "Fix all error items, then run this command again to confirm before restarting."
    } else {
        "This skill is ready. Restart the gateway to apply any recent changes."
    };

    if has_error {
        let out = json!({
            "status": status, "summary": summary, "hint": hint,
            "data": { "skill": name, "issues": issues },
        });
        eprintln!("{}", serde_json::to_string_pretty(&out).unwrap());
        std::process::exit(1);
    } else {
        cli_out(status, &summary, hint, json!({ "skill": name, "issues": issues }));
    }
}

// ── status ────────────────────────────────────────────────────────────────────

fn cmd_status(config: &Config, discovered: &DiscoveryResult, keys: &KeysFile) {
    let active_keys  = keys.keys.iter().filter(|k| k.active).count();
    let revoked_keys = keys.keys.iter().filter(|k| !k.active).count();
    let n_skills     = discovered.skills.len();
    let n_errors     = discovered.errors.len();

    let status  = if n_skills == 0 { "error" } else if n_errors > 0 { "warn" } else { "ok" };
    let summary = format!("{n_skills} skill(s) registered. {n_errors} rejected. {active_keys} active key(s).");
    let hint    = if n_skills == 0 {
        "No skills registered. Check skills_root in config.toml and run 'skills validate'."
    } else {
        "Run 'skills list' to see registered tools. Run 'keys list' to see active clients."
    };

    cli_out(status, &summary, hint, json!({
        "skills": {
            "registered": n_skills,
            "rejected":   n_errors,
        },
        "keys": {
            "active":  active_keys,
            "revoked": revoked_keys,
        },
        "config": {
            "listen":                format!("{}:{}", config.listen_host, config.listen_port),
            "skills_root":           config.skills_root,
            "max_concurrent_tools":  config.max_concurrent_tools,
            "rate_limit_per_minute": config.rate_limit_per_minute,
        },
    }));
}

// ── keys list ─────────────────────────────────────────────────────────────────

fn cmd_keys_list(keys: &KeysFile) {
    let now = Utc::now();
    let list: Vec<Value> = keys.keys.iter().map(|k| {
        let expired = k.expires_at.as_ref().and_then(|e| e.parse::<DateTime<Utc>>().ok())
            .map(|dt| dt <= now)
            .unwrap_or(false);
        json!({
            "id":            k.id,
            "active":        k.active,
            "expires_at":    k.expires_at,
            "expired":       expired,
            "allowed_tools": k.allowed_tools,
        })
    }).collect();

    let active = keys.keys.iter().filter(|k| k.active).count();
    cli_out(
        "ok",
        &format!("{} key(s) total, {} active.", list.len(), active),
        "Raw key values are never shown. To rotate: 'keys revoke <id>' then 'keys add <id> <key>'.",
        json!({ "keys": list }),
    );
}

// ── keys add ──────────────────────────────────────────────────────────────────

fn cmd_keys_add(keys_path: &str, id: &str, key: &str) {
    let mut kf = match load_keys(keys_path) {
        Ok(k)  => k,
        Err(e) => { cli_err("Cannot load keys.toml.", &e, json!({ "path": keys_path })); return; }
    };

    if kf.keys.iter().any(|k| k.id == id) {
        cli_err(
            &format!("Key id '{id}' already exists."),
            "Choose a different id, or revoke the existing one first with 'keys revoke <id>'.",
            json!({ "id": id }),
        );
        return;
    }

    kf.keys.push(ApiKey { 
        id: id.to_string(), 
        key: key.to_string(), 
        active: true,
        expires_at: None,
        allowed_tools: None,
    });

    if let Err(e) = save_keys(keys_path, &kf) {
        cli_err("Cannot save keys.toml.", &e, json!({ "path": keys_path }));
        return;
    }

    cli_out(
        "ok",
        &format!("Key '{id}' added to keys.toml."),
        "Restart the gateway to activate: systemctl restart suckless-mcp",
        json!({ "id": id, "active": true }),
    );
}

// ── keys revoke ───────────────────────────────────────────────────────────────

fn cmd_keys_revoke(keys_path: &str, id: &str) {
    let mut kf = match load_keys(keys_path) {
        Ok(k)  => k,
        Err(e) => { cli_err("Cannot load keys.toml.", &e, json!({ "path": keys_path })); return; }
    };

    match kf.keys.iter_mut().find(|k| k.id == id) {
        None => {
            cli_err(
                &format!("Key '{id}' not found."),
                "Run 'keys list' to see available key ids.",
                json!({ "id": id }),
            );
        }
        Some(k) => {
            k.active = false;
            if let Err(e) = save_keys(keys_path, &kf) {
                cli_err("Cannot save keys.toml.", &e, json!({ "path": keys_path }));
                return;
            }
            cli_out(
                "ok",
                &format!("Key '{id}' marked inactive in keys.toml."),
                "Restart the gateway to deactivate: systemctl restart suckless-mcp. \
                 The key is not deleted — set active=true in keys.toml to restore.",
                json!({ "id": id, "active": false }),
            );
        }
    }
}

// ── schema ────────────────────────────────────────────────────────────────────

fn cmd_schema_skill() {
    let schema = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "title": "skill.toml",
        "description": "suckless-mcp skill manifest. One file per skill folder. Write your CLI tool with --flags everywhere, then describe each flag here.",
        "type": "object",
        "required": ["name", "version", "description", "runtime"],
        "additionalProperties": false,
        "properties": {
            "name": {
                "type": "string",
                "pattern": "^[a-z][a-z0-9_\\-]*$",
                "description": "Unique snake_case or kebab-case tool name, e.g. cadastral_lookup"
            },
            "version": {
                "type": "string",
                "description": "Semver string, e.g. 1.0.0"
            },
            "description": {
                "type": "string",
                "minLength": 10,
                "description": "One or two sentences describing what this tool does and when an LLM should use it."
            },
            "public": {
                "type": "boolean",
                "default": false,
                "description": "If true, tool is visible and callable without authentication. Use only for low-sensitivity tools."
            },
            "runtime": {
                "type": "object",
                "required": ["entrypoint", "timeout_secs"],
                "additionalProperties": false,
                "properties": {
                    "entrypoint": {
                        "type": "string",
                        "description": "Python script filename relative to this skill folder, e.g. my_tool.py"
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 300,
                        "description": "Max seconds before the subprocess is killed."
                    },
                    "max_output_bytes": {
                        "type": "integer",
                        "minimum": 1024,
                        "default": 32768,
                        "description": "Max stdout bytes accepted. Output beyond this is rejected."
                    }
                }
            },
            "inputs": {
                "type": "object",
                "description": "Named inputs. Each maps to one MCP inputSchema property and one --flag value pair passed to the script. Boolean inputs emit --flag only (no value) when true.",
                "additionalProperties": {
                    "type": "object",
                    "required": ["type", "flag"],
                    "additionalProperties": false,
                    "properties": {
                        "type": {
                            "type": "string",
                            "enum": ["string", "integer", "boolean", "number"],
                            "description": "JSON Schema type of this input."
                        },
                        "flag": {
                            "type": "string",
                            "pattern": "^--[a-z][a-z0-9\\-]*$",
                            "description": "Long-form CLI flag passed to the script, e.g. --city"
                        },
                        "description": {
                            "type": "string",
                            "description": "Shown to the LLM. Describe what value to provide."
                        },
                        "required": { "type": "boolean", "default": false },
                        "enum":    { "type": "array", "items": { "type": "string" },
                                    "description": "Allowlist of valid string values." },
                        "default": { "description": "Default value used when input is omitted." },
                        "pattern": { "type": "string",
                                    "description": "Regex the string value must match." },
                        "max_length": { "type": "integer", "minimum": 1, "default": 512,
                                       "description": "Max byte length for string inputs." }
                    }
                }
            },
            "permissions": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "secrets": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Env var names injected into the subprocess. Each must exist in the host environment at startup."
                    },
                    "network": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Declared outbound URLs. Informational in v1 — not enforced."
                    }
                }
            }
        }
    });
    println!("{}", serde_json::to_string_pretty(&schema).unwrap());
}

fn cmd_schema_config() {
    let schema = json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "title": "config.toml",
        "description": "suckless-mcp server configuration.",
        "type": "object",
        "required": ["listen_host", "listen_port", "skills_root", "max_concurrent_tools"],
        "additionalProperties": false,
        "properties": {
            "listen_host":           { "type": "string", "default": "127.0.0.1" },
            "listen_port":           { "type": "integer", "minimum": 1, "maximum": 65535 },
            "skills_root":           { "type": "string", "description": "Absolute path to skills folder." },
            "max_concurrent_tools":  { "type": "integer", "minimum": 1 },
            "rate_limit_per_minute": { "type": "integer", "minimum": 1, "default": 60 }
        }
    });
    println!("{}", serde_json::to_string_pretty(&schema).unwrap());
}

// ── help ──────────────────────────────────────────────────────────────────────

fn print_help() {
    println!("suckless-mcp v{}", env!("CARGO_PKG_VERSION"));
    println!("Suckless MCP remote gateway — Python CLI tools as MCP endpoints.\n");
    println!("USAGE:");
    println!("  suckless-mcp [--config <path>] <subcommand>\n");
    println!("SUBCOMMANDS:");
    println!("  serve                  Start the gateway (default)");
    println!("  status                 Show loaded state as JSON");
    println!("  skills list            List registered skills as JSON");
    println!("  skills validate        Validate all skills, report as JSON");
    println!("  skills check <name>    Validate one skill by name");
    println!("  keys list              List key ids (never raw values)");
    println!("  keys add <id> <key>    Append a key to keys.toml");
    println!("  keys revoke <id>       Mark a key inactive in keys.toml");
    println!("  schema skill           Emit skill.toml JSON Schema");
    println!("  schema config          Emit config.toml JSON Schema");
    println!("\nOPTIONS:");
    println!("  --config <path>        Config file (default: /etc/suckless-mcp/config.toml)");
    println!("\nAll subcommands output JSON. Exit 0 = success, 1 = error.");
    println!("Errors go to stderr. Data goes to stdout.");
}

// ============================================================================
// MAIN
// ============================================================================

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return;
    }

    // ── Resolve --config path ─────────────────────────────────────────────────
    let config_path = args.windows(2)
        .find(|w| w[0] == "--config")
        .map(|w| w[1].clone())
        .or_else(|| args.iter().find(|a| !a.starts_with("--") && a.ends_with(".toml")).cloned())
        .unwrap_or_else(|| "/etc/suckless-mcp/config.toml".to_string());

    let keys_path = config_path.replace("config.toml", "keys.toml");

    // ── Parse subcommand ──────────────────────────────────────────────────────
    let subcmd: Vec<&str> = args.iter()
        .filter(|a| !a.starts_with("--") && !a.ends_with(".toml"))
        .map(|a| a.as_str())
        .collect();

    // schema subcommands need no config
    match subcmd.as_slice() {
        ["schema", "skill"]   => { cmd_schema_skill();  return; }
        ["schema", "config"]  => { cmd_schema_config(); return; }
        _ => {}
    }

    // Load config — required for everything else
    let config = match load_config(&config_path) {
        Ok(c)  => c,
        Err(e) => {
            cli_err(
                "Cannot load config.toml.",
                &format!("Check that {config_path} exists and is readable. Error: {e}"),
                json!({ "path": config_path, "reason": e }),
            );
            return;
        }
    };

    let keys = match load_keys(&keys_path) {
        Ok(k)  => k,
        Err(e) => {
            cli_err(
                "Cannot load keys.toml.",
                &format!("Check that {keys_path} exists. Create it with 'keys add'. Error: {e}"),
                json!({ "path": keys_path, "reason": e }),
            );
            return;
        }
    };

    // ── keys subcommands — no skill discovery needed ──────────────────────────
    match subcmd.as_slice() {
        ["keys", "list"]         => { cmd_keys_list(&keys); return; }
        ["keys", "add", id, key] => { cmd_keys_add(&keys_path, id, key); return; }
        ["keys", "revoke", id]   => { cmd_keys_revoke(&keys_path, id); return; }
        ["keys", ..] => {
            cli_err(
                "Unknown keys subcommand.",
                "Valid: keys list | keys add <id> <key> | keys revoke <id>",
                json!({ "args": subcmd }),
            );
            return;
        }
        _ => {}
    }

    // ── Discover and validate skills ──────────────────────────────────────────
    let host_env: HashMap<String, String> = std::env::vars().collect();
    let discovered  = discover_skills(&config.skills_root, &host_env);

    // ── Remaining CLI subcommands ─────────────────────────────────────────────
    match subcmd.as_slice() {
        ["status"]                  => { cmd_status(&config, &discovered, &keys); return; }
        ["skills", "list"]          => { cmd_skills_list(&discovered); return; }
        ["skills", "validate"]      => { cmd_skills_validate(&discovered); return; }
        ["skills", "check", name]   => { cmd_skills_check(&discovered, name); return; }
        ["skills", ..] => {
            cli_err(
                "Unknown skills subcommand.",
                "Valid: skills list | skills validate | skills check <name>",
                json!({ "args": subcmd }),
            );
            return;
        }
        _ => {}
    }

    // ── serve (default) ───────────────────────────────────────────────────────
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_env("LOG_LEVEL")
                .add_directive("auth_gate_mcp=info".parse().unwrap()),
        )
        .init();

    let skills = preflight(discovered.skills);
    warn_key_policies(&keys.keys, &skills);

    if skills.is_empty() {
        eprintln!("FATAL: no valid skills registered. Fix errors above and restart.");
        std::process::exit(1);
    }

    let active_keys: Vec<ApiKey> = keys.keys.into_iter().filter(|k| k.active).collect();
    if active_keys.is_empty() {
        eprintln!("FATAL: no active keys in keys.toml. Add one with 'keys add <id> <key>'.");
        std::process::exit(1);
    }

    let state = AppState {
        keys:       Arc::new(active_keys),
        skills:     Arc::new(skills),
        sem:        Arc::new(Semaphore::new(config.max_concurrent_tools)),
        buckets:    Arc::new(DashMap::new()),
        rate_limit: config.rate_limit_per_minute,
    };

    let app = Router::new()
        .route("/mcp",    post(mcp_handler))
        .route("/mcp",    get(sse_handler))
        .route("/health", get(health_handler))
        .with_state(state);

    let addr = format!("{}:{}", config.listen_host, config.listen_port);

    info!("suckless-mcp v{} listening on {addr}", env!("CARGO_PKG_VERSION"));
    info!("skills_root           = {}", config.skills_root);
    info!("max_concurrent_tools  = {}", config.max_concurrent_tools);
    info!("rate_limit_per_minute = {}", config.rate_limit_per_minute);

    let listener = TcpListener::bind(&addr).await
        .unwrap_or_else(|e| panic!("cannot bind {addr}: {e}"));

    axum::serve(listener, app).await
        .expect("server error");
}
