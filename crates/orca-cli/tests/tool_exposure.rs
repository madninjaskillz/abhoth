//! Ported from `ToolRegistryTests` (the `ToolExposure` part) and wiring checks for the optional
//! tool groups.

use orca_core::config::OrcaConfig;

fn advertised(config: &OrcaConfig) -> Vec<String> {
    orca_cli::build_registry(config)
        .definitions()
        .into_iter()
        .map(|d| d.name)
        .collect()
}

#[test]
fn tool_exposure_default_diet_trims_to_core() {
    let mut config = OrcaConfig::default();
    let names = advertised(&config);
    assert!(
        names.len() <= 31,
        "advertised {} tools: diet not applied",
        names.len()
    );
    for name in ["read_file", "bash"] {
        assert!(names.iter().any(|n| n == name), "{name}");
    }
    assert!(!names.iter().any(|n| n == "lsp_hover")); // lsp disabled by default
    assert!(!names.iter().any(|n| n == "generate_image")); // imagegen disabled by default

    config.lsp.enabled = true;
    assert!(advertised(&config).iter().any(|n| n == "lsp_hover"));

    config.tools.extended_tools = true;
    let registry = orca_cli::build_registry(&config);
    assert_eq!(registry.definitions().len(), registry.all().len());
}

#[test]
fn tool_exposure_image_tools_follow_image_gen_enabled_and_extra_tools() {
    let mut config = OrcaConfig::default();
    config.tools.extra_tools = vec!["CROP_IMAGE".into()];
    let names = advertised(&config);
    assert!(names.iter().any(|n| n == "crop_image"));
    assert!(!names.iter().any(|n| n == "resize_image"));

    config.image_gen.enabled = true;
    let names = advertised(&config);
    for name in ["generate_image", "edit_image", "crop_image", "resize_image"] {
        assert!(names.iter().any(|n| n == name), "{name}");
    }
}

#[test]
fn optional_tool_groups_are_registered_but_not_all_advertised() {
    let registry = orca_cli::build_registry(&OrcaConfig::default());
    for name in [
        "lsp_hover",
        "lsp_workspace_symbols",
        "generate_image",
        "crop_image",
    ] {
        assert!(registry.resolve(name).is_some(), "{name}");
    }
}
