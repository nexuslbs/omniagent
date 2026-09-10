//! Prebuilt-binary plugin artifacts (the `binary` field of `plugin.json`).
//!
//! Some plugins ship as a downloadable executable instead of source code:
//! there is nothing to compile, so `install-git` stays clone-only and the
//! plugin INSTALL action downloads the artifact declared in the manifest and
//! places it (executable) in the plugin directory. `entrypoint.command` then
//! starts the MCP server from that local file.
//!
//! Guarantees implemented here:
//! - platform/arch selection (`{os}`/`{arch}`/`{goarch}`/`{version}` templates
//!   or an explicit `assets` map keyed by `<os>-<arch>`),
//! - optional SHA-256 verification (fails closed: a mismatch never lands a file),
//! - archive support (`tar.gz`/`tgz`/`zip`) so GitHub Release assets work,
//! - bounded download (connect/total timeout, max size) and redirect following,
//! - atomic placement (temp file + rename, `0755`): a failed install never
//!   leaves a partial binary behind,
//! - repeatable/idempotent installs (a re-install re-fetches and replaces).

use crate::err_msg;
use crate::error::{AppResult, ErrorContext};
use crate::plugin::{PluginBinary, PluginManifest};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Hard cap for a downloaded artifact: 256 MiB.
pub const MAX_ARTIFACT_BYTES: usize = 256 * 1024 * 1024;

/// Archive format of the remote artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryFormat {
    /// The artifact IS the executable.
    Raw,
    /// gzip-compressed tarball (`.tar.gz` / `.tgz`).
    TarGz,
    /// zip archive (`.zip`).
    Zip,
}

impl BinaryFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            BinaryFormat::Raw => "raw",
            BinaryFormat::TarGz => "tar.gz",
            BinaryFormat::Zip => "zip",
        }
    }

    /// Parse a declared format string.
    pub fn parse(s: &str) -> Option<BinaryFormat> {
        match s.trim().to_lowercase().as_str() {
            "raw" | "binary" | "bin" => Some(BinaryFormat::Raw),
            "tar.gz" | "tgz" | "gz" => Some(BinaryFormat::TarGz),
            "zip" => Some(BinaryFormat::Zip),
            _ => None,
        }
    }

    /// Infer the format from an artifact URL (query string ignored).
    pub fn infer(url: &str) -> BinaryFormat {
        let path = url.split(['?', '#']).next().unwrap_or(url).to_lowercase();
        if path.ends_with(".tar.gz") || path.ends_with(".tgz") {
            BinaryFormat::TarGz
        } else if path.ends_with(".zip") {
            BinaryFormat::Zip
        } else {
            BinaryFormat::Raw
        }
    }
}

/// The artifact selected for the running platform.
#[derive(Debug, Clone)]
pub struct ResolvedBinaryArtifact {
    pub url: String,
    pub checksum: Option<String>,
    /// Installed file name inside the plugin directory.
    pub file_name: String,
    /// Member file name inside the archive (unused for `Raw`).
    pub member: String,
    pub format: BinaryFormat,
    pub version: Option<String>,
}

/// A binary that is present in the plugin directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledBinary {
    pub path: String,
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
    pub size: u64,
}

/// Observable install status of a plugin's declared binary (API/dashboard).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginBinaryStatus {
    /// The manifest declares a `binary` artifact.
    pub declared: bool,
    /// The declared file exists in the plugin directory.
    pub installed: bool,
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_checksum: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// OS component of the platform key (matches Go/GitHub release naming).
pub fn os_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "windows",
        _ => "linux",
    }
}

/// Architecture component of the platform key (Rust naming).
pub fn arch_name() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" | "amd64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        other => other,
    }
}

/// Architecture in Go/GitHub-release naming (`amd64`/`arm64`).
pub fn go_arch_name() -> &'static str {
    match arch_name() {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

/// The platform key used by `assets`/`checksums` maps: `<os>-<arch>`.
pub fn platform_key() -> String {
    format!("{}-{}", os_name(), arch_name())
}

/// Substitute the supported placeholders in a URL template.
fn expand_placeholders(template: &str, version: Option<&str>) -> String {
    let mut out = template.to_string();
    if let Some(v) = version {
        out = out.replace("{version}", v);
    }
    out.replace("{os}", os_name())
        .replace("{arch}", arch_name())
        .replace("{goarch}", go_arch_name())
        .replace("{os_go}", os_name())
}

/// Reject file names that could escape the plugin directory.
fn safe_file_name(name: &str) -> AppResult<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        err_msg!("Binary artifact declares an empty file name");
    }
    if trimmed.contains('/') || trimmed.contains('\\') || trimmed == "." || trimmed == ".." {
        err_msg!(
            "Binary artifact file name '{}' must be a plain file name (no path separators)",
            name
        );
    }
    Ok(trimmed.to_string())
}

/// Normalize an expected checksum to lowercase hex (accepts `sha256:<hex>`).
pub fn normalize_checksum(raw: &str) -> Option<String> {
    let value = raw.trim();
    let value = value
        .strip_prefix("sha256:")
        .or_else(|| value.strip_prefix("SHA256:"))
        .unwrap_or(value)
        .trim();
    if value.is_empty() || !value.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(value.to_lowercase())
}

/// Hex-encoded SHA-256 of a byte buffer.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

/// Verify a SHA-256 checksum; the error names both values so the API/dashboard
/// can surface an actionable message.
pub fn verify_checksum(bytes: &[u8], expected: &str) -> Result<(), String> {
    let expected = normalize_checksum(expected).ok_or_else(|| {
        format!(
            "declared checksum '{}' is not a valid SHA-256 hex digest",
            expected
        )
    })?;
    let actual = sha256_hex(bytes);
    if actual != expected {
        return Err(format!(
            "checksum mismatch: expected sha256:{} got sha256:{}",
            expected, actual
        ));
    }
    Ok(())
}

/// Pick the artifact for the running platform and resolve its checksum.
pub fn resolve_artifact(
    bin: &PluginBinary,
    plugin_name: &str,
) -> AppResult<ResolvedBinaryArtifact> {
    let key = platform_key();

    let url = match bin.assets.as_ref().and_then(|m| m.get(&key)) {
        Some(explicit) if !explicit.trim().is_empty() => explicit.trim().to_string(),
        _ => {
            let template = match bin.url.as_deref() {
                Some(t) if !t.trim().is_empty() => t.trim(),
                _ => {
                    err_msg!(
                        "No binary artifact for platform '{}': declare a `url` template or an \
                         `assets` entry for this platform",
                        key
                    );
                }
            };
            if template.contains("{version}") && bin.version.is_none() {
                err_msg!(
                    "Binary artifact URL uses {{version}} but no `version` is declared: pin the \
                     version (e.g. \"version\": \"1.20.0\")"
                );
            }
            expand_placeholders(template, bin.version.as_deref())
        }
    };

    if !url.starts_with("https://") && !url.starts_with("http://") {
        err_msg!("Binary artifact URL '{}' must be an http(s) URL", url);
    }

    let checksum = bin
        .checksums
        .as_ref()
        .and_then(|m| m.get(&key))
        .cloned()
        .or_else(|| bin.checksum.clone());

    let format = match bin.format.as_deref() {
        Some(declared) => match BinaryFormat::parse(declared) {
            Some(f) => f,
            None => err_msg!(
                "Unsupported binary format '{}': use raw, tar.gz or zip",
                declared
            ),
        },
        None => BinaryFormat::infer(&url),
    };

    let member = bin
        .member
        .clone()
        .or_else(|| bin.file.clone())
        .unwrap_or_else(|| plugin_name.to_string());
    let file_name = bin.file.clone().unwrap_or_else(|| member.clone());

    Ok(ResolvedBinaryArtifact {
        url,
        checksum,
        file_name: safe_file_name(&file_name)?,
        member: safe_file_name(&member)?,
        format,
        version: bin.version.clone(),
    })
}

/// Status of a declared binary for the API/dashboard.
pub fn status_for(
    manifest: &PluginManifest,
    plugin_dir: Option<&str>,
) -> Option<PluginBinaryStatus> {
    let bin = manifest.binary.as_ref()?;
    let file = bin
        .file
        .clone()
        .or_else(|| bin.member.clone())
        .unwrap_or_else(|| manifest.name.clone());
    let expected_checksum = bin
        .checksums
        .as_ref()
        .and_then(|m| m.get(&platform_key()))
        .cloned()
        .or_else(|| bin.checksum.clone());

    let (installed, path, size) = match plugin_dir {
        Some(dir) => {
            let candidate = Path::new(dir).join(&file);
            match std::fs::metadata(&candidate) {
                Ok(meta) if meta.is_file() => (
                    true,
                    Some(candidate.to_string_lossy().to_string()),
                    Some(meta.len()),
                ),
                _ => (false, Some(candidate.to_string_lossy().to_string()), None),
            }
        }
        None => (false, None, None),
    };

    Some(PluginBinaryStatus {
        declared: true,
        installed,
        file,
        path,
        version: bin.version.clone(),
        expected_checksum,
        size,
    })
}

/// Find a file named `member` anywhere in an extracted tree (bounded depth).
fn find_member(root: &Path, member: &str) -> Option<PathBuf> {
    walkdir::WalkDir::new(root)
        .max_depth(4)
        .into_iter()
        .filter_map(|e| e.ok())
        .find(|e| e.file_type().is_file() && e.file_name().to_string_lossy() == member)
        .map(|e| e.into_path())
}

/// Extract an archive payload into `work_dir` and return the member path.
fn extract_member(
    payload: &Path,
    work_dir: &Path,
    format: BinaryFormat,
    member: &str,
) -> AppResult<PathBuf> {
    match format {
        BinaryFormat::Raw => Ok(payload.to_path_buf()),
        BinaryFormat::TarGz => {
            let status = std::process::Command::new("tar")
                .env_clear()
                .env("PATH", crate::process_env::MINIMAL_PATH)
                .arg("-xzf")
                .arg(payload)
                .arg("-C")
                .arg(work_dir)
                .status()
                .ctx("Failed to execute tar to extract the binary artifact")?;
            if !status.success() {
                err_msg!("tar failed to extract the binary artifact ({})", status);
            }
            match find_member(work_dir, member) {
                Some(found) => Ok(found),
                None => err_msg!("archive does not contain a file named '{}'", member),
            }
        }
        BinaryFormat::Zip => {
            let status = std::process::Command::new("unzip")
                .env_clear()
                .env("PATH", crate::process_env::MINIMAL_PATH)
                .arg("-o")
                .arg("-q")
                .arg(payload)
                .arg("-d")
                .arg(work_dir)
                .status()
                .ctx("Failed to execute unzip to extract the binary artifact")?;
            if !status.success() {
                err_msg!("unzip failed to extract the binary artifact ({})", status);
            }
            match find_member(work_dir, member) {
                Some(found) => Ok(found),
                None => err_msg!("archive does not contain a file named '{}'", member),
            }
        }
    }
}

/// Verify + extract + atomically place an already-downloaded artifact.
///
/// Split out from the network path so the placement/verification contract is
/// unit-testable: a checksum mismatch leaves NO file behind.
pub fn install_bytes(
    plugin_dir: &str,
    art: &ResolvedBinaryArtifact,
    bytes: &[u8],
) -> AppResult<InstalledBinary> {
    if bytes.is_empty() {
        err_msg!("Downloaded binary artifact is empty");
    }
    if let Some(expected) = art.checksum.as_deref() {
        if let Err(e) = verify_checksum(bytes, expected) {
            err_msg!("{}", e);
        }
    }

    let dir = Path::new(plugin_dir);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let work = dir.join(format!(".binary-install-{}", nanos));
    std::fs::create_dir_all(&work).ctx(format!("Failed to create work dir {}", work.display()))?;

    let payload = work.join("payload");
    std::fs::write(&payload, bytes).ctx("Failed to write artifact payload to the work dir")?;

    let extract_dir = work.join("extract");
    std::fs::create_dir_all(&extract_dir).ctx("Failed to create extraction dir")?;

    let result = (|| -> AppResult<InstalledBinary> {
        let member_path = extract_member(&payload, &extract_dir, art.format, &art.member)?;

        // Place atomically: temp file in the plugin dir, chmod, rename.
        let final_path = dir.join(&art.file_name);
        let tmp_path = dir.join(format!(".{}.tmp-{}", art.file_name, nanos));
        std::fs::copy(&member_path, &tmp_path).ctx(format!(
            "Failed to copy the extracted binary into {}",
            tmp_path.display()
        ))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o755))
                .ctx("Failed to make the installed binary executable")?;
        }
        let size = std::fs::metadata(&tmp_path).map(|m| m.len()).unwrap_or(0);
        if let Err(e) = std::fs::rename(&tmp_path, &final_path) {
            let _ = std::fs::remove_file(&tmp_path);
            err_msg!(
                "Failed to move the binary into place ({}): {}",
                final_path.display(),
                e
            );
        }

        Ok(InstalledBinary {
            path: final_path.to_string_lossy().to_string(),
            file: art.file_name.clone(),
            version: art.version.clone(),
            checksum: art.checksum.clone(),
            size,
        })
    })();

    if let Err(e) = std::fs::remove_dir_all(&work) {
        tracing::warn!(
            "Failed to remove binary install work dir {}: {:?}",
            work.display(),
            e
        );
    }

    result
}

/// Build the credential header value for an authenticated artifact.
fn auth_header_for(bin: &PluginBinary, token: &str) -> (String, String) {
    let header = bin
        .auth_header
        .clone()
        .unwrap_or_else(|| "Authorization".to_string());
    let scheme = bin
        .auth_scheme
        .clone()
        .unwrap_or_else(|| "Bearer ".to_string());
    (header, format!("{}{}", scheme, token))
}

/// Download the artifact (bounded, redirects followed).
pub async fn download_artifact(
    art: &ResolvedBinaryArtifact,
    bin: &PluginBinary,
    auth_token: Option<&str>,
) -> AppResult<Vec<u8>> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(300))
        .user_agent("omniagent-plugin-installer/1.0")
        .build()
        .ctx("Failed to build HTTP client")?;

    let mut request = client.get(&art.url);
    if let Some(token) = auth_token.filter(|t| !t.is_empty()) {
        let (header, value) = auth_header_for(bin, token);
        request = request.header(header, value);
    }

    let mut response = request.send().await.ctx(format!(
        "Failed to download the binary artifact from {} (network error / host unreachable)",
        art.url
    ))?;

    let status = response.status();
    if !status.is_success() {
        err_msg!(
            "Failed to download the binary artifact from {}: HTTP {} ({})",
            art.url,
            status.as_u16(),
            status.canonical_reason().unwrap_or("unknown")
        );
    }

    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await.ctx(format!(
        "Failed while reading the binary artifact body from {}",
        art.url
    ))? {
        if body.len() + chunk.len() > MAX_ARTIFACT_BYTES {
            err_msg!(
                "Binary artifact from {} exceeds the maximum size of {} MiB",
                art.url,
                MAX_ARTIFACT_BYTES / (1024 * 1024)
            );
        }
        body.extend_from_slice(&chunk);
    }

    if body.is_empty() {
        err_msg!("Binary artifact from {} is empty", art.url);
    }
    Ok(body)
}

/// Full install: resolve the platform artifact, download, verify, place.
pub async fn install_binary(
    plugin_dir: &str,
    plugin_name: &str,
    bin: &PluginBinary,
    auth_token: Option<String>,
) -> AppResult<InstalledBinary> {
    let art = resolve_artifact(bin, plugin_name)?;
    tracing::info!(
        "Binary install: plugin '{}' -> {} (format {}, file '{}')",
        plugin_name,
        art.url,
        art.format.as_str(),
        art.file_name
    );
    let bytes = download_artifact(&art, bin, auth_token.as_deref()).await?;
    install_bytes(plugin_dir, &art, &bytes)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// A manifest WITHOUT `binary`: the plugin must behave exactly as before.
    const PLAIN_MANIFEST: &str = r#"{
      "name": "plain-tool",
      "version": "0.1.0",
      "type": "mcp",
      "description": "source-built plugin",
      "entrypoint": { "command": "python3", "args": ["server.py"], "transport": "stdio" }
    }"#;

    /// A manifest WITH a `binary` declaration (engram, GitHub Releases asset).
    const BINARY_MANIFEST: &str = r#"{
      "name": "engram",
      "version": "1.20.0",
      "type": "mcp",
      "description": "prebuilt binary plugin",
      "entrypoint": { "command": "engram", "args": ["mcp"], "transport": "stdio" },
      "binary": {
        "url": "https://github.com/Gentleman-Programming/engram/releases/download/v{version}/engram_{version}_{os}_{goarch}.tar.gz",
        "version": "1.20.0",
        "file": "engram",
        "format": "tar.gz",
        "checksums": { "linux-x86_64": "sha256:7dc3003318e303bee269a4772144f3ce01c8ec700bfd524aaec76770acd389ca" },
        "auth": "$secret:ENGRAM_DOWNLOAD_TOKEN"
      }
    }"#;

    fn manifest(json: &str) -> PluginManifest {
        serde_json::from_str(json).expect("manifest json must parse")
    }

    fn temp_dir(tag: &str) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("omniagent-bin-{}-{}", tag, nanos));
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().to_string()
    }

    fn bin_decl(url: &str, checksum: Option<&str>) -> PluginBinary {
        let mut v = serde_json::json!({ "url": url, "file": "engram", "format": "raw" });
        if let Some(c) = checksum {
            v["checksum"] = serde_json::json!(c);
        }
        serde_json::from_value(v).unwrap()
    }

    /// Minimal one-shot HTTP server (bounded request count) for download tests.
    fn spawn_http_server(responses: Vec<Vec<u8>>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(&response);
                let _ = stream.flush();
            }
        });
        format!("http://{}", addr)
    }

    fn http_response(status: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            status,
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    // -- manifest acceptance -------------------------------------------------

    #[test]
    fn manifest_without_binary_is_unchanged() {
        let m = manifest(PLAIN_MANIFEST);
        assert!(m.binary.is_none(), "absent `binary` must stay None");
        assert!(status_for(&m, Some("/tmp/plain-tool")).is_none());
        let ep = m.entrypoint.expect("entrypoint preserved");
        assert_eq!(ep.command, "python3");
        assert_eq!(ep.args, vec!["server.py"]);
    }

    #[test]
    fn manifest_with_binary_parses_every_field() {
        let m = manifest(BINARY_MANIFEST);
        let bin = m.binary.expect("binary parsed");
        assert_eq!(bin.version.as_deref(), Some("1.20.0"));
        assert_eq!(bin.file.as_deref(), Some("engram"));
        assert_eq!(bin.format.as_deref(), Some("tar.gz"));
        assert_eq!(bin.auth.as_deref(), Some("$secret:ENGRAM_DOWNLOAD_TOKEN"));
        assert!(bin.url.as_deref().unwrap().contains("{goarch}"));
        assert!(bin.checksums.as_ref().unwrap().contains_key("linux-x86_64"));
    }

    // -- platform resolution -------------------------------------------------

    #[test]
    fn resolve_expands_platform_placeholders() {
        let m = manifest(BINARY_MANIFEST);
        let art = resolve_artifact(m.binary.as_ref().unwrap(), "engram").unwrap();
        assert!(art.url.starts_with("https://github.com/"), "{}", art.url);
        assert!(!art.url.contains('{'), "unexpanded placeholder: {}", art.url);
        assert!(art.url.contains(&format!("{}_{}", os_name(), go_arch_name())));
        assert!(art.url.contains("1.20.0"));
        assert_eq!(art.file_name, "engram");
        assert_eq!(art.member, "engram");
        assert_eq!(art.format, BinaryFormat::TarGz);
        assert_eq!(
            art.checksum.as_deref(),
            Some("sha256:7dc3003318e303bee269a4772144f3ce01c8ec700bfd524aaec76770acd389ca")
        );
    }

    #[test]
    fn resolve_assets_map_overrides_url_template() {
        let v = serde_json::json!({
            "url": "https://example.com/fallback.tar.gz",
            "file": "tool",
            "assets": { platform_key(): "https://example.com/exact.tar.gz" }
        });
        let bin: PluginBinary = serde_json::from_value(v).unwrap();
        let art = resolve_artifact(&bin, "tool").unwrap();
        assert_eq!(art.url, "https://example.com/exact.tar.gz");
    }

    #[test]
    fn resolve_missing_asset_is_informative() {
        let v = serde_json::json!({
            "file": "tool",
            "assets": { "plan9-x86_64": "https://example.com/plan9.tar.gz" }
        });
        let bin: PluginBinary = serde_json::from_value(v).unwrap();
        let err = format!("{}", resolve_artifact(&bin, "tool").unwrap_err());
        assert!(err.contains("No binary artifact for platform"), "{}", err);
        assert!(err.contains(&platform_key()), "{}", err);
    }

    #[test]
    fn resolve_version_placeholder_requires_pin() {
        let v = serde_json::json!({
            "url": "https://example.com/tool_{version}.tar.gz",
            "file": "tool"
        });
        let bin: PluginBinary = serde_json::from_value(v).unwrap();
        let err = format!("{}", resolve_artifact(&bin, "tool").unwrap_err());
        assert!(err.contains("no `version` is declared"), "{}", err);
    }

    #[test]
    fn resolve_rejects_unsafe_file_names() {
        let v = serde_json::json!({
            "url": "https://example.com/tool.tar.gz",
            "file": "../escape"
        });
        let bin: PluginBinary = serde_json::from_value(v).unwrap();
        let err = format!("{}", resolve_artifact(&bin, "tool").unwrap_err());
        assert!(err.contains("plain file name"), "{}", err);
    }

    #[test]
    fn resolve_rejects_non_http_url_and_bad_format() {
        let v = serde_json::json!({ "url": "file:///tmp/tool", "file": "tool" });
        let bin: PluginBinary = serde_json::from_value(v).unwrap();
        assert!(format!("{}", resolve_artifact(&bin, "tool").unwrap_err())
            .contains("must be an http(s) URL"));

        let v = serde_json::json!({
            "url": "https://example.com/tool.rar",
            "file": "tool",
            "format": "rar"
        });
        let bin: PluginBinary = serde_json::from_value(v).unwrap();
        assert!(format!("{}", resolve_artifact(&bin, "tool").unwrap_err())
            .contains("Unsupported binary format"));
    }

    #[test]
    fn format_inference_from_url() {
        assert_eq!(BinaryFormat::infer("https://x/y.tgz?a=1"), BinaryFormat::TarGz);
        assert_eq!(BinaryFormat::infer("https://x/y.tar.gz"), BinaryFormat::TarGz);
        assert_eq!(BinaryFormat::infer("https://x/y.zip"), BinaryFormat::Zip);
        assert_eq!(BinaryFormat::infer("https://x/y"), BinaryFormat::Raw);
    }

    // -- checksum ------------------------------------------------------------

    #[test]
    fn checksum_normalization_and_verification() {
        assert_eq!(normalize_checksum("SHA256:AbC").as_deref(), Some("abc"));
        assert_eq!(normalize_checksum("abc").as_deref(), Some("abc"));
        assert!(normalize_checksum("").is_none());
        assert!(normalize_checksum("not-hex!").is_none());

        let bytes = b"engram";
        let good = sha256_hex(bytes);
        assert!(verify_checksum(bytes, &good).is_ok());
        assert!(verify_checksum(bytes, &format!("sha256:{}", good)).is_ok());
        let err = verify_checksum(bytes, &"0".repeat(64)).unwrap_err();
        assert!(err.contains("checksum mismatch"), "{}", err);
        assert!(err.contains(&good), "actual checksum missing: {}", err);
    }

    // -- placement contract --------------------------------------------------

    #[test]
    fn install_bytes_places_executable_and_reinstall_replaces_it() {
        let dir = temp_dir("place");
        let art = resolve_artifact(&bin_decl("https://example.com/engram", None), "engram").unwrap();
        let first = install_bytes(&dir, &art, b"#!/bin/sh\necho first\n").unwrap();
        assert!(std::path::Path::new(&first.path).is_file());
        assert_eq!(first.file, "engram");
        assert_eq!(first.size, 21);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&first.path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755, "binary must be executable");
        }

        // Repeatable / idempotent: a second install re-fetches and replaces.
        let second = install_bytes(&dir, &art, b"#!/bin/sh\necho second\n").unwrap();
        assert_eq!(second.path, first.path);
        let content = std::fs::read_to_string(&second.path).unwrap();
        assert!(content.contains("second"), "{}", content);
        // No temp/work leftovers.
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with('.') || n == "payload")
            .collect();
        assert!(leftovers.is_empty(), "leftovers: {:?}", leftovers);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn checksum_mismatch_leaves_no_file() {
        let dir = temp_dir("mismatch");
        let art = resolve_artifact(
            &bin_decl("https://example.com/engram", Some(&"a".repeat(64))),
            "engram",
        )
        .unwrap();
        let err = format!("{}", install_bytes(&dir, &art, b"payload").unwrap_err());
        assert!(err.contains("checksum mismatch"), "{}", err);
        assert!(!std::path::Path::new(&dir).join("engram").exists());
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "no partial artifact or work dir may remain"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_artifact_is_rejected() {
        let dir = temp_dir("empty");
        let art =
            resolve_artifact(&bin_decl("https://example.com/engram", None), "engram").unwrap();
        let err = format!("{}", install_bytes(&dir, &art, b"").unwrap_err());
        assert!(err.contains("empty"), "{}", err);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_reports_declared_and_installed() {
        let mut m = manifest(BINARY_MANIFEST);
        let dir = temp_dir("status");
        let status = status_for(&m, Some(&dir)).unwrap();
        assert!(status.declared);
        assert!(!status.installed);
        std::fs::write(std::path::Path::new(&dir).join("engram"), b"bin").unwrap();
        let status = status_for(&m, Some(&dir)).unwrap();
        assert!(status.installed);
        assert_eq!(status.size, Some(3));
        // A manifest change is reflected straight from the manifest.
        m.binary = None;
        assert!(status_for(&m, Some(&dir)).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- network paths -------------------------------------------------------

    #[tokio::test]
    async fn http_404_is_reported_with_status_and_url() {
        let base = spawn_http_server(vec![http_response("404 Not Found", b"")]);
        let url = format!("{}/engram", base);
        let dir = temp_dir("404");
        let err = format!(
            "{}",
            install_binary(&dir, "engram", &bin_decl(&url, None), None)
                .await
                .unwrap_err()
        );
        assert!(err.contains("HTTP 404"), "{}", err);
        assert!(err.contains(&url), "{}", err);
        assert!(!std::path::Path::new(&dir).join("engram").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn network_failure_is_reported_with_url() {
        // Port 1 on localhost: nothing listens there -> connection refused.
        let url = "http://127.0.0.1:1/engram".to_string();
        let dir = temp_dir("netfail");
        let err = format!(
            "{}",
            install_binary(&dir, "engram", &bin_decl(&url, None), None)
                .await
                .unwrap_err()
        );
        assert!(err.contains(&url), "{}", err);
        assert!(err.contains("network error"), "{}", err);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn download_verify_and_install_round_trip() {
        let body = b"#!/bin/sh\necho engram\n".to_vec();
        let checksum = format!("sha256:{}", sha256_hex(&body));
        let base = spawn_http_server(vec![
            http_response("200 OK", &body),
            http_response("200 OK", &body),
        ]);
        let url = format!("{}/engram", base);
        let dir = temp_dir("roundtrip");
        let installed = install_binary(&dir, "engram", &bin_decl(&url, Some(&checksum)), None)
            .await
            .unwrap();
        assert_eq!(installed.file, "engram");
        assert_eq!(std::fs::read(&installed.path).unwrap(), body);
        // Install is repeatable (idempotent): a second run replaces the file.
        let again = install_binary(&dir, "engram", &bin_decl(&url, Some(&checksum)), None)
            .await
            .unwrap();
        assert_eq!(again.path, installed.path);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
