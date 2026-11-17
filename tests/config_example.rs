//! 示例配置必须能通过严格 schema 校验。
//!
//! 文档与示例是用户最先复制的东西；这里把它变成可执行检查，避免示例悄悄过期。

use scapegoat_bot::config::SbotConfig;
use scapegoat_bot::config::models::ModelsFile;

/// 带注释的完整样例；随部署 skill 一起编进二进制（`sbot docs sbot-deploy sbot.example.toml`）。
const EXAMPLE: &str = "skills/sbot-deploy/references/sbot.example.toml";

#[test]
fn config_example_is_valid() {
    let loaded = SbotConfig::load(EXAMPLE).expect("the example config must parse");
    loaded
        .config
        .validate_shape()
        .unwrap_or_else(|errors| panic!("{EXAMPLE} failed validation:\n{errors}"));

    let models = ModelsFile::embedded().expect("default models");
    loaded
        .config
        .validate_with_models(&models)
        .unwrap_or_else(|errors| panic!("{EXAMPLE} vs models:\n{errors}"));
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
fn documented_configs_are_valid() {
    // README 与部署 skill 里的 toml 代码块必须真的能用：直接从文档里抽出来测，不再维护一份
    // 内联副本（副本会和文档各自漂移）。每份文档的第一个代码块是“最小配置”，必须独立通过
    // 校验；后面的代码块是往配置里加的片段，接在最小配置后面也必须通过——配置是严格
    // schema，文档里写错一个键名，照着做的人就启动不了。
    let models = ModelsFile::embedded().expect("default models");
    // 代码块的数量写死：数量变了说明文档结构变了，上面的假设要跟着核对。
    for (doc, expected) in [("README.md", 1), ("skills/sbot-deploy/SKILL.md", 2)] {
        let text = std::fs::read_to_string(doc).unwrap();
        let blocks = toml_blocks(&text);
        assert_eq!(
            blocks.len(),
            expected,
            "{doc}: unexpected number of toml blocks"
        );
        let minimal = &blocks[0];
        for (index, block) in blocks.iter().enumerate() {
            let source = if index == 0 {
                minimal.clone()
            } else {
                format!("{minimal}\n{block}")
            };
            let config: SbotConfig = toml::from_str(&source).unwrap_or_else(|error| {
                panic!("{doc}: toml block {index} does not parse: {error}")
            });
            config
                .validate_shape()
                .unwrap_or_else(|errors| panic!("{doc}: toml block {index} invalid:\n{errors}"));
            config
                .validate_with_models(&models)
                .unwrap_or_else(|errors| panic!("{doc}: toml block {index} vs models:\n{errors}"));
        }
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
    let text = std::fs::read_to_string(EXAMPLE).unwrap();
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
    let text = std::fs::read_to_string(EXAMPLE).unwrap();
    let example: toml::Value = toml::from_str(&text).expect("the example config must parse");
    let defaults = toml::Value::try_from(SbotConfig::default()).expect("defaults serialize");
    let mut diffs = Vec::new();
    collect_differences("", &example, &defaults, &mut diffs);
    // 示例 monitor 是演示用的（默认没有 monitor），不算漂移。
    diffs.retain(|diff| !diff.starts_with("monitor:"));
    assert!(
        diffs.is_empty(),
        "{EXAMPLE} differs from the code defaults:\n{}",
        diffs.join("\n")
    );
}

/// `SBOT__*` 覆盖取自进程环境：给子进程设一个类型不对的值，`sbot doctor` 加载配置失败并指出
/// 是哪一项；换成合法的值就能加载。
///
/// 放在子进程里做，是因为改本进程的环境会被同时在跑的其它测试读到。
#[test]
fn the_binary_takes_overrides_from_its_environment() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("sbot.toml"), "version = 1\n").unwrap();
    let doctor = |max_steps: &str| {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_sbot"))
            .arg("doctor")
            .env("SBOT_HOME", home.path())
            .env("SBOT__AGENT__MAX_STEPS", max_steps)
            .output()
            .expect("sbot doctor runs");
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    };

    let rejected = doctor("many");
    assert!(rejected.contains("ERROR config"), "{rejected}");
    assert!(rejected.contains("agent.max_steps"), "{rejected}");

    let accepted = doctor("7");
    assert!(!accepted.contains("ERROR config"), "{accepted}");
    assert!(accepted.contains("config:"), "{accepted}");
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
