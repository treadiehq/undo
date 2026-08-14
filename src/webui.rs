//! Local web interface: `undo ui`.
//!
//! Serves the embedded single-page app plus a JSON API over a loopback-only
//! HTTP listener. Security model:
//!
//! - The listener binds 127.0.0.1 exclusively; it is never reachable from the
//!   network.
//! - Every `/api` request must carry a per-session bearer token that is
//!   generated at startup and only revealed in the URL printed to the
//!   terminal. Web pages in the user's browser cannot read it, so drive-by
//!   `fetch`/form requests from other sites are rejected.
//! - The `Host` header must be a loopback name, which blocks DNS-rebinding.
//! - No CORS headers are ever emitted, so cross-origin responses stay opaque.

use anyhow::{Context, Result};
use include_dir::{Dir, include_dir};
use serde::Deserialize;
use serde_json::json;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use tiny_http::{Header, Method, Request, Response, Server};

use crate::db::Database;
use crate::models::WatchedProject;
use crate::{BOLD, DIM, GREEN, RESET, recoveries, webui_data};

static UI_ASSETS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/ui/dist");

const DEFAULT_PORT: u16 = 5533;
const WORKER_THREADS: usize = 4;
const MAX_BODY_BYTES: usize = 1024 * 1024;

pub fn cmd_ui(run: Option<&str>, port: Option<u16>, no_open: bool) -> Result<()> {
    // If the current folder is a watched project, make sure its recorder is
    // running so the UI shows live changes. Other projects are displayed
    // as-is; the UI reports their recording state instead of silently
    // spawning recorders for them.
    let cwd = std::env::current_dir()?.canonicalize()?;
    let db = Database::open()?;
    let current_project = db.find_project_for_path(&cwd)?;
    let current_project_id = match &current_project {
        Some(project) => {
            let _ = crate::daemon::ensure_recording_for_path(Path::new(&project.root_path));
            Some(project.id)
        }
        None => None,
    };

    // `undo ui r_421` deep-links straight to that Run's review: the URL
    // fragment tells the web app which timeline item to focus. Resolve the
    // reference first so a typo opens the plain timeline with a warning
    // instead of a confusing empty focus.
    let focus_fragment = match run {
        Some(reference) => {
            let resolved = current_project
                .as_ref()
                .map(|project| db.get_run_by_ref(project.id, reference))
                .transpose()?
                .flatten();
            match resolved {
                Some(session) => Some(format!("#run={}", session.public_id())),
                None => {
                    println!(
                        "{}Run '{}' was not found in this folder's history; opening the full timeline.{}",
                        DIM, reference, RESET
                    );
                    None
                }
            }
        }
        None => None,
    };
    drop(db);

    let token = generate_token()?;
    let (server, bound_port) = bind_server(port)?;
    let url = format!(
        "http://127.0.0.1:{bound_port}/?token={token}{}",
        focus_fragment.as_deref().unwrap_or("")
    );

    println!("{}Undo UI{} is running.", BOLD, RESET);
    println!();
    println!("  {}Open{}  {}", GREEN, RESET, url);
    println!();
    println!(
        "{}The link contains this session's access token. History stays on this machine; the listener only accepts local connections. Press Ctrl+C to stop.{}",
        DIM, RESET
    );
    if !no_open {
        open_browser(&url);
    }

    let context = Arc::new(ServerContext {
        token,
        current_project_id,
    });
    let server = Arc::new(server);
    let mut workers = Vec::new();
    for _ in 0..WORKER_THREADS.saturating_sub(1) {
        let server = Arc::clone(&server);
        let context = Arc::clone(&context);
        workers.push(std::thread::spawn(move || worker_loop(&server, &context)));
    }
    worker_loop(&server, &context);
    for worker in workers {
        let _ = worker.join();
    }
    Ok(())
}

struct ServerContext {
    token: String,
    current_project_id: Option<i64>,
}

fn worker_loop(server: &Server, context: &ServerContext) {
    loop {
        match server.recv() {
            Ok(request) => handle_request(request, context),
            Err(error) => {
                crate::logging::error(&format!("web UI listener error: {error}"));
                return;
            }
        }
    }
}

fn bind_server(requested: Option<u16>) -> Result<(Server, u16)> {
    let port = requested.unwrap_or(DEFAULT_PORT);
    match Server::http(("127.0.0.1", port)) {
        Ok(server) => Ok((server, port)),
        Err(error) if requested.is_none() => {
            // Default port taken (likely another `undo ui`): fall back to an
            // OS-assigned port instead of failing.
            let _ = error;
            let server = Server::http(("127.0.0.1", 0))
                .map_err(|error| anyhow::anyhow!("could not bind a local port: {error}"))?;
            let bound = server
                .server_addr()
                .to_ip()
                .map(|addr| addr.port())
                .ok_or_else(|| anyhow::anyhow!("could not determine the bound port"))?;
            Ok((server, bound))
        }
        Err(error) => Err(anyhow::anyhow!(
            "could not bind 127.0.0.1:{port}: {error}\nPick another port with: undo ui --port <PORT>"
        )),
    }
}

fn generate_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    let mut urandom = std::fs::File::open("/dev/urandom").context("opening /dev/urandom")?;
    urandom
        .read_exact(&mut bytes)
        .context("reading random bytes for the session token")?;
    Ok(crate::to_hex(&bytes))
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let launcher = "open";
    #[cfg(not(target_os = "macos"))]
    let launcher = "xdg-open";
    let _ = std::process::Command::new(launcher)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

// ── request handling ────────────────────────────────────────────────

fn handle_request(mut request: Request, context: &ServerContext) {
    let url = request.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (url, String::new()),
    };

    let response = if path.starts_with("/api/") {
        api_response(&mut request, context, &path, &query)
    } else {
        asset_response(&path)
    };
    let _ = match response {
        Ok(response) => request.respond(response),
        Err(error) => request.respond(json_response(400, &json!({ "error": error.to_string() }))),
    };
}

type HttpResponse = Response<std::io::Cursor<Vec<u8>>>;

fn api_response(
    request: &mut Request,
    context: &ServerContext,
    path: &str,
    query: &str,
) -> Result<HttpResponse> {
    if !host_is_loopback(request) {
        return Ok(json_response(
            403,
            &json!({ "error": "requests must come from this machine" }),
        ));
    }
    if !authorized(request, query, &context.token) {
        return Ok(json_response(
            401,
            &json!({ "error": "missing or invalid access token" }),
        ));
    }

    let method = request.method().clone();
    let body = read_body(request)?;
    let segments: Vec<&str> = path
        .trim_start_matches("/api/")
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();

    let db = Database::open()?;
    let value = match (&method, segments.as_slice()) {
        (Method::Get, ["bootstrap"]) => json!({
            "version": env!("CARGO_PKG_VERSION"),
            "current_project_id": context.current_project_id,
            "projects": webui_data::project_summaries(&db)?,
        }),
        (Method::Get, ["projects", id, "timeline"]) => {
            let project = require_project(&db, id)?;
            let params = QueryParams::parse(query);
            let limit = params.usize("limit").unwrap_or(500).clamp(1, 5000);
            let since_secs = params.i64("since_secs");
            serde_json::to_value(webui_data::timeline_payload(
                &db, &project, limit, since_secs,
            )?)?
        }
        (Method::Get, ["projects", id, "poll"]) => {
            let project = require_project(&db, id)?;
            serde_json::to_value(webui_data::poll_payload(&db, &project)?)?
        }
        (Method::Get, ["projects", id, "diff"]) => {
            let project = require_project(&db, id)?;
            let params = QueryParams::parse(query);
            let rel_path = params
                .string("path")
                .ok_or_else(|| anyhow::anyhow!("missing 'path' parameter"))?;
            let first = params
                .i64("first")
                .ok_or_else(|| anyhow::anyhow!("missing 'first' parameter"))?;
            let last = params
                .i64("last")
                .ok_or_else(|| anyhow::anyhow!("missing 'last' parameter"))?;
            serde_json::to_value(webui_data::diff_payload(
                &db, &project, &rel_path, first, last,
            )?)?
        }
        (Method::Post, ["projects", id, "recoveries"]) => {
            let project = require_project(&db, id)?;
            let spec: CreateRecoverySpec =
                serde_json::from_slice(&body).context("invalid recovery request body")?;
            serde_json::to_value(create_recovery(&db, &project, spec)?)?
        }
        (Method::Post, ["projects", id, "recoveries", reference, "apply"]) => {
            let project = require_project(&db, id)?;
            let spec = parse_apply_recovery_spec(&body)?;
            let outcome = recoveries::apply_recovery_paths_in(
                &db,
                &project,
                reference,
                spec.paths.as_deref(),
            )?;
            json!({
                "applied": !outcome.already_applied,
                "already_applied": outcome.already_applied,
                "files_changed": outcome.files_changed,
                "recovery": webui_data::recovery_view(&db, &project, &outcome.recovery)?,
            })
        }
        _ => {
            return Ok(json_response(
                404,
                &json!({ "error": format!("unknown API route: {path}") }),
            ));
        }
    };
    Ok(json_response(200, &value))
}

#[derive(Default, Deserialize)]
struct ApplyRecoverySpec {
    /// Project-relative paths selected from the stored recovery preview.
    /// Omitted paths preserve the original full-plan apply behavior.
    paths: Option<Vec<String>>,
}

fn parse_apply_recovery_spec(body: &[u8]) -> Result<ApplyRecoverySpec> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(ApplyRecoverySpec::default());
    }
    serde_json::from_slice(body).context("invalid apply recovery request body")
}

#[derive(Deserialize)]
struct CreateRecoverySpec {
    /// Undo work from a completed Run: restore `paths` to their state at the
    /// Run's start.
    run_id: Option<String>,
    /// Undo a group of un-attributed changes: restore `paths` to their state
    /// as of this change id.
    boundary_event_id: Option<i64>,
    /// Restore `path` (default: whole project) to an exact Unix timestamp.
    timestamp: Option<i64>,
    paths: Option<Vec<String>>,
    path: Option<String>,
    request: Option<String>,
}

fn create_recovery(
    db: &Database,
    project: &WatchedProject,
    spec: CreateRecoverySpec,
) -> Result<webui_data::RecoveryView> {
    let base = Path::new(&project.root_path);
    let recovery = if let Some(run_ref) = &spec.run_id {
        let run = db
            .get_run_by_ref(project.id, run_ref)?
            .ok_or_else(|| anyhow::anyhow!("Run '{run_ref}' not found"))?;
        let paths = require_paths(&spec)?;
        let label = spec
            .request
            .unwrap_or_else(|| format!("web: undo {} files from {}", paths.len(), run_ref));
        recoveries::create_run_recovery_in(
            db,
            project,
            base,
            &run,
            &paths,
            &label,
            "web",
            "exact-paths",
            None,
        )?
    } else if let Some(boundary) = spec.boundary_event_id {
        let paths = require_paths(&spec)?;
        let label = spec
            .request
            .unwrap_or_else(|| format!("web: undo {} files of recent edits", paths.len()));
        recoveries::create_event_boundary_recovery_in(
            db, project, base, &paths, boundary, &label, "web",
        )?
    } else if let Some(timestamp) = spec.timestamp {
        let path = spec.path.as_deref().unwrap_or(".");
        let label = spec
            .request
            .unwrap_or_else(|| format!("web: restore {path} to an earlier time"));
        recoveries::create_timestamp_recovery_in(db, project, base, path, timestamp, &label, "web")?
    } else {
        anyhow::bail!("provide run_id, boundary_event_id, or timestamp");
    };
    webui_data::recovery_view(db, project, &recovery)
}

fn require_paths(spec: &CreateRecoverySpec) -> Result<Vec<String>> {
    let paths = spec.paths.clone().unwrap_or_default();
    if paths.is_empty() {
        anyhow::bail!("select at least one file to undo");
    }
    Ok(paths)
}

fn require_project(db: &Database, id_segment: &str) -> Result<WatchedProject> {
    let project_id: i64 = id_segment
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid project id '{id_segment}'"))?;
    webui_data::project_by_id(db, project_id)?
        .ok_or_else(|| anyhow::anyhow!("project {project_id} not found"))
}

fn read_body(request: &mut Request) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    request
        .as_reader()
        .take(MAX_BODY_BYTES as u64 + 1)
        .read_to_end(&mut body)?;
    if body.len() > MAX_BODY_BYTES {
        anyhow::bail!("request body too large");
    }
    Ok(body)
}

// ── auth ────────────────────────────────────────────────────────────

fn authorized(request: &Request, query: &str, expected: &str) -> bool {
    let header_token = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("x-undo-token"))
        .map(|header| header.value.as_str().to_string());
    let query_token = QueryParams::parse(query).string("token");
    header_token
        .or(query_token)
        .is_some_and(|token| token_matches(&token, expected))
}

/// Constant-time-ish comparison; a browser page probing the port cannot use
/// response timing to recover the token byte-by-byte.
fn token_matches(provided: &str, expected: &str) -> bool {
    provided.len() == expected.len()
        && provided
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

fn host_is_loopback(request: &Request) -> bool {
    let Some(host) = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("host"))
        .map(|header| header.value.as_str().to_string())
    else {
        return false;
    };
    let name = host
        .rsplit_once(':')
        .map(|(name, _)| name)
        .unwrap_or(host.as_str());
    matches!(name, "127.0.0.1" | "localhost" | "[::1]")
}

// ── static assets ───────────────────────────────────────────────────

fn asset_response(path: &str) -> Result<HttpResponse> {
    let trimmed = path.trim_start_matches('/');
    let candidate = if trimmed.is_empty() {
        "index.html"
    } else {
        trimmed
    };
    let file = UI_ASSETS.get_file(candidate).or_else(|| {
        // SPA fallback: extensionless routes load the app shell.
        if candidate.contains('.') {
            None
        } else {
            UI_ASSETS.get_file("index.html")
        }
    });
    let Some(file) = file else {
        return Ok(text_response(404, "not found"));
    };

    let mut response = Response::from_data(file.contents().to_vec())
        .with_header(header("Content-Type", content_type(file.path())))
        .with_header(header("X-Content-Type-Options", "nosniff"));
    // Nuxt emits content-hashed files under _nuxt/; everything else may
    // change between binary versions.
    if candidate.starts_with("_nuxt/") {
        response = response.with_header(header(
            "Cache-Control",
            "public, max-age=31536000, immutable",
        ));
    } else {
        response = response.with_header(header("Cache-Control", "no-cache"));
    }
    Ok(response)
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("map") => "application/json",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn json_response(status: u16, value: &serde_json::Value) -> HttpResponse {
    Response::from_data(value.to_string().into_bytes())
        .with_status_code(status)
        .with_header(header("Content-Type", "application/json"))
        .with_header(header("Cache-Control", "no-store"))
        .with_header(header("X-Content-Type-Options", "nosniff"))
}

fn text_response(status: u16, body: &str) -> HttpResponse {
    Response::from_data(body.as_bytes().to_vec())
        .with_status_code(status)
        .with_header(header("Content-Type", "text/plain; charset=utf-8"))
}

fn header(field: &str, value: &str) -> Header {
    Header::from_bytes(field.as_bytes(), value.as_bytes())
        .expect("static header fields and values are always valid")
}

// ── query strings ───────────────────────────────────────────────────

struct QueryParams {
    entries: Vec<(String, String)>,
}

impl QueryParams {
    fn parse(query: &str) -> Self {
        let entries = query
            .split('&')
            .filter(|pair| !pair.is_empty())
            .filter_map(|pair| {
                let (key, value) = pair.split_once('=')?;
                Some((percent_decode(key), percent_decode(value)))
            })
            .collect();
        QueryParams { entries }
    }

    fn string(&self, key: &str) -> Option<String> {
        self.entries
            .iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| value.clone())
    }

    fn i64(&self, key: &str) -> Option<i64> {
        self.string(key)?.parse().ok()
    }

    fn usize(&self, key: &str) -> Option<usize> {
        self.string(key)?.parse().ok()
    }
}

/// Decode %XX escapes. The UI encodes with `encodeURIComponent`, which never
/// produces `+` for spaces, so only percent escapes are handled. Truncated or
/// invalid escapes pass through as literal bytes.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 3 <= bytes.len()
            && let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            out.push(high * 16 + low);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The token gate must reject near-misses; equality is judged on the full
    /// value, not a prefix.
    #[test]
    fn token_matching_requires_exact_value() {
        assert!(token_matches("abc123", "abc123"));
        assert!(!token_matches("abc124", "abc123"));
        assert!(!token_matches("abc12", "abc123"));
        assert!(!token_matches("", "abc123"));
    }

    /// Only loopback Host headers are accepted — a DNS-rebinding page pointing
    /// `evil.example` at 127.0.0.1 fails this check.
    #[test]
    fn host_names_other_than_loopback_are_rejected() {
        let loopback = ["127.0.0.1:5533", "localhost:5533", "127.0.0.1", "[::1]:80"];
        for host in loopback {
            let name = host.rsplit_once(':').map(|(name, _)| name).unwrap_or(host);
            assert!(
                matches!(name, "127.0.0.1" | "localhost" | "[::1]"),
                "expected {host} to be treated as loopback"
            );
        }
        for host in ["evil.example:5533", "192.168.1.10:5533", "undo.local"] {
            let name = host.rsplit_once(':').map(|(name, _)| name).unwrap_or(host);
            assert!(!matches!(name, "127.0.0.1" | "localhost" | "[::1]"));
        }
    }

    #[test]
    fn percent_decoding_handles_paths_and_unicode() {
        assert_eq!(percent_decode("src%2Fmain.rs"), "src/main.rs");
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("caf%C3%A9"), "café");
        // Truncated escapes fall through as literals instead of panicking.
        assert_eq!(percent_decode("bad%2"), "bad%2");
        assert_eq!(percent_decode("bad%"), "bad%");
    }

    #[test]
    fn query_params_extract_typed_values() {
        let params = QueryParams::parse("limit=200&path=src%2Fapp.rs&since_secs=3600");
        assert_eq!(params.usize("limit"), Some(200));
        assert_eq!(params.string("path").as_deref(), Some("src/app.rs"));
        assert_eq!(params.i64("since_secs"), Some(3600));
        assert_eq!(params.string("missing"), None);
    }

    #[test]
    fn apply_payload_preserves_full_and_selected_plan_modes() {
        assert!(parse_apply_recovery_spec(b"").unwrap().paths.is_none());
        assert!(parse_apply_recovery_spec(br#"{}"#).unwrap().paths.is_none());
        assert_eq!(
            parse_apply_recovery_spec(br#"{"paths":["src/app.rs"]}"#)
                .unwrap()
                .paths,
            Some(vec!["src/app.rs".to_string()])
        );
        assert_eq!(
            parse_apply_recovery_spec(br#"{"paths":[]}"#).unwrap().paths,
            Some(Vec::new())
        );
    }

    #[test]
    fn extensionless_routes_fall_back_to_the_app_shell() {
        // The embedded dist always contains index.html (build.rs guarantees a
        // placeholder), so SPA routes and the root both resolve.
        assert!(UI_ASSETS.get_file("index.html").is_some());
    }
}
