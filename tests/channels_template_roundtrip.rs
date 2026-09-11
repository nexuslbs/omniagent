//! Gate (b) of the ONE thread-template chain
//! (`workflow_role > workflow > kanban_task > board > channel > profile`):
//! a CHANNEL carrying a `template` value must LOAD and SURVIVE SAVE - the
//! channels.yml writer must not drop the field (see
//! `profiles/omni/wiki/Projects/Omniagent/Field-Resolution.md`, section 5).
//!
//! The profile tier round-trip is covered by `src/profiles_yaml.rs` unit tests
//! and the workflow/role tiers by `src/workflows.rs` round-trip tests; this
//! closes the channel tier against the real public load/save functions.

use omniagent::channels_yaml::{load_channels_from, save_channels_file, ChannelDef, ChannelsFile};

/// Find the channels.yml the writer produced (data_dir/config/channels.yml,
/// with a shallow fallback for the flat layout).
fn find_channels_yml(dir: &std::path::Path) -> std::path::PathBuf {
    for candidate in [
        dir.join("config").join("channels.yml"),
        dir.join("channels.yml"),
    ] {
        if candidate.exists() {
            return candidate;
        }
    }
    panic!("channels.yml not written under {}", dir.display());
}

#[test]
fn channel_template_survives_save_and_reload() {
    let dir = std::env::temp_dir().join(format!("channels-tpl-rt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");

    let mut file = ChannelsFile::default();
    file.channels.insert(
        "tpl-rt".to_string(),
        ChannelDef {
            platform: Some("mattermost".to_string()),
            profile: Some("omni".to_string()),
            template: Some("chan-tpl-roundtrip".to_string()),
            ..Default::default()
        },
    );
    save_channels_file(dir.to_str().unwrap(), &file).expect("save channels.yml");

    // The writer must have emitted the field to disk (not silently dropped).
    let raw = std::fs::read_to_string(find_channels_yml(&dir)).expect("read channels.yml");
    assert!(
        raw.contains("template: chan-tpl-roundtrip"),
        "channels.yml must carry the template field, got:\n{raw}"
    );

    // And the reader must hand it back (load/save round-trip).
    let loaded = load_channels_from(dir.to_str().unwrap()).expect("reload channels.yml");
    assert_eq!(
        loaded.channels["tpl-rt"].template.as_deref(),
        Some("chan-tpl-roundtrip"),
        "channel template must survive the save/reload round-trip"
    );
}
