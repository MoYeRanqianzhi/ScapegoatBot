//! `Views` 的 config 部分。
//!
//! 由 `views/mod.rs` 的 `list` / `read` 路由调用。

use super::*;

impl Views {
    /// 把当前配置序列化成 TOML 表；失败时视为视图不可用。
    pub(super) fn config_table(&self) -> Option<toml::Table> {
        let config = self.config_snapshot()?;
        let text = toml::to_string(&config).ok()?;
        toml::from_str(&text).ok()
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

    /// `/config.toml`：`sbot.toml` 原文；凭据不在其中。
    pub(super) fn read_config_toml(&self) -> Result<ViewRead, VfsError> {
        let path = self
            .context
            .config
            .as_ref()
            .map(|handle| handle.read().expect("config lock poisoned").path.clone())
            .ok_or_else(|| VfsError::Offline("/config.toml".to_owned()))?;
        let text =
            std::fs::read_to_string(&path).map_err(|error| VfsError::Storage(error.to_string()))?;
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

    /// 写配置：只允许 `model_writable` 白名单；校验通过后原子写回。
    ///
    /// 支持三种形式：
    /// - `/config/<section>/<key>`：叶子值（TOML 字面量）；
    /// - `/config/<section>`：整段覆盖；
    /// - `/config.toml`：整文件，但只读键不得改变。
    pub(crate) fn write_config(&self, target: &ParsedTarget, content: &str) -> Result<String, VfsError> {
        let Some(handle) = &self.context.config else {
            return Err(VfsError::Offline(
                "configuration is not attached".to_owned(),
            ));
        };
        let path = {
            let guard = handle.read().expect("config lock poisoned");
            guard.path.clone()
        };
        let current_text =
            std::fs::read_to_string(&path).map_err(|error| VfsError::Storage(error.to_string()))?;
        let mut document: toml_edit::DocumentMut = current_text
            .parse()
            .map_err(|error| VfsError::Storage(format!("cannot parse sbot.toml: {error}")))?;

        let segments = target.path.segments();
        match segments {
            [root] if *root == "config.toml" => {
                let new_document: toml_edit::DocumentMut = content.parse().map_err(|error| {
                    VfsError::Storage(format!("the new sbot.toml is not valid TOML: {error}"))
                })?;
                // 逐键对比，只读键发生变化则整体拒绝。
                let old_value: toml::Value = toml::from_str(&current_text)
                    .map_err(|error| VfsError::Storage(error.to_string()))?;
                let new_value: toml::Value = toml::from_str(&new_document.to_string())
                    .map_err(|error| VfsError::Storage(error.to_string()))?;
                check_protected_keys(&old_value, &new_value)?;
                let config =
                    validate_config_text(&new_document.to_string(), self.context.models.as_ref())?;
                self.persist(&path, new_document.to_string())?;
                self.notify(&config);
                *handle.write().expect("config lock poisoned") = LoadedConfig {
                    config,
                    path: path.clone(),
                    dir: path
                        .parent()
                        .map(std::path::Path::to_path_buf)
                        .unwrap_or_else(|| PathBuf::from(".")),
                };
                Ok(format!("wrote {} bytes to /config.toml", content.len()))
            }
            [root, section] if *root == "config" => {
                if !section_writable(section) {
                    return Err(VfsError::Storage(format!(
                        "config section {section:?} is read-only for the agent"
                    )));
                }
                let snippet: toml_edit::DocumentMut = content.parse().map_err(|error| {
                    VfsError::Storage(format!("cannot parse the section body: {error}"))
                })?;
                let table = snippet.as_table().clone();
                document[section.as_str()] = toml_edit::Item::Table(table);
                let config =
                    validate_config_text(&document.to_string(), self.context.models.as_ref())?;
                self.persist(&path, document.to_string())?;
                self.notify(&config);
                *handle.write().expect("config lock poisoned") = LoadedConfig {
                    config,
                    path: path.clone(),
                    dir: path
                        .parent()
                        .map(std::path::Path::to_path_buf)
                        .unwrap_or_else(|| PathBuf::from(".")),
                };
                Ok(format!("wrote /config/{section}"))
            }
            [root, section, key] if *root == "config" => {
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
                    document[section.as_str()] = toml_edit::Item::Table(toml_edit::Table::new());
                }
                document[section.as_str()][key.as_str()] = item;
                let config =
                    validate_config_text(&document.to_string(), self.context.models.as_ref())?;
                self.persist(&path, document.to_string())?;
                self.notify(&config);
                *handle.write().expect("config lock poisoned") = LoadedConfig {
                    config,
                    path: path.clone(),
                    dir: path
                        .parent()
                        .map(std::path::Path::to_path_buf)
                        .unwrap_or_else(|| PathBuf::from(".")),
                };
                Ok(format!("wrote /config/{section}/{key}"))
            }
            _ => Err(VfsError::NotFound(target.path.to_string())),
        }
    }

    /// 原子写回：临时文件 + fsync + rename。
    pub(super) fn persist(&self, path: &std::path::Path, text: String) -> Result<(), VfsError> {
        let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
        {
            use std::io::Write;
            let mut file = std::fs::File::create(&tmp)
                .map_err(|error| VfsError::Storage(error.to_string()))?;
            file.write_all(text.as_bytes())
                .map_err(|error| VfsError::Storage(error.to_string()))?;
            file.sync_all()
                .map_err(|error| VfsError::Storage(error.to_string()))?;
        }
        std::fs::rename(&tmp, path).map_err(|error| VfsError::Storage(error.to_string()))
    }

    /// 是否属于可写的配置视图（`/config/**` 或 `/config.toml`）。
    pub(crate) fn is_config_path(path: &VfsPath) -> bool {
        path.segments()
            .first()
            .is_some_and(|first| *first == "config" || *first == "config.toml")
    }
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
