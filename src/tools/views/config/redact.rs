//! 配置视图里的密钥遮蔽。
//!
//! `sbot.toml` 本身不放凭据（凭据在 `auth.json`），唯一的例外是 `[[mcp.servers]]` 的
//! `env`：外部 MCP 进程常用环境变量传 token。模型读到的配置视图把这些值替换成
//! [`REDACTED`]；整文件写回时，原样带回的占位符恢复成原值，模型不必也不能知道它们。

use toml_edit::{DocumentMut, Item, TableLike};

/// 被遮蔽的值在视图里显示成这个占位符。
pub(super) const REDACTED: &str = "<redacted>";

/// 遮蔽序列化后的有效配置（`/config/**` 视图）。
pub(super) fn redact_table(table: &mut toml::Table) {
    // `mcp.servers` 是表数组；没有这一段就没有要遮蔽的东西。
    let Some(servers) = table
        .get_mut("mcp")
        .and_then(|mcp| mcp.get_mut("servers"))
        .and_then(toml::Value::as_array_mut)
    else {
        return;
    };
    for server in servers {
        if let Some(env) = server.get_mut("env").and_then(toml::Value::as_table_mut) {
            // 只遮蔽值，键名保留：模型可以知道“这个服务器配了 GITHUB_TOKEN”。
            for (_, value) in env.iter_mut() {
                *value = toml::Value::String(REDACTED.to_owned());
            }
        }
    }
}

/// 遮蔽 `sbot.toml` 原文（`/config.toml` 视图），其余内容与注释保持原样。
///
/// 原文解析失败时返回错误而不是原文：解析不了就无法确认里面没有密钥。
pub(super) fn redact_document(text: &str) -> Result<String, toml_edit::TomlError> {
    let mut document: DocumentMut = text.parse()?;
    for env in env_tables_mut(&mut document) {
        for (_, item) in env.iter_mut() {
            *item = toml_edit::value(REDACTED);
        }
    }
    Ok(document.to_string())
}

/// 整文件写回前：新文档里仍是 [`REDACTED`] 的 env 值换回旧文档的原值。
///
/// 两边按服务器出现顺序与键名对应；模型改动了服务器顺序或键名时不会恢复，
/// 随后的只读键检查会因为 `mcp` 变化而拒绝整次写入。
pub(super) fn restore_redacted(new: &mut DocumentMut, old: &DocumentMut) {
    // 取旧文档的 env 表要可变借用，复制一份，避免改到调用方的旧文档。
    let mut old = old.clone();
    let old_envs = env_tables_mut(&mut old);
    for (new_env, old_env) in env_tables_mut(new).into_iter().zip(old_envs) {
        for (key, item) in new_env.iter_mut() {
            if item.as_str() == Some(REDACTED)
                && let Some(original) = old_env.get(key.get())
            {
                *item = original.clone();
            }
        }
    }
}

/// 文档里每个 MCP 服务器的 `env` 表，按出现顺序。
///
/// 兼容两种写法：`[[mcp.servers]]` 表数组，以及 `servers = [{ ... }]` 内联数组。
fn env_tables_mut(document: &mut DocumentMut) -> Vec<&mut dyn TableLike> {
    let Some(servers) = document
        .get_mut("mcp")
        .and_then(|mcp| mcp.get_mut("servers"))
    else {
        return Vec::new();
    };
    let servers: Vec<&mut dyn TableLike> = match servers {
        Item::ArrayOfTables(array) => array
            .iter_mut()
            .map(|server| server as &mut dyn TableLike)
            .collect(),
        Item::Value(toml_edit::Value::Array(array)) => array
            .iter_mut()
            .filter_map(|server| {
                server
                    .as_inline_table_mut()
                    .map(|server| server as &mut dyn TableLike)
            })
            .collect(),
        _ => Vec::new(),
    };
    servers
        .into_iter()
        .filter_map(|server| server.get_mut("env").and_then(Item::as_table_like_mut))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "# keep me\n\
        [[mcp.servers]]\n\
        name = \"gh\"\n\
        command = \"npx\"\n\
        env = { GITHUB_TOKEN = \"s3cret\" }\n\
        \n\
        [[mcp.servers]]\n\
        name = \"db\"\n\
        command = \"db-mcp\"\n\
        [mcp.servers.env]\n\
        PASSWORD = \"hunter2\"\n";

    #[test]
    fn both_table_styles_are_redacted_and_comments_survive() {
        let redacted = redact_document(TEXT).unwrap();
        assert!(!redacted.contains("s3cret"), "{redacted}");
        assert!(!redacted.contains("hunter2"), "{redacted}");
        assert!(redacted.contains("GITHUB_TOKEN"), "{redacted}");
        assert!(redacted.contains("# keep me"), "{redacted}");

        // 内联数组写法同样遮蔽。
        let inline = redact_document(
            "[mcp]\nservers = [{ name = \"a\", command = \"b\", env = { K = \"v1\" } }]\n",
        )
        .unwrap();
        assert!(!inline.contains("v1"), "{inline}");
    }

    #[test]
    fn writing_back_the_redacted_text_restores_the_secrets() {
        let old: DocumentMut = TEXT.parse().unwrap();
        let mut new: DocumentMut = redact_document(TEXT).unwrap().parse().unwrap();
        restore_redacted(&mut new, &old);
        let restored: toml::Value = toml::from_str(&new.to_string()).unwrap();
        let original: toml::Value = toml::from_str(TEXT).unwrap();
        assert_eq!(restored, original);
    }

    #[test]
    fn the_effective_config_table_is_redacted() {
        let mut table: toml::Table = toml::from_str(TEXT).unwrap();
        redact_table(&mut table);
        let text = toml::to_string(&table).unwrap();
        assert!(
            !text.contains("s3cret") && !text.contains("hunter2"),
            "{text}"
        );
    }
}
