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

    let models = ModelsFile::embedded().expect("default models");
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
        .unwrap_or_else(|errors| panic!("default config invalid:\n{errors}"));
}

#[test]
fn readme_minimal_configs_are_valid() {
    // README 与入门指南里“最小配置”的 toml 代码块必须真的能启动：直接从文档里抽出来测，
    // 不再维护一份内联副本（副本会和文档各自漂移）。
    let models = ModelsFile::embedded().expect("default models");
    for doc in ["README.md", "docs/guide/README.md"] {
        let text = std::fs::read_to_string(doc).unwrap();
        let blocks = toml_blocks(&text);
        // 两份文档都只有一个 toml 代码块，就是最小配置；数量变了说明文档结构变了，要跟着改。
        assert_eq!(blocks.len(), 1, "{doc}: expected exactly one toml block");
        let config: SbotConfig = toml::from_str(&blocks[0])
            .unwrap_or_else(|error| panic!("{doc}: minimal config does not parse: {error}"));
        config
            .validate_shape()
            .unwrap_or_else(|errors| panic!("{doc}: minimal config invalid:\n{errors}"));
        config
            .validate_with_models(&models)
            .unwrap_or_else(|errors| panic!("{doc}: minimal config vs models:\n{errors}"));
    }
}

/// 取出 Markdown 里所有 ```` ```toml ```` 代码块的内容。
fn toml_blocks(markdown: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current: Option<String> = None;
    for line in markdown.lines() {
        match current.as_mut() {
            // 代码块外：遇到 ```toml 开始收集。
            None if line.trim() == "```toml" => current = Some(String::new()),
            None => {}
            // 代码块内：遇到结束围栏就收尾，否则逐行追加。
            Some(_) if line.trim() == "```" => blocks.extend(current.take()),
            Some(block) => {
                block.push_str(line);
                block.push('\n');
            }
        }
    }
    blocks
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

#[test]
fn config_example_values_match_code_defaults() {
    // 示例写出的每个值都必须等于代码默认值：以前示例与代码各说各的（max_steps 48 / 64 等）。
    let text = std::fs::read_to_string("config.example.toml").unwrap();
    let example: toml::Value = toml::from_str(&text).expect("config.example.toml must parse");
    let defaults = toml::Value::try_from(SbotConfig::default()).expect("defaults serialize");
    let mut diffs = Vec::new();
    collect_differences("", &example, &defaults, &mut diffs);
    // 示例 monitor 是演示用的（默认没有 monitor），不算漂移。
    diffs.retain(|diff| !diff.starts_with("monitor:"));
    assert!(
        diffs.is_empty(),
        "config.example.toml differs from the code defaults:\n{}",
        diffs.join("\n")
    );
}

/// 逐键比较示例与默认值，把不同的叶子按 `a.b.c: example=… default=…` 记下来。
fn collect_differences(
    path: &str,
    example: &toml::Value,
    default: &toml::Value,
    out: &mut Vec<String>,
) {
    match (example, default) {
        // 两边都是表：逐键下钻；默认值里没有的键（`None` 不序列化）也算不同。
        (toml::Value::Table(example), toml::Value::Table(default)) => {
            for (key, value) in example {
                let child = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                match default.get(key) {
                    Some(default) => collect_differences(&child, value, default, out),
                    None => out.push(format!("{child}: example={value} default=<absent>")),
                }
            }
        }
        // 叶子（或类型不同）：直接比较。
        _ if example == default => {}
        _ => out.push(format!("{path}: example={example} default={default}")),
    }
}
