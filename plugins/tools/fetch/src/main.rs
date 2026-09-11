//! mcp-server-fetch: standalone MCP server for HTTP requests.
//! Communicates via stdio JSON-RPC (MCP protocol).
//!
//! Tools: fetch
//!
//! Config (every value reaches the plugin subprocess as the env var named after
//! the plugin.json `config_schema` key - see `apply_config_schema_defaults` in
//! src/mcp/external/config.rs; the uppercase spelling is accepted as a fallback
//! when launched from a plain docker-compose env):
//!
//!   - `allow_unsafe_methods` (default "false"): when false, only
//!     SAFE/read-only methods are allowed (GET, HEAD, OPTIONS); when true,
//!     all reqwest-supported methods (POST, PUT, PATCH, DELETE, ...) are
//!     allowed.
//!   - `allow_custom_headers` (default "false"): when false, the optional
//!     `headers` argument is rejected with an informative error; when true,
//!     the agent may define custom HTTP headers per call.
//!   - `database_url` (schema default `$env:DATABASE_URL`, resolved by the
//!     core from its own environment): Postgres URL of the omniagent secrets
//!     store, used to expand `$secret:NAME` references in header values at
//!     call time. An empty value disables `$secret:` expansion (the tool then
//!     reports an explicit error naming the secret and this config key).
//!
//! Header values support reference expansion AT CALL TIME:
//!
//!   - `$env:VAR`     -> the value of VAR in the plugin process environment
//!   - `$secret:NAME` -> `secrets.current_value` for NAME in the omniagent DB
//!
//! References may be embedded ("Basic $secret:TWILIO_BASIC"), so a credential
//! never has to appear literally in the tool call, the tool result or the logs.
//! Values expanded from `$secret:` are REDACTED from the tool result and from
//! every error message; header VALUES are never logged.

use anyhow::Result;
use mcp_server_util::*;
use reqwest::header::{HeaderName, HeaderValue};
use serde_json::Value;
use sqlx::Connection;

/// Methods that are always allowed (safe / read-only).
const SAFE_METHODS: [&str; 3] = ["GET", "HEAD", "OPTIONS"];

/// Maximum length of a single header value (bytes).
const MAX_HEADER_VALUE_BYTES: usize = 8 * 1024;
/// Maximum combined length of all custom headers (bytes).
const MAX_HEADER_TOTAL_BYTES: usize = 16 * 1024;
/// Maximum number of custom headers per call.
const MAX_HEADER_COUNT: usize = 32;

// ---------------------------------------------------------------------------
// Config (env-var based, read at every call so config reloads take effect)
// ---------------------------------------------------------------------------

/// Read a boolean config value from the first present env var in `keys`.
fn bool_config(keys: &[&str]) -> bool {
    for key in keys {
        if let Ok(raw) = std::env::var(key) {
            let t = raw.trim().to_ascii_lowercase();
            return t == "true" || t == "1" || t == "yes";
        }
    }
    false
}

/// Parse the `allow_unsafe_methods` config from the environment (default false).
fn allow_unsafe_methods_from_env() -> bool {
    bool_config(&["allow_unsafe_methods", "ALLOW_UNSAFE_METHODS"])
}

/// Parse the `allow_custom_headers` config from the environment (default false).
fn allow_custom_headers_from_env() -> bool {
    bool_config(&["allow_custom_headers", "ALLOW_CUSTOM_HEADERS"])
}

/// Parse the `database_url` config from the environment.
///
/// Returns an empty string when unset OR when the core could not resolve a
/// `$env:`/`$secret:` reference (an unresolved reference must not be used as a
/// connection string).
fn database_url_from_env() -> String {
    for key in ["database_url", "DATABASE_URL"] {
        if let Ok(raw) = std::env::var(key) {
            let value = raw.trim().to_string();
            if value.is_empty() {
                continue;
            }
            if value.starts_with("$env:") || value.starts_with("$secret:") {
                continue;
            }
            return value;
        }
    }
    String::new()
}

/// Decide whether `method` may be sent under the given config.
///
/// Safe methods (GET/HEAD/OPTIONS) are always allowed; any other method is
/// allowed only when `allow_unsafe` is true (reqwest validates the final
/// method string, so unknown methods still fail cleanly).
fn method_allowed(method: &str, allow_unsafe: bool) -> bool {
    let upper = method.trim().to_ascii_uppercase();
    if SAFE_METHODS.contains(&upper.as_str()) {
        return true;
    }
    allow_unsafe
}

// ---------------------------------------------------------------------------
// Call-time reference expansion ($env:VAR / $secret:NAME)
// ---------------------------------------------------------------------------

/// Characters allowed in a reference name (`$env:NAME`, `$secret:NAME`).
fn is_ref_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.'
}

/// Secret store access for one tool call.
///
/// A single lazy Postgres connection is opened per call (only when a
/// `$secret:` reference is actually expanded) and every expanded value is
/// remembered so the result can be scrubbed before it is returned.
struct Secrets {
    database_url: String,
    conn: Option<sqlx::postgres::PgConnection>,
    /// (secret_name, value) pairs expanded during this call - used for redaction.
    expanded: Vec<(String, String)>,
}

impl Secrets {
    fn new(database_url: String) -> Self {
        Self {
            database_url,
            conn: None,
            expanded: Vec::new(),
        }
    }

    /// Resolve a secret by name (DB lookup, cached for this call).
    ///
    /// Error messages never contain the secret value.
    async fn lookup(&mut self, name: &str) -> std::result::Result<String, String> {
        if let Some((_, value)) = self.expanded.iter().find(|(n, _)| n == name) {
            return Ok(value.clone());
        }
        if self.database_url.is_empty() {
            return Err(format!(
                "Error: the header value references $secret:{name}, but the fetch plugin has no \
                 `database_url` configured to resolve secrets. Set `database_url` in the fetch \
                 plugin config (it defaults to $env:DATABASE_URL, the URL the omniagent server \
                 itself uses) or use $env:VAR / a literal value instead. The secret value is \
                 never read from or written to the tool call."
            ));
        }
        if self.conn.is_none() {
            let conn = sqlx::postgres::PgConnection::connect(&self.database_url)
                .await
                .map_err(|_| {
                    format!(
                        "Error: could not connect to the omniagent database to resolve \
                         $secret:{name}. Check the fetch plugin `database_url` config."
                    )
                })?;
            self.conn = Some(conn);
        }
        let row: Option<String> =
            sqlx::query_scalar::<_, String>("SELECT current_value FROM secrets WHERE name = $1")
                .bind(name)
                .fetch_optional(self.conn.as_mut().expect("connection established above"))
                .await
                .map_err(|_| {
                    format!(
                        "Error: failed to read secret '{name}' from the omniagent secrets store \
                 (check the fetch plugin `database_url` config)."
                    )
                })?;
        match row {
            None => Err(format!(
                "Error: secret '{name}' was not found in the omniagent secrets store. Create it \
                 (secrets API: POST /secrets) before referencing $secret:{name} in a header value."
            )),
            Some(value) if value.is_empty() => Err(format!(
                "Error: secret '{name}' exists in the omniagent secrets store but its value is \
                 EMPTY; store the real value before referencing $secret:{name} in a header value."
            )),
            Some(value) => {
                self.expanded.push((name.to_string(), value.clone()));
                Ok(value)
            }
        }
    }
}

/// Expand `$env:VAR` and `$secret:NAME` references inside one header value.
///
/// References may appear anywhere in the value, not only at the start.
async fn expand_value(input: &str, secrets: &mut Secrets) -> std::result::Result<String, String> {
    if !input.contains('$') {
        return Ok(input.to_string());
    }
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] != '$' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        // $env: / $secret: (longest prefix match: $secret: is 8 chars, $env: is 5)
        let rest: String = chars[i..].iter().collect();
        let (kind, name_start) = if rest.starts_with("$secret:") {
            ("secret", i + 8)
        } else if rest.starts_with("$env:") {
            ("env", i + 5)
        } else {
            out.push(chars[i]);
            i += 1;
            continue;
        };
        let mut j = name_start;
        while j < chars.len() && is_ref_char(chars[j]) {
            j += 1;
        }
        if j == name_start {
            return Err(format!(
                "Error: header value contains '{kind}:' without a name. Use ${kind}:NAME."
            ));
        }
        let name: String = chars[name_start..j].iter().collect();
        if kind == "env" {
            match std::env::var(&name) {
                Ok(value) if !value.is_empty() => out.push_str(&value),
                Ok(_) => {
                    return Err(format!(
                        "Error: $env:{name} is set but EMPTY. Set it to a non-empty value in the \
                         plugin env map / deployment environment before using it in a header value."
                    ))
                }
                Err(_) => {
                    return Err(format!(
                    "Error: $env:{name} is not visible to the fetch plugin (plugin children run \
                         with an isolated environment: only the plugin config/env map is passed). \
                         Add {name} to the plugin env or use $secret:{name} instead."
                ))
                }
            }
        } else {
            let value = secrets.lookup(&name).await?;
            out.push_str(&value);
        }
        i = j;
    }
    Ok(out)
}

/// Replace every expanded `$secret:` VALUE found in `text` with a redaction
/// marker, so a secret never reaches the tool result or an error message.
fn scrub(text: &str, expanded: &[(String, String)]) -> String {
    let mut out = text.to_string();
    for (name, value) in expanded {
        if value.is_empty() {
            continue;
        }
        out = out.replace(value.as_str(), &format!("***REDACTED($secret:{name})***"));
    }
    out
}

// ---------------------------------------------------------------------------
// Custom header validation
// ---------------------------------------------------------------------------

/// Validate one header name (RFC 7230 token, no CR/LF, bounded length).
fn validate_header_name(name: &str) -> std::result::Result<(), String> {
    if name.is_empty() {
        return Err("Error: header names must not be empty.".to_string());
    }
    if name.len() > 256 {
        return Err(format!(
            "Error: header name '{}…' is too long (max 256 bytes).",
            &name[..32.min(name.len())]
        ));
    }
    if name.contains('\r') || name.contains('\n') {
        return Err(format!(
            "Error: header name '{}' contains a CR/LF character. Header injection is not allowed.",
            name.replace(['\r', '\n'], "?")
        ));
    }
    HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
        format!(
            "Error: invalid header name '{name}' (must be an RFC 7230 token: no spaces, \
             separators or control characters)."
        )
    })?;
    Ok(())
}

/// Validate a RAW (not yet expanded) header value.
///
/// Only structural safety is checked here - the final value is validated again
/// after `$env:`/`$secret:` expansion.
fn validate_raw_header_value(name: &str, value: &str) -> std::result::Result<(), String> {
    if value.contains('\r') || value.contains('\n') {
        return Err(format!(
            "Error: header '{name}' value contains a CR/LF character. Header injection / request \
             splitting is not allowed."
        ));
    }
    if value.len() > MAX_HEADER_VALUE_BYTES {
        return Err(format!(
            "Error: header '{name}' value is too large ({} bytes, max {}).",
            value.len(),
            MAX_HEADER_VALUE_BYTES
        ));
    }
    Ok(())
}

/// Parse + validate the `headers` argument into (name, raw value) pairs.
fn parse_headers(raw: &Value) -> std::result::Result<Vec<(String, String)>, String> {
    let obj = raw.as_object().ok_or_else(|| {
        "Error: the 'headers' argument must be a JSON object mapping header name to value, \
         e.g. {\"Authorization\": \"Basic $secret:TWILIO_BASIC\"}."
            .to_string()
    })?;
    if obj.len() > MAX_HEADER_COUNT {
        return Err(format!(
            "Error: too many custom headers ({} provided, max {}).",
            obj.len(),
            MAX_HEADER_COUNT
        ));
    }
    let mut total = 0usize;
    let mut pairs: Vec<(String, String)> = Vec::with_capacity(obj.len());
    for (name, value) in obj {
        validate_header_name(name)?;
        let value = value.as_str().ok_or_else(|| {
            format!("Error: header '{name}' must have a STRING value (got a JSON value).")
        })?;
        validate_raw_header_value(name, value)?;
        total += name.len() + value.len();
        pairs.push((name.clone(), value.to_string()));
    }
    if total > MAX_HEADER_TOTAL_BYTES {
        return Err(format!(
            "Error: custom headers are too large in total ({total} bytes, max {MAX_HEADER_TOTAL_BYTES})."
        ));
    }
    Ok(pairs)
}

/// Build the reqwest header list for this call: parse, expand, validate.
///
/// Returns `Err(message)` for a soft user error (already redacted).
async fn build_headers(
    raw: &Value,
    secrets: &mut Secrets,
) -> std::result::Result<Vec<(HeaderName, HeaderValue)>, String> {
    let pairs = parse_headers(raw)?;
    let mut out: Vec<(HeaderName, HeaderValue)> = Vec::with_capacity(pairs.len());
    for (name, raw_value) in pairs {
        let resolved = match expand_value(&raw_value, secrets).await {
            Ok(v) => v,
            Err(e) => return Err(scrub(&e, &secrets.expanded)),
        };
        let header_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| format!("Error: invalid header name '{name}'."))?;
        let header_value = HeaderValue::from_str(&resolved).map_err(|_| {
            format!(
                "Error: header '{name}' value is not a valid HTTP header value after expansion \
                 (control characters are not allowed)."
            )
        })?;
        out.push((header_name, header_value));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// fetch
// ---------------------------------------------------------------------------

/// Fetch a URL over HTTP(S).
///
/// Fully async (reqwest async client) with connect + total timeouts - a hung
/// upstream can NEVER block an async worker thread or wedge the plugin
/// runtime (Aug 2026 all-plugins-async push).
async fn handle_fetch(args: Value) -> Result<(String, bool)> {
    let url = args["url"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: 'url'"))?;

    let method = args["method"]
        .as_str()
        .unwrap_or("GET")
        .trim()
        .to_ascii_uppercase();

    // Gate non-safe methods BEFORE any request is sent.
    let allow_unsafe = allow_unsafe_methods_from_env();
    if !method_allowed(&method, allow_unsafe) {
        return Ok((
            format!(
                "Error: HTTP method '{}' is not allowed. The fetch plugin only allows \
                 safe/read-only methods (GET, HEAD, OPTIONS) unless the config \
                 `allow_unsafe_methods` is set to true (then POST, PUT, PATCH, \
                 DELETE, etc. are allowed).",
                method
            ),
            true,
        ));
    }

    // Optional custom headers (gated by allow_custom_headers, default false).
    let mut secrets = Secrets::new(database_url_from_env());
    let mut headers: Vec<(HeaderName, HeaderValue)> = Vec::new();
    let raw_headers = args.get("headers").filter(|v| !v.is_null());
    if let Some(raw) = raw_headers {
        let is_empty_object = raw.as_object().map(|o| o.is_empty()).unwrap_or(false);
        if is_empty_object {
            // An explicitly empty map is a no-op, allowed in every mode.
        } else if !allow_custom_headers_from_env() {
            return Ok((
                "Error: the 'headers' argument is disabled. The fetch plugin rejects custom HTTP \
                 headers unless the config `allow_custom_headers` is set to true (plugin config: \
                 plugins.yml `fetch.config.allow_custom_headers` or \
                 POST /api/plugins/tools/bundled/fetch/config with \
                 {\"config\":{\"allow_custom_headers\":\"true\"}}). Header values may reference \
                 $env:VAR and $secret:NAME."
                    .to_string(),
                true,
            ));
        } else {
            match build_headers(raw, &mut secrets).await {
                Ok(h) => headers = h,
                Err(e) => return Ok((e, true)),
            }
        }
    }

    let method_parsed = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|_| anyhow::anyhow!("Invalid HTTP method: '{}'", method))?;

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let mut request = client
        .request(method_parsed, url)
        .header("User-Agent", "OmniAgent/1.0");
    for (name, value) in &headers {
        request = request.header(name.clone(), value.clone());
    }

    let response = request.send().await?;

    let status = response.status();
    let body = response.text().await?;
    // Never echo an expanded secret value back to the agent.
    let body = scrub(&body, &secrets.expanded);

    // Truncate to ~50K chars
    let max_chars: usize = 50_000;
    let truncated = if body.len() > max_chars {
        format!(
            "{}\n\n[... truncated from {} to ~{} chars]",
            &body[..max_chars],
            body.len(),
            max_chars
        )
    } else {
        body
    };

    let text = format!(
        "HTTP {} {}\n\n{}",
        status.as_u16(),
        status.canonical_reason().unwrap_or(""),
        truncated
    );
    Ok((text, !status.is_success()))
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let fetch_handler: ToolHandler = Box::new(|args: Value, _meta: Option<McpMeta>| {
        Box::pin(async move { handle_fetch(args).await })
    });

    let tools = vec![McpToolEntry {
        def: McpToolDef {
            name: "fetch".to_string(),
            description:
                "FETCH/HTTP a URL from the internet (default method GET). Use this to download web pages, API responses, or any HTTP-accessible content. Optional 'method' argument: GET/HEAD/OPTIONS are always allowed; POST/PUT/PATCH/DELETE only when the plugin config allow_unsafe_methods=true. Optional 'headers' argument (JSON object header name -> value, ONLY when the plugin config allow_custom_headers=true): header values support $env:VAR and $secret:NAME references, expanded at call time, so credentials (e.g. \"Authorization\": \"Basic $secret:TWILIO_BASIC\") never appear literally in the tool call; values expanded from $secret: are redacted from the result, and CR/LF or oversize headers are rejected. Does NOT work with file:// URLs or local files: use filesystem__read for local files."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The URL to fetch"
                    },
                    "method": {
                        "type": "string",
                        "enum": ["GET", "HEAD", "OPTIONS", "POST", "PUT", "PATCH", "DELETE"],
                        "description": "HTTP method (default GET). Non-safe methods (POST/PUT/PATCH/DELETE) require config allow_unsafe_methods=true."
                    },
                    "headers": {
                        "type": "object",
                        "additionalProperties": { "type": "string" },
                        "description": "Optional custom HTTP headers as a JSON object (header name -> string value), e.g. {\"Authorization\": \"Basic $secret:TWILIO_BASIC\", \"Accept\": \"application/json\"}. Requires config allow_custom_headers=true; rejected with an informative error otherwise. Values may embed $env:VAR (plugin environment) and $secret:NAME (omniagent secrets store) references, expanded at call time. Names/values containing CR or LF are rejected (header injection), as are values >8 KiB, more than 32 headers or >16 KiB in total. Values expanded from $secret: are redacted from the tool result and logs."
                    }
                },
                "required": ["url"]
            }),
        },
        handler: fetch_handler,
    }];

    let server_info = ServerInfo {
        name: "mcp-server-fetch".to_string(),
        version: "0.1.0".to_string(),
    };

    run_server(server_info, tools).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_secrets() -> Secrets {
        Secrets::new(String::new())
    }

    #[test]
    fn safe_methods_always_allowed() {
        for m in ["GET", "HEAD", "OPTIONS", "get", "Head", "options"] {
            assert!(method_allowed(m, false), "{} should be allowed", m);
            assert!(method_allowed(m, true), "{} should be allowed", m);
        }
    }

    #[test]
    fn unsafe_methods_rejected_by_default() {
        for m in ["POST", "PUT", "PATCH", "DELETE"] {
            assert!(
                !method_allowed(m, false),
                "{} must be rejected when allow_unsafe_methods=false",
                m
            );
        }
    }

    #[test]
    fn unsafe_methods_allowed_when_configured() {
        for m in ["POST", "PUT", "PATCH", "DELETE", "post", "Put"] {
            assert!(method_allowed(m, true), "{} should be allowed", m);
        }
    }

    #[test]
    fn empty_or_unknown_methods() {
        assert!(!method_allowed("", false));
        assert!(!method_allowed("FOOBAR", false));
        // With allow_unsafe=true reqwest still rejects unknown methods at
        // parse time, but the gate itself lets them through.
        assert!(method_allowed("FOOBAR", true));
    }

    #[test]
    fn allow_unsafe_defaults_false_when_env_unset() {
        // Test process env does not define the var → default false.
        assert!(!allow_unsafe_methods_from_env());
    }

    #[test]
    fn allow_custom_headers_defaults_false_when_env_unset() {
        assert!(!allow_custom_headers_from_env());
    }

    #[test]
    fn database_url_never_returns_an_unresolved_ref() {
        std::env::set_var("database_url", "$env:DATABASE_URL");
        let value = database_url_from_env();
        assert!(!value.starts_with("$env:"), "{}", value);
        std::env::remove_var("database_url");
        let value = database_url_from_env();
        assert!(!value.starts_with("$env:"), "{}", value);
    }

    #[test]
    fn headers_argument_must_be_an_object() {
        let err = parse_headers(&json!("nope")).unwrap_err();
        assert!(err.contains("must be a JSON object"), "{}", err);
        let err = parse_headers(&json!(["nope"])).unwrap_err();
        assert!(err.contains("must be a JSON object"), "{}", err);
    }

    #[test]
    fn headers_values_must_be_strings() {
        let err = parse_headers(&json!({"X-Test": 42})).unwrap_err();
        assert!(err.contains("X-Test"), "{}", err);
        assert!(err.contains("STRING"), "{}", err);
    }

    #[test]
    fn crlf_in_header_name_or_value_is_rejected() {
        let err = parse_headers(&json!({"X-Test\r\nX-Injected": "v"})).unwrap_err();
        assert!(err.contains("CR/LF"), "{}", err);
        let err = parse_headers(&json!({"X-Test": "v\r\nX-Injected: evil"})).unwrap_err();
        assert!(err.contains("CR/LF"), "{}", err);
        let err = parse_headers(&json!({"X-Test\n": "v"})).unwrap_err();
        assert!(err.contains("CR/LF"), "{}", err);
        // Bare LF / CR too.
        let err = parse_headers(&json!({"X-Test": "v\nInjected: evil"})).unwrap_err();
        assert!(err.contains("CR/LF"), "{}", err);
    }

    #[test]
    fn invalid_header_name_is_rejected() {
        let err = parse_headers(&json!({"Bad Name": "v"})).unwrap_err();
        assert!(err.contains("invalid header name"), "{}", err);
        let err = parse_headers(&json!({"": "v"})).unwrap_err();
        assert!(err.contains("must not be empty"), "{}", err);
    }

    #[test]
    fn oversize_headers_are_rejected() {
        let big = "a".repeat(MAX_HEADER_VALUE_BYTES + 1);
        let err = parse_headers(&json!({"X-Big": big})).unwrap_err();
        assert!(err.contains("too large"), "{}", err);

        // Many small headers that together exceed the total budget.
        let mut map = serde_json::Map::new();
        for i in 0..MAX_HEADER_COUNT {
            map.insert(format!("X-{i}"), json!("b".repeat(1000)));
        }
        let err = parse_headers(&Value::Object(map)).unwrap_err();
        assert!(err.contains("too large in total"), "{}", err);

        // Too many headers.
        let mut map = serde_json::Map::new();
        for i in 0..(MAX_HEADER_COUNT + 1) {
            map.insert(format!("X-{i}"), json!("v"));
        }
        let err = parse_headers(&Value::Object(map)).unwrap_err();
        assert!(err.contains("too many custom headers"), "{}", err);
    }

    #[tokio::test]
    async fn literal_values_pass_through_unchanged() {
        let mut s = test_secrets();
        let out = expand_value("application/json", &mut s).await.unwrap();
        assert_eq!(out, "application/json");
    }

    #[tokio::test]
    async fn env_refs_are_expanded_embedded() {
        std::env::set_var("FETCH_TEST_TOKEN", "s3cr3t");
        let mut s = test_secrets();
        let out = expand_value("Basic $env:FETCH_TEST_TOKEN", &mut s)
            .await
            .unwrap();
        assert_eq!(out, "Basic s3cr3t");
        // Nothing from $env: is treated as a secret-store value.
        assert!(s.expanded.is_empty());
        std::env::remove_var("FETCH_TEST_TOKEN");
    }

    #[tokio::test]
    async fn unset_env_ref_is_an_informative_error() {
        let mut s = test_secrets();
        let err = expand_value("$env:FETCH_TEST_MISSING_VAR", &mut s)
            .await
            .unwrap_err();
        assert!(err.contains("FETCH_TEST_MISSING_VAR"), "{}", err);
        assert!(err.contains("not visible to the fetch plugin"), "{}", err);
    }

    #[tokio::test]
    async fn nameless_ref_is_rejected() {
        let mut s = test_secrets();
        let err = expand_value("$secret:", &mut s).await.unwrap_err();
        assert!(err.contains("without a name"), "{}", err);
    }

    #[tokio::test]
    async fn secret_ref_without_database_url_is_an_informative_error() {
        let mut s = test_secrets();
        let err = expand_value("Basic $secret:TWILIO_BASIC", &mut s)
            .await
            .unwrap_err();
        assert!(err.contains("$secret:TWILIO_BASIC"), "{}", err);
        assert!(err.contains("database_url"), "{}", err);
    }

    #[tokio::test]
    async fn expand_handles_multiple_refs_in_one_value() {
        std::env::set_var("FETCH_TEST_USER", "alice");
        std::env::set_var("FETCH_TEST_PASS", "hunter2");
        let mut s = test_secrets();
        let out = expand_value(
            "$env:FETCH_TEST_USER:$env:FETCH_TEST_PASS (token=$env:FETCH_TEST_USER)",
            &mut s,
        )
        .await
        .unwrap();
        assert_eq!(out, "alice:hunter2 (token=alice)");
        std::env::remove_var("FETCH_TEST_USER");
        std::env::remove_var("FETCH_TEST_PASS");
    }

    #[test]
    fn scrub_redacts_expanded_secret_values() {
        let expanded = vec![(
            "TWILIO_BASIC".to_string(),
            "QWxhZGRpbjpvcGVuIHNlc2FtZQ==".to_string(),
        )];
        let body = "{\"Authorization\": \"Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==\"}";
        let red = scrub(body, &expanded);
        assert!(!red.contains("QWxhZGRpbjpvcGVuIHNlc2FtZQ=="), "{}", red);
        assert!(
            red.contains("***REDACTED($secret:TWILIO_BASIC)***"),
            "{}",
            red
        );
        // Nothing to redact → unchanged.
        assert_eq!(scrub("plain", &[]), "plain");
    }

    #[tokio::test]
    async fn build_headers_expands_and_validates() {
        std::env::set_var("FETCH_TEST_HDR", "ok");
        let mut s = test_secrets();
        let pairs = build_headers(
            &json!({"X-Test": "v-$env:FETCH_TEST_HDR", "Accept": "application/json"}),
            &mut s,
        )
        .await
        .unwrap();
        assert_eq!(pairs.len(), 2);
        let x = pairs
            .iter()
            .find(|(n, _)| n.as_str() == "x-test")
            .expect("x-test header present");
        assert_eq!(x.1.to_str().unwrap(), "v-ok");
        std::env::remove_var("FETCH_TEST_HDR");
    }

    #[test]
    fn empty_header_object_is_parsed_to_no_headers() {
        assert!(parse_headers(&json!({})).unwrap().is_empty());
    }
}
