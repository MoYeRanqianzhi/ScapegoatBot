//! `Views` 的 config 部分。
//!
//! 由 `views.rs` 的 `list` / `read` / `write` 路由调用。读到的配置经过
//! [`redact`] 遮蔽（MCP 服务器的 `env` 值）。

mod redact;

use super::*;

impl Views {
    /// 把当前配置序列化成 TOML 表（密钥已遮蔽）；失败时视为视图不可用。
    pub(super) fn config_table(&self) -> Option<toml::Table> {
        let config = self.config_snapshot()?;
        let text = toml::to_string(&config).ok()?;
        let mut table = toml::from_str(&text).ok()?;
        redact::redact_table(&mut table);
        Some(table)
    }

    pub(super) fn list_config_root(&self) -> Vec<ViewEntry> {
        let mut entries: Vec<ViewEntry> = self
            .config_table()
            .map(|table| {
                table
                    .keys()
                    .map(|name| text_entry(&format!("/config/{name}")))
                    .collect()
            })
            .unwrap_or_default();
        entries.push(text_entry("/config/models.json"));
        entries
    }

    pub(super) fn list_config_section(&self, section: &str) -> Vec<ViewEntry> {
        if section == "models.json" {
            return Vec::new();
        }
        let Some(table) = self.config_table() else {
            return Vec::new();
        };
        let Some(value) = table.get(section) else {
            return Vec::new();
        };
        // 数组（如 [[monitor]]）作为叶子整体读取。
        let Some(section_table) = value.as_table() else {
            return Vec::new();
        };
        section_table
            .keys()
            .map(|key| text_entry(&format!("/config/{section}/{key}")))
            .collect()
    }

    pub(super) fn read_config_digest(&self) -> Result<ViewRead, VfsError> {
        let Some(table) = self.config_table() else {
            return Err(VfsError::Offline("/config".to_owned()));
        };
        let mut out = String::from("# Configuration\n");
        for (name, value) in &table {
            let kind = if value.is_table() { "table" } else { "leaf" };
            out.push_str(&format!("- /config/{name} ({kind})\n"));
        }
        out.push_str("- /config/models.json (read-only)\n");
        Ok(ViewRead::Text(out))
    }

    pub(super) fn read_config_section(&self, section: &str) -> Result<ViewRead, VfsError> {
        let table = self
            .config_table()
            .ok_or_else(|| VfsError::Offline("/config".to_owned()))?;
        let value = table
            .get(section)
            .ok_or_else(|| VfsError::NotFound(format!("/config/{section}")))?;
        let text = match value {
            toml::Value::Table(_) => toml::to_string(value).unwrap_or_else(|_| value.to_string()),
            _ => value.to_string(),
        };
        Ok(ViewRead::Text(text))
    }

    pub(super) fn read_config_value(&self, section: &str, key: &str) -> Result<ViewRead, VfsError> {
        let table = self
            .config_table()
            .ok_or_else(|| VfsError::Offline("/config".to_owned()))?;
        let value = table
            .get(section)
            .and_then(|section| section.get(key))
            .ok_or_else(|| VfsError::NotFound(format!("/config/{section}/{key}")))?;
        Ok(ViewRead::Text(value.to_string()))
    }

    /// `/config.toml`：`sbot.toml` 原文；凭据不在其中，MCP 服务器的 `env` 值已遮蔽。
    pub(super) fn read_config_toml(&self) -> Result<ViewRead, VfsError> {
        let path = self
            .context
            .config
            .as_ref()
            .map(|handle| handle.read().expect("config lock poisoned").path.clone())
            .ok_or_else(|| VfsError::Offline("/config.toml".to_owned()))?;
        let text =
            std::fs::read_to_string(&path).map_err(|error| VfsError::Storage(error.to_string()))?;
        let text = redact::redact_document(&text)
            .map_err(|error| VfsError::Storage(format!("cannot parse sbot.toml: {error}")))?;
        Ok(ViewRead::Text(text))
    }

    /// `/config/models.json`：只读模型定义。
    pub(super) fn read_models_json(&self) -> Result<ViewRead, VfsError> {
        let models = self
            .context
            .models
            .as_ref()
            .ok_or_else(|| VfsError::Offline("/config/models.json".to_owned()))?;
        Ok(ViewRead::Text(
            serde_json::to_string_pretty(models).unwrap_or_default(),
        ))
    }

    /// 写配置：只允许 `model_writable` 白名单；校验通过后原子写回，再换掉内存里的配置并
    /// 通知热更新。
    ///
    /// 支持三种形式：
    /// - `/config/<section>/<key>`：叶子值（TOML 字面量）；
    /// - `/config/<section>`：整段覆盖；
    /// - `/config.toml`：整文件，但只读键不得改变。
    pub(crate) fn write_config(
        &self,
        target: &ParsedTarget,
        content: &str,
    ) -> Result<String, VfsError> {
        let Some(handle) = &self.context.config else {
            return Err(VfsError::Offline(
                "configuration is not attached".to_owned(),
            ));
        };
        // hold 意味着上下文可能已被注入：此时改配置（放宽过滤、改技能目录、
        // 换模型……）会把一次注入变成持久的设置，等 hold 解除后仍然生效。
        if self
            .context
            .gate
            .as_ref()
            .is_some_and(|gate| gate.hold_active())
        {
            return Err(VfsError::Storage(
                crate::tools::CONFIG_HOLD_DISABLED.to_owned(),
            ));
        }
        // 从读文件到换句柄全程持有配置句柄的写锁。控制通道改模型 / 思考强度也是在这把锁里
        // 读改写同一个文件；两边各读一份旧文件、各自写回去的话，后写的会把先写的改动盖掉，
        // 文件与内存里的配置还会和正在用的 provider 对不上。
        let mut loaded = handle.write().expect("config lock poisoned");
        let path = loaded.path.clone();
        let current_text =
            std::fs::read_to_string(&path).map_err(|error| VfsError::Storage(error.to_string()))?;
        let document: toml_edit::DocumentMut = current_text
            .parse()
            .map_err(|error| VfsError::Storage(format!("cannot parse sbot.toml: {error}")))?;

        let (new_text, written) = match target.path.segments() {
            [root] if *root == "config.toml" => (
                whole_file(content, &current_text, &document)?,
                format!("wrote {} bytes to /config.toml", content.len()),
            ),
            [root, section] if *root == "config" => (
                replace_section(document, section, content)?,
                format!("wrote /config/{section}"),
            ),
            [root, section, key] if *root == "config" => (
                replace_value(document, section, key, content)?,
                format!("wrote /config/{section}/{key}"),
            ),
            _ => return Err(VfsError::NotFound(target.path.to_string())),
        };
        // 校验不过就不落盘；落盘不成就不换内存里的配置。
        let config = validate_config_text(&new_text, self.context.models.as_ref())?;
        crate::config::replace_file(&path, &new_text)
            .map_err(|error| VfsError::Storage(error.to_string()))?;
        loaded.config = config.clone();
        drop(loaded);
        // 通知在换好句柄、放掉锁之后：回调读配置句柄时看到的已经是新配置，也不会在持锁时
        // 重入这把锁。
        self.notify(&config);
        Ok(written)
    }

    /// 是否属于可写的配置视图（`/config/**` 或 `/config.toml`）。
    pub(crate) fn is_config_path(path: &VfsPath) -> bool {
        path.segments()
            .first()
            .is_some_and(|first| *first == "config" || *first == "config.toml")
    }
}

/// `/config.toml`：整文件替换。返回要写回的文本。
///
/// 视图里读到的是遮蔽后的原文，原样带回的占位符换回原值；只读键与现在的文件逐键对比，
/// 有变化就整体拒绝。
fn whole_file(
    content: &str,
    current_text: &str,
    current: &toml_edit::DocumentMut,
) -> Result<String, VfsError> {
    let mut document: toml_edit::DocumentMut = content.parse().map_err(|error| {
        VfsError::Storage(format!("the new sbot.toml is not valid TOML: {error}"))
    })?;
    redact::restore_redacted(&mut document, current);
    let new_text = document.to_string();
    let old_value: toml::Value =
        toml::from_str(current_text).map_err(|error| VfsError::Storage(error.to_string()))?;
    let new_value: toml::Value =
        toml::from_str(&new_text).map_err(|error| VfsError::Storage(error.to_string()))?;
    check_protected_keys(&old_value, &new_value)?;
    Ok(new_text)
}

/// `/config/<section>`：整段覆盖，`content` 是这一段的正文。返回要写回的文本。
fn replace_section(
    mut document: toml_edit::DocumentMut,
    section: &str,
    content: &str,
) -> Result<String, VfsError> {
    if !section_writable(section) {
        return Err(VfsError::Storage(format!(
            "config section {section:?} is read-only for the agent"
        )));
    }
    let snippet: toml_edit::DocumentMut = content
        .parse()
        .map_err(|error| VfsError::Storage(format!("cannot parse the section body: {error}")))?;
    document[section] = toml_edit::Item::Table(snippet.as_table().clone());
    Ok(document.to_string())
}

/// `/config/<section>/<key>`：改一个叶子值，`content` 是 TOML 字面量。返回要写回的文本。
fn replace_value(
    mut document: toml_edit::DocumentMut,
    section: &str,
    key: &str,
    content: &str,
) -> Result<String, VfsError> {
    if !key_writable(section, key) {
        return Err(VfsError::Storage(format!(
            "config key {section}.{key} is read-only for the agent"
        )));
    }
    let snippet: toml_edit::DocumentMut =
        format!("{key} = {content}").parse().map_err(|error| {
            VfsError::Storage(format!(
                "value for {section}.{key} is not a TOML literal: {error}"
            ))
        })?;
    let Some(item) = snippet.as_table().get(key).cloned() else {
        return Err(VfsError::Storage("empty config value".to_owned()));
    };
    if !document.contains_key(section) {
        document[section] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    document[section][key] = item;
    Ok(document.to_string())
}

/// 整段可写的配置节。
fn section_writable(section: &str) -> bool {
    matches!(
        section,
        "filter" | "compaction" | "skills" | "queue" | "style"
    )
}

/// 叶子可写判定：整段可写，或 `agent` 的显式白名单键。
fn key_writable(section: &str, key: &str) -> bool {
    section_writable(section)
        || (section == "agent" && matches!(key, "available_models" | "thinking_level"))
}

/// 整文件写入时，只读键不得改变。
fn check_protected_keys(old: &toml::Value, new: &toml::Value) -> Result<(), VfsError> {
    let Some(old) = old.as_table() else {
        return Ok(());
    };
    let Some(new) = new.as_table() else {
        return Err(VfsError::Storage(
            "sbot.toml must be a TOML table".to_owned(),
        ));
    };
    let mut keys: std::collections::BTreeSet<String> = old.keys().cloned().collect();
    keys.extend(new.keys().cloned());
    for key in keys {
        if key == "agent" {
            let old_agent = old.get(&key).and_then(toml::Value::as_table);
            let new_agent = new.get(&key).and_then(toml::Value::as_table);
            let mut agent_keys: std::collections::BTreeSet<String> = old_agent
                .into_iter()
                .flat_map(|table| table.keys().cloned())
                .collect();
            agent_keys.extend(
                new_agent
                    .into_iter()
                    .flat_map(|table| table.keys().cloned()),
            );
            for agent_key in agent_keys {
                if agent_key == "available_models" || agent_key == "thinking_level" {
                    continue;
                }
                let old_value = old_agent.and_then(|table| table.get(&agent_key));
                let new_value = new_agent.and_then(|table| table.get(&agent_key));
                if old_value != new_value {
                    return Err(VfsError::Storage(format!(
                        "config key agent.{agent_key} is read-only for the agent"
                    )));
                }
            }
            continue;
        }
        if section_writable(&key) {
            continue;
        }
        if old.get(&key) != new.get(&key) {
            return Err(VfsError::Storage(format!(
                "config key {key} is read-only for the agent"
            )));
        }
    }
    Ok(())
}

/// 解析并校验候选配置；失败时不写盘。
fn validate_config_text(text: &str, models: Option<&ModelsFile>) -> Result<SbotConfig, VfsError> {
    let config: SbotConfig = toml::from_str(text)
        .map_err(|error| VfsError::Storage(format!("invalid sbot.toml: {error}")))?;
    config
        .validate_shape()
        .map_err(|errors| VfsError::Storage(format!("config validation failed: {errors}")))?;
    if let Some(models) = models {
        config
            .validate_with_models(models)
            .map_err(|errors| VfsError::Storage(format!("config validation failed: {errors}")))?;
    }
    Ok(config)
}
