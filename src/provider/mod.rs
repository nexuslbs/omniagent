//! Provider plugin management — external subprocess providers and registry.
//!
//! Provider plugins can be either:
//! 1. HTTP-based — omniagent calls their API directly (configured via `base_url`)
//! 2. Subprocess-based — omniagent spawns them as child processes and
//!    communicates via JSON-lines over stdio (configured via `entrypoint`)

pub mod external;
pub mod registry;
