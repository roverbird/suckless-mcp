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
//    GET  /health     — liveness probe, no auth
//
//  CLI FLAGS (agentic-ready, all output JSON to stdout):
//    --serve                  start the gateway (default)
//    --status                 report loaded state as JSON
//    --skills [--name N]      list skills (all or one); exits 1 on load errors
//    --keys-list              list key ids (never raw values)
//    --keys-add --id I --key K  append a key to keys.toml
//    --keys-revoke --id I     mark a key inactive
//
//  FILES:
//    /etc/suckless-mcp/config.toml   — server settings
//    /etc/suckless-mcp/keys.toml     — api keys
//    /opt/skills/<name>/skill.toml   — one per skill, auto-discovered
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
    http::{HeaderMap, Request, StatusCode},
    middleware::{self, Next},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::{net::TcpListener, process::Command, sync::Semaphore, time::timeout};
use tracing::{error, info, warn};

// ============================================================================
// CONSTANTS
// ============================================================================

const SKILLS_ROOT:       &str = "/opt/skills";
const MAX_OUTPUT_BYTES:  usize = 65_536;   // 64 KB global limit

// ============================================================================
// CONFIG  (/etc/suckless-mcp/config.toml)
// ============================================================================

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct Config {
    listen_host:          String,
    listen_port:          u16,
    max_concurrent_tools: usize,
}

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
    description: String,
    runtime:     Runtime,
    #[serde(default)]
    inputs:      HashMap<String, InputSpec>,
    #[serde(default)]
    permissions: Permissions,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct Runtime {
    entrypoint:   String,
    timeout_secs: u64,
}

/// One parameter in [inputs].
/// Maps to one MCP inputSchema property and one --flag value pair in subprocess argv.
/// Boolean flags emit --flag only (no value) when true.
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
    #[serde(default)]
    default:     Option<Value>,
}

impl InputSpec {
    fn to_json_schema(&self) -> Value {
        let mut prop = json!({
            "type":        self.kind.as_str(),
            "description": self.description,
        });
        if let Some(ref d) = self.default { prop["default"] = d.clone(); }
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
}

// ============================================================================
// INTERNAL SKILL RECORD
// ============================================================================

#[derive(Clone)]
struct Skill {
    toml:   SkillToml,
    script: PathBuf,
    env:    Vec<(String, String)>,
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

fn discover_skills(host_env: &HashMap<String, String>) -> DiscoveryResult {
    let mut result = DiscoveryResult { skills: Vec::new(), errors: Vec::new() };
    walk_skills(Path::new(SKILLS_ROOT), &mut result, host_env);
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
            " (hint: run '--skills' or check https://toml.io for syntax reference)"
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

    // Validate all input flags up-front — fail loud, not at call time
    for (iname, spec) in &toml.inputs {
        if !spec.flag.starts_with("--") {
            return Err(format!(
                "input '{iname}' flag '{}' must start with '--' (e.g. --{})",
                spec.flag, iname
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

    Ok(Skill { toml, script, env })
}

// ============================================================================
// PREFLIGHT VALIDATION
// ============================================================================

fn preflight(skills: Vec<Skill>) -> Vec<Skill> {
    info!("━━━ Preflight ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    // load_skill() already rejects invalid skills hard.
    // Preflight is now just a startup log of what registered.
    for s in &skills {
        info!("  ✓ {}", s.toml.name);
    }
    info!("━━━ {}/{} skills registered ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━",
        skills.len(), skills.len());
    skills
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

fn rpc_ok(id: Value, result: Value) -> axum::response::Response {
    Json(RpcResponse { jsonrpc: "2.0", id, result: Some(result), error: None })
        .into_response()
}

fn rpc_err(id: Value, code: i32, message: impl Into<String>) -> axum::response::Response {
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
    keys:   Arc<Vec<ApiKey>>,
    skills: Arc<Vec<Skill>>,
    sem:    Arc<Semaphore>,
}

// ============================================================================
// AUTH
// ============================================================================

fn authenticate<'a>(headers: &HeaderMap, keys: &'a [ApiKey]) -> Result<&'a ApiKey, axum::response::Response> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(|| (StatusCode::UNAUTHORIZED, "Unauthorized").into_response())?;

    keys.iter()
        .find(|k| k.key == token && k.active)
        .ok_or_else(|| {
            warn!("rejected invalid/inactive key");
            (StatusCode::UNAUTHORIZED, "Unauthorized").into_response()
        })
}

/// Tower middleware: runs before routing, so every request — regardless of method
/// or path — is rejected with 401 if it lacks a valid Bearer token.
/// The /health route is explicitly exempted.
async fn auth_middleware(
    State(state): State<AppState>,
    req: Request<axum::body::Body>,
    next: Next,
) -> axum::response::Response {
    // Health check is public — skip auth
    if req.uri().path() == "/health" {
        return next.run(req).await;
    }

    let token = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    let valid = match token {
        Some(t) => state.keys.iter().any(|k| k.key == t && k.active),
        None    => false,
    };

    if !valid {
        warn!("auth_middleware: rejected unauthenticated request method={} path={}",
            req.method(), req.uri().path());
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }

    next.run(req).await
}

/// Extract optional MCP session ID for echo (stateless — we just reflect it).
fn session_id(headers: &HeaderMap) -> Option<String> {
    headers.get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

// ============================================================================
// POST /mcp  — MCP JSON-RPC handler
// ============================================================================

async fn mcp_handler(
    headers:      HeaderMap,
    State(state): State<AppState>,
    Json(req):    Json<RpcRequest>,
) -> axum::response::Response {
    let key = match authenticate(&headers, &state.keys) {
        Ok(k)  => k,
        Err(r) => return r,
    };

    if req.jsonrpc != "2.0" {
        return rpc_err(Value::Null, -32600, "jsonrpc must be \"2.0\"");
    }

    let id  = req.id.clone().unwrap_or(Value::Null);
    let sid = session_id(&headers);

    let response = dispatch(req, id, key, &state).await;

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
    req:   RpcRequest,
    id:    Value,
    key:   &ApiKey,
    state: &AppState,
) -> axum::response::Response {
    match req.method.as_str() {

        // ── initialize ────────────────────────────────────────────────────────
        "initialize" => {
            info!("initialize client={}", key.id);
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
            let tools: Vec<Value> = state.skills.iter()
                .map(|s| json!({
                    "name":        s.toml.name,
                    "description": s.toml.description,
                    "inputSchema": s.input_schema(),
                }))
                .collect();

            info!("tools/list client={} count={}", key.id, tools.len());
            rpc_ok(id, json!({ "tools": tools }))
        }

        // ── tools/call ────────────────────────────────────────────────────────
        "tools/call" => {
            let tool_name = match req.params["name"].as_str() {
                Some(n) => n.to_string(),
                None    => return rpc_err(id, -32602, "params.name is required"),
            };

            let skill = match state.skills.iter().find(|s| s.toml.name == tool_name) {
                Some(s) => s.clone(),
                None    => return rpc_err(id, -32601, format!("unknown tool: {tool_name}")),
            };

            let args = &req.params["arguments"];

            // ── Required argument check ───────────────────────────────────────
            for (name, spec) in &skill.toml.inputs {
                if spec.required && args[name].is_null() {
                    return rpc_err(id, -32602, format!("missing required argument: '{name}'"));
                }
            }

            // ── Type check ────────────────────────────────────────────────────
            for (name, spec) in &skill.toml.inputs {
                let val = &args[name];
                if val.is_null() { continue; }

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

            if output.stdout.len() > MAX_OUTPUT_BYTES {
                warn!("output_overflow skill={tool_name} bytes={}", output.stdout.len());
                return rpc_err(id, -32603,
                    format!("output exceeded {MAX_OUTPUT_BYTES} bytes"));
            }

            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            if serde_json::from_str::<Value>(&stdout).is_err() {
                warn!("invalid_json skill={tool_name}");
                return rpc_err(id, -32603, "script output is not valid JSON");
            }

            info!("ok skill={tool_name} client={} duration_ms={duration_ms}", key.id);

            rpc_ok(id, json!({
                "content": [{ "type": "text", "text": stdout }]
            }))
        }

        other => {
            warn!("unknown_method={other} client={}", key.id);
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

fn cli_out(status: &str, summary: &str, hint: &str, data: Value) {
    println!("{}", serde_json::to_string_pretty(&json!({
        "status":  status,
        "summary": summary,
        "hint":    hint,
        "data":    data,
    })).unwrap());
}

fn cli_err(summary: &str, hint: &str, data: Value) -> ! {
    eprintln!("{}", serde_json::to_string_pretty(&json!({
        "status":  "error",
        "summary": summary,
        "hint":    hint,
        "data":    data,
    })).unwrap());
    std::process::exit(1);
}

// ── skills ────────────────────────────────────────────────────────────────────
//
// Lists all discovered skills plus any load errors.
// Exits 1 if any skill failed to load — safe to use in deploy scripts:
//   suckless-mcp --skills && systemctl restart suckless-mcp
//
// With --name: narrows to one skill (exits 1 if not found).

fn cmd_skills(discovered: &DiscoveryResult, name: Option<&str>) {
    if let Some(name) = name {
        match discovered.skills.iter().find(|s| s.toml.name == name) {
            None    => cli_err(
                &format!("Skill '{name}' not found."),
                "Run '--skills' to see all available skills.",
                json!({ "name": name }),
            ),
            Some(s) => cli_out("ok",
                &format!("Skill '{}' loaded.", s.toml.name),
                "Restart the gateway to apply any recent changes.",
                json!({
                    "name":         s.toml.name,
                    "description":  s.toml.description,
                    "script":       s.script.display().to_string(),
                    "inputs":       s.toml.inputs.keys().collect::<Vec<_>>(),
                    "timeout_secs": s.toml.runtime.timeout_secs,
                }),
            ),
        }
        return;
    }

    let skills: Vec<Value> = discovered.skills.iter().map(|s| json!({
        "name":         s.toml.name,
        "description":  s.toml.description,
        "script":       s.script.display().to_string(),
        "inputs":       s.toml.inputs.keys().collect::<Vec<_>>(),
        "timeout_secs": s.toml.runtime.timeout_secs,
        "status":       "ok",
    })).collect();

    let errors: Vec<Value> = discovered.errors.iter()
        .map(|(folder, e)| json!({ "name": folder, "status": "error", "error": e }))
        .collect();

    let n        = skills.len();
    let n_errors = errors.len();
    let status   = if n_errors > 0 && n == 0 { "error" } else if n_errors > 0 { "warn" } else { "ok" };
    let summary  = format!("{n} skill(s) ok, {n_errors} failed to load.");
    let hint     = if n_errors > 0 { "Fix load errors before restarting." }
                   else            { "These are the tools exposed via POST /mcp." };

    let mut all = skills;
    all.extend(errors);

    if n_errors > 0 {
        eprintln!("{}", serde_json::to_string_pretty(&json!({
            "status": status, "summary": summary, "hint": hint,
            "data": { "skills": all },
        })).unwrap());
        std::process::exit(1);
    } else {
        cli_out(status, &summary, hint, json!({ "skills": all }));
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
        "No skills registered. Check SKILLS_ROOT (/opt/skills) and run '--skills'."
    } else {
        "Run '--skills' to see registered tools. Run '--keys-list' to see active clients."
    };

    cli_out(status, &summary, hint, json!({
        "skills": { "registered": n_skills, "rejected": n_errors },
        "keys":   { "active": active_keys, "revoked": revoked_keys },
        "config": {
            "listen":               format!("{}:{}", config.listen_host, config.listen_port),
            "skills_root":          SKILLS_ROOT,
            "max_concurrent_tools": config.max_concurrent_tools,
        },
    }));
}

// ── keys list ─────────────────────────────────────────────────────────────────

fn cmd_keys_list(keys: &KeysFile) {
    let list: Vec<Value> = keys.keys.iter().map(|k| json!({
        "id":     k.id,
        "active": k.active,
    })).collect();

    let active = keys.keys.iter().filter(|k| k.active).count();
    cli_out(
        "ok",
        &format!("{} key(s) total, {} active.", list.len(), active),
        "Raw key values are never shown. To rotate: '--keys-revoke --id ID' then '--keys-add --id ID --key K'.",
        json!({ "keys": list }),
    );
}

// ── keys add ──────────────────────────────────────────────────────────────────

fn cmd_keys_add(keys_path: &str, id: &str, key: &str) {
    let mut kf = match load_keys(keys_path) {
        Ok(k)  => k,
        Err(e) => cli_err("Cannot load keys.toml.", &e, json!({ "path": keys_path })),
    };

    if kf.keys.iter().any(|k| k.id == id) {
        cli_err(
            &format!("Key id '{id}' already exists."),
            "Choose a different id, or revoke the existing one first with '--keys-revoke --id ID'.",
            json!({ "id": id }),
        );
    }

    kf.keys.push(ApiKey { id: id.to_string(), key: key.to_string(), active: true });

    if let Err(e) = save_keys(keys_path, &kf) {
        cli_err("Cannot save keys.toml.", &e, json!({ "path": keys_path }));
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
        Err(e) => cli_err("Cannot load keys.toml.", &e, json!({ "path": keys_path })),
    };

    match kf.keys.iter_mut().find(|k| k.id == id) {
        None => cli_err(
            &format!("Key '{id}' not found."),
            "Run '--keys-list' to see available key ids.",
            json!({ "id": id }),
        ),
        Some(k) => {
            k.active = false;
            if let Err(e) = save_keys(keys_path, &kf) {
                cli_err("Cannot save keys.toml.", &e, json!({ "path": keys_path }));
            }
            cli_out(
                "ok",
                &format!("Key '{id}' marked inactive in keys.toml."),
                "Restart the gateway to deactivate: systemctl restart suckless-mcp.",
                json!({ "id": id, "active": false }),
            );
        }
    }
}

// ── help ──────────────────────────────────────────────────────────────────────

fn print_help() {
    println!("suckless-mcp v{}", env!("CARGO_PKG_VERSION"));
    println!("Suckless MCP remote gateway — CLI tools as MCP endpoints.\n");
    println!("USAGE:");
    println!("  suckless-mcp --<action> [--<param> <value> ...]\n");
    println!("ACTIONS:");
    println!("  --serve                        Start the gateway (default if no action given)");
    println!("  --status                       Show loaded state as JSON");
    println!("  --skills                       List all skills; exits 1 on load errors");
    println!("  --skills --name NAME           Show one skill by name");
    println!("  --keys-list                    List key ids (never raw values)");
    println!("  --keys-add --id ID --key KEY   Append a key to keys.toml");
    println!("  --keys-revoke --id ID          Mark a key inactive in keys.toml");
    println!("\nOPTIONS:");
    println!("  --config PATH   Config file (default: /etc/suckless-mcp/config.toml)");
    println!("  --help          Show this message");
    println!("\nSkills root is hardcoded to: {SKILLS_ROOT}");
    println!("All actions output JSON. Exit 0 = success, 1 = error.");
}

/// Minimal flag parser. Returns the value of the first occurrence of `flag`,
/// or None if absent. Handles both `--flag value` and `--flag=value` forms.
fn flag<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let prefix = format!("{flag}=");
    for (i, a) in args.iter().enumerate() {
        if a == flag {
            return args.get(i + 1).map(|s| s.as_str());
        }
        if let Some(val) = a.strip_prefix(&prefix) {
            return Some(val);
        }
    }
    None
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if has_flag(&args, "--help") || has_flag(&args, "-h") || args.is_empty() {
        print_help();
        return;
    }

    // ── Global options ────────────────────────────────────────────────────────
    let config_path = flag(&args, "--config")
        .unwrap_or("/etc/suckless-mcp/config.toml");
    let keys_path = config_path.replace("config.toml", "keys.toml");

    // ── Actions that need no config ───────────────────────────────────────────
    // (none currently — all actions need at least keys.toml)

    // ── Load config and keys — required for all actions ───────────────────────
    let config = match load_config(config_path) {
        Ok(c)  => c,
        Err(e) => cli_err(
            "Cannot load config.toml.",
            &format!("Check that {config_path} exists and is readable. Error: {e}"),
            json!({ "path": config_path, "reason": e }),
        ),
    };

    let keys = match load_keys(&keys_path) {
        Ok(k)  => k,
        Err(e) => cli_err(
            "Cannot load keys.toml.",
            &format!("Check that {keys_path} exists. Create it with '--keys-add'. Error: {e}"),
            json!({ "path": keys_path, "reason": e }),
        ),
    };

    // ── Keys actions — no skill discovery needed ──────────────────────────────
    if has_flag(&args, "--keys-list") {
        cmd_keys_list(&keys);
        return;
    }
    if has_flag(&args, "--keys-add") {
        let id  = flag(&args, "--id") .unwrap_or_else(|| cli_err(
            "--keys-add requires --id",
            "Usage: suckless-mcp --keys-add --id ID --key KEY",
            json!({}),
        ));
        let key = flag(&args, "--key").unwrap_or_else(|| cli_err(
            "--keys-add requires --key",
            "Usage: suckless-mcp --keys-add --id ID --key KEY",
            json!({}),
        ));
        cmd_keys_add(&keys_path, id, key);
        return;
    }
    if has_flag(&args, "--keys-revoke") {
        let id = flag(&args, "--id").unwrap_or_else(|| cli_err(
            "--keys-revoke requires --id",
            "Usage: suckless-mcp --keys-revoke --id ID",
            json!({}),
        ));
        cmd_keys_revoke(&keys_path, id);
        return;
    }

    // ── Discover skills ───────────────────────────────────────────────────────
    let host_env: HashMap<String, String> = std::env::vars().collect();
    let discovered = discover_skills(&host_env);

    // ── Skills and status actions ─────────────────────────────────────────────
    if has_flag(&args, "--status") {
        cmd_status(&config, &discovered, &keys);
        return;
    }
    if has_flag(&args, "--skills") {
        cmd_skills(&discovered, flag(&args, "--name"));
        return;
    }

    // ── Unknown flag check ────────────────────────────────────────────────────
    // Any unrecognised --flag is an error rather than silently falling through to serve.
    let known = &[
        "--serve", "--status", "--skills",
        "--keys-list", "--keys-add", "--keys-revoke",
        "--config", "--id", "--key", "--name", "--help", "-h",
    ];
    for a in &args {
        if a.starts_with("--") && !known.iter().any(|k| a == k || a.starts_with(&format!("{k}="))) {
            cli_err(
                &format!("Unknown flag: {a}"),
                "Run 'suckless-mcp --help' to see all flags.",
                json!({ "flag": a }),
            );
        }
    }

    // ── --serve (default action) ──────────────────────────────────────────────
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_env("LOG_LEVEL")
                .add_directive("suckless_mcp=info".parse().unwrap()),
        )
        .init();

    let skills = preflight(discovered.skills);

    if skills.is_empty() {
        eprintln!("FATAL: no valid skills registered. Fix errors above and restart.");
        std::process::exit(1);
    }

    let active_keys: Vec<ApiKey> = keys.keys.into_iter().filter(|k| k.active).collect();
    if active_keys.is_empty() {
        eprintln!("FATAL: no active keys in keys.toml. Add one with '--keys-add --id ID --key KEY'.");
        std::process::exit(1);
    }

    let state = AppState {
        keys:   Arc::new(active_keys),
        skills: Arc::new(skills),
        sem:    Arc::new(Semaphore::new(config.max_concurrent_tools)),
    };

    let app = Router::new()
        .route("/mcp",    post(mcp_handler))
        .route("/health", get(health_handler))
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware))
        .with_state(state);

    let addr = format!("{}:{}", config.listen_host, config.listen_port);

    info!("suckless-mcp v{} listening on {addr}", env!("CARGO_PKG_VERSION"));
    info!("skills_root          = {SKILLS_ROOT}");
    info!("max_concurrent_tools = {}", config.max_concurrent_tools);

    let listener = TcpListener::bind(&addr).await
        .unwrap_or_else(|e| panic!("cannot bind {addr}: {e}"));

    axum::serve(listener, app).await
        .expect("server error");
}
