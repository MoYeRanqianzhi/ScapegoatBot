//! 凭据值包装：明文优先，`$ENV` 可选插值。
//!
//! 设计依据：设计 §9.4 与用户明确要求——`auth.json` 里直接写明文，
//! 不要求环境变量。`"$NAME"` 只是可选便捷写法：仅当值以 `$` 开头才展开。
//!
//! 安全约束：
//! - `Debug` 输出必须脱敏，避免被 `tracing::debug!("{config:?}")` 泄漏；
//! - `Display` 只给前 3 位 + 后 4 位，用于 `sbot status` 与日志；
//! - 序列化回到 `auth.json` 时输出原文（写回配置需要），但 `auth.json` 不挂载 VFS。

use std::fmt;

use serde::{Deserialize, Serialize};

/// 一个凭据来源。
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// 从 `auth.json` 的原始字符串构造；写回配置时需要原文。
    #[allow(dead_code)]
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// 原始写法（可能含 `$` 前缀），仅用于写回文件。
    #[allow(dead_code)]
    pub fn raw(&self) -> &str {
        &self.0
    }

    /// 解析出实际值。
    ///
    /// - 以 `$` 开头：读取对应环境变量；变量缺失或为空时返回 `None`，
    ///   由调用方决定是“后端不可用”还是启动失败（设计 §9.4 / §9.6）。
    /// - 其他：原样作为明文返回；空串视为未配置。
    pub fn value(&self) -> Option<String> {
        if let Some(name) = self.0.strip_prefix('$') {
            if name.is_empty() {
                return None;
            }
            match std::env::var(name) {
                Ok(value) if !value.is_empty() => Some(value),
                _ => None,
            }
        } else if self.0.is_empty() {
            None
        } else {
            Some(self.0.clone())
        }
    }

    /// 是否已配置（能把值解析出来）。
    pub fn is_present(&self) -> bool {
        self.value().is_some()
    }

    /// 脱敏展示：长度足够时保留前 3 后 4，否则整体打码。
    pub fn masked(&self) -> String {
        let Some(value) = self.value() else {
            return "<unset>".to_owned();
        };
        let chars: Vec<char> = value.chars().collect();
        if chars.len() <= 8 {
            return "*".repeat(chars.len());
        }
        let head: String = chars.iter().take(3).collect();
        let tail: String = chars.iter().rev().take(4).rev().collect();
        format!("{head}...{tail}")
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 只暴露状态，不暴露任何字符。
        f.debug_tuple("Secret")
            .field(&if self.is_present() {
                "<set>"
            } else {
                "<unset>"
            })
            .finish()
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.masked())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_is_used_as_is() {
        let secret = Secret::new("sk-jev-test");
        assert_eq!(secret.value().as_deref(), Some("sk-jev-test"));
        assert!(secret.is_present());
    }

    #[test]
    fn empty_literal_is_unset() {
        let secret = Secret::new("");
        assert_eq!(secret.value(), None);
        assert!(!secret.is_present());
    }

    #[test]
    fn env_prefix_reads_variable() {
        // 用测试专用变量名，避免与宿主环境冲突。
        unsafe { std::env::set_var("SBOT_TEST_SECRET_VALUE", "from-env") };
        let secret = Secret::new("$SBOT_TEST_SECRET_VALUE");
        assert_eq!(secret.value().as_deref(), Some("from-env"));
        unsafe { std::env::remove_var("SBOT_TEST_SECRET_VALUE") };
        assert_eq!(secret.value(), None);
    }

    #[test]
    fn debug_never_leaks_value() {
        let secret = Secret::new("sk-super-secret");
        let rendered = format!("{secret:?}");
        assert!(!rendered.contains("sk-super-secret"));
        assert!(rendered.contains("<set>"));
    }

    #[test]
    fn masked_keeps_edges() {
        let secret = Secret::new("sk-1234567890abc");
        assert_eq!(secret.masked(), "sk-...0abc");
        assert_eq!(Secret::new("short").masked(), "*****");
    }
}
