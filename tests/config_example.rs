//! 示例配置必须能通过严格 schema 校验。
//!
//! 文档与示例是用户最先复制的东西；这里把它变成可执行检查，避免示例悄悄过期。

use scapegoat_bot::config::SbotConfig;
use scapegoat_bot::config::models::ModelsFile;

#[test]
fn config_example_is_valid() {
    let loaded = SbotConfig::load("config.example.toml").expect("config.example.toml must parse");
    loaded
        .config
        .validate_shape()
        .unwrap_or_else(|errors| panic!("config.example.toml failed validation:\n{errors}"));

    let models =
        ModelsFile::parse(include_str!("../assets/models.default.json")).expect("default models");
    loaded
        .config
        .validate_with_models(&models)
        .unwrap_or_else(|errors| panic!("config.example.toml vs models:\n{errors}"));
}

#[test]
fn default_config_passes_shape_validation() {
    // 空 compaction_model 等默认值必须可用；曾经默认配置过不了校验。
    let config = SbotConfig::default();
    config
        .validate_shape()
        .unwrap_or_else(|errors| panic!("default config invalid:
{errors}"));
}

#[test]
fn readme_minimal_config_is_valid() {
    // README 的“最小可用配置”必须真的能启动。
    let text = r#"
version = 1

[agent]
model = "anthropic/claude-opus-5-5"
available_models = ["anthropic/claude-opus-5-5"]

[napcat]
url = "ws://127.0.0.1:3001"

[filter]
ignore_groups = []
"#;
    let config: SbotConfig = toml::from_str(text).expect("minimal config parses");
    config
        .validate_shape()
        .unwrap_or_else(|errors| panic!("minimal config invalid:
{errors}"));
    let models =
        ModelsFile::parse(include_str!("../assets/models.default.json")).expect("default models");
    config
        .validate_with_models(&models)
        .unwrap_or_else(|errors| panic!("minimal config vs models:
{errors}"));
}

#[test]
fn config_example_has_no_credentials() {
    let text = std::fs::read_to_string("config.example.toml").unwrap();
    assert!(!text.contains("sk-"), "no literal key may appear");
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        assert!(
            !line.starts_with("api_key") && !line.starts_with("access_token"),
            "credential-like assignment in example: {line}"
        );
    }
}
