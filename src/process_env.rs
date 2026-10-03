//! Process-environment isolation for spawned children.
//!
//! omniagent must NEVER pass its ambient environment (which includes the
//! /opt/omni/.env vars loaded by the server, e.g. COMPOSE_PROJECT_NAME for the
//! production compose project) to spawned plugin/tool child processes. Every
//! child is spawned with an EMPTY environment plus ONLY explicitly passed vars:
//!
//! - the plugin's configured `env:` map (with `$env:` / `$secret:` refs
//!   resolved by the core because the config explicitly declared them), and
//! - an explicit `PATH` so the child can resolve its own grandchildren:
//!   Rust's `Command::new` resolves bare program names via `execvp` using the
//!   PARENT's environ `PATH`, so an env-cleared child would otherwise fail with
//!   ENOENT on its own spawns.
//!
//! No other variable is ever passed implicitly. There is NO whitelist.

/// Minimal fallback `PATH`, used ONLY when the parent process has no `PATH` at
/// all. It is never inherited - the child still receives exactly one
/// explicitly set `PATH`.
pub const MINIMAL_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// The `PATH` passed to every spawned child (audit HV-C2).
///
/// A fixed five-directory list is not portable: on a host whose toolchain
/// lives elsewhere (NixOS, `$HOME/.cargo/bin`, `/opt/homebrew/bin`, a Windows
/// host) the child cannot find `git`/`docker`/`cargo` and fails with a bare
/// "command not found" far from the cause. The repo already proved the list
/// insufficient by patching `$CARGO_HOME/bin` into it for the compile child.
///
/// The value is the PARENT's `PATH` (so the child sees the same toolchain the
/// server itself runs with), optionally extended with the operator setting
/// `child_extra_path` (APPENDED, so it adds directories without discarding the
/// parent's). Empty-environment isolation is preserved: the child still gets
/// exactly ONE explicitly set `PATH` and no other inherited variable.
pub fn child_path() -> String {
    let parent = std::env::var("PATH").ok();
    let extra = crate::runtime_settings::raw("child_extra_path");
    compose_child_path(parent.as_deref(), extra.as_deref())
}

/// Pure composition behind [`child_path`] (unit-testable without mutating the
/// process environment). An absent/blank parent `PATH` falls back to
/// [`MINIMAL_PATH`]; a blank `extra` is ignored.
pub fn compose_child_path(parent: Option<&str>, extra: Option<&str>) -> String {
    let base = parent
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .unwrap_or(MINIMAL_PATH);
    match extra.map(str::trim).filter(|e| !e.is_empty()) {
        Some(extra) => format!("{base}:{extra}"),
        None => base.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_path_is_appended_to_the_parent_path() {
        assert_eq!(
            compose_child_path(Some("/nix/store/bin:/usr/bin"), Some("/opt/extra/bin")),
            "/nix/store/bin:/usr/bin:/opt/extra/bin"
        );
    }

    #[test]
    fn blank_extra_path_leaves_the_parent_path_untouched() {
        assert_eq!(
            compose_child_path(Some("/usr/bin"), Some("   ")),
            "/usr/bin"
        );
    }

    #[test]
    fn missing_parent_path_falls_back_to_minimal_path() {
        assert_eq!(compose_child_path(None, None), MINIMAL_PATH);
        assert_eq!(compose_child_path(Some(""), None), MINIMAL_PATH);
    }

    /// The real child PATH is the parent's, and it is never empty.
    #[test]
    fn child_path_uses_the_parent_toolchain() {
        std::env::set_var("PATH", "/parent-only/bin");
        assert_eq!(child_path(), "/parent-only/bin");
    }
}
