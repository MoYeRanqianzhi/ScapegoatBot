//! VFS 路径：POSIX 风格、NFC 规范化、严格校验。
//!
//! 设计依据：设计 §6.3。
//!
//! 校验在**进入存储之前**完成，而不是靠字符串过滤：
//! - 只接受以 `/` 开头的绝对虚拟路径；
//! - 拒绝反斜杠、盘符、空段、`.`、`..`、控制字符与 NUL；
//! - 全路径长度 ≤ 255，深度 ≤ 16；
//! - 先做 Unicode NFC，再按段校验。
//!
//! 查询参数只允许出现在动态视图上；静态文件路径带 `?` 直接报错。

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use unicode_normalization::UnicodeNormalization;

/// 路径长度上限（字符数）。
pub const MAX_PATH_CHARS: usize = 255;
/// 路径深度上限。
pub const MAX_DEPTH: usize = 16;

/// 解析后的虚拟路径。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct VfsPath {
    /// 各段；根为空。
    segments: Vec<String>,
}

impl VfsPath {
    /// 根路径。
    pub fn root() -> Self {
        Self {
            segments: Vec::new(),
        }
    }

    /// 解析并校验一个路径字符串（不含查询串）。
    pub fn parse(raw: &str) -> Result<Self, PathError> {
        if raw.is_empty() {
            return Err(PathError::Empty);
        }
        if !raw.starts_with('/') {
            return Err(PathError::NotAbsolute);
        }
        if raw.contains('\\') {
            return Err(PathError::Backslash);
        }
        if raw.contains('\0') {
            return Err(PathError::Nul);
        }
        if raw.chars().any(|c| c.is_control()) {
            return Err(PathError::ControlCharacter);
        }
        // 盘符只可能出现在首个段里，例如 `C:/x`；绝对路径已要求以 `/` 开头，
        // 但 `/C:/x` 这种写法同样要拒绝。
        if raw
            .split('/')
            .any(|segment| segment.len() == 2 && segment.ends_with(':'))
        {
            return Err(PathError::DriveLetter);
        }

        // 去掉末尾单个 `/`，但根 `/` 保持为空。
        let body = raw.trim_end_matches('/');
        if body.is_empty() {
            return Ok(Self::root());
        }

        let normalized: String = body.nfc().collect();
        if normalized.chars().count() > MAX_PATH_CHARS {
            return Err(PathError::TooLong);
        }

        let mut segments = Vec::new();
        for segment in normalized.split('/').skip(1) {
            if segment.is_empty() {
                return Err(PathError::EmptySegment);
            }
            if segment == "." || segment == ".." {
                return Err(PathError::Traversal);
            }
            segments.push(segment.to_owned());
        }
        if segments.len() > MAX_DEPTH {
            return Err(PathError::TooDeep);
        }
        Ok(Self { segments })
    }

    /// 各段。
    pub fn segments(&self) -> &[String] {
        &self.segments
    }

    /// 是否为根。
    pub fn is_root(&self) -> bool {
        self.segments.is_empty()
    }

    /// 最后一段（根没有）。
    pub fn file_name(&self) -> Option<&str> {
        self.segments.last().map(String::as_str)
    }

    /// 父路径；根没有父路径。
    pub fn parent(&self) -> Option<Self> {
        if self.segments.is_empty() {
            return None;
        }
        Some(Self {
            segments: self.segments[..self.segments.len() - 1].to_vec(),
        })
    }

    /// 追加一段。
    pub fn join(&self, segment: &str) -> Result<Self, PathError> {
        if segment.is_empty() || segment == "." || segment == ".." || segment.contains('/') {
            return Err(PathError::InvalidSegment);
        }
        let mut segments = self.segments.clone();
        segments.push(segment.to_owned());
        if segments.len() > MAX_DEPTH {
            return Err(PathError::TooDeep);
        }
        Ok(Self { segments })
    }

    /// 修改最后一段（用于 `Edit` 的 rename 语义等）。
    pub fn with_file_name(&self, name: &str) -> Result<Self, PathError> {
        let parent = self.parent().ok_or(PathError::InvalidSegment)?;
        parent.join(name)
    }

    /// 是否是某个前缀路径（含自身）。
    pub fn starts_with(&self, prefix: &Self) -> bool {
        self.segments.len() >= prefix.segments.len()
            && self.segments[..prefix.segments.len()] == prefix.segments[..]
    }
}

impl Serialize for VfsPath {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for VfsPath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for VfsPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.segments.is_empty() {
            return f.write_str("/");
        }
        for segment in &self.segments {
            write!(f, "/{segment}")?;
        }
        Ok(())
    }
}

/// 路径解析错误。消息面向模型，英文（设计 §7.1）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    #[error("path must not be empty")]
    Empty,
    #[error("path must be absolute (start with '/')")]
    NotAbsolute,
    #[error("backslashes are not allowed; use '/' as the separator")]
    Backslash,
    #[error("NUL bytes are not allowed in paths")]
    Nul,
    #[error("control characters are not allowed in paths")]
    ControlCharacter,
    #[error("drive letters are not allowed in virtual paths")]
    DriveLetter,
    #[error("empty path segments are not allowed")]
    EmptySegment,
    #[error("'.' and '..' segments are not allowed")]
    Traversal,
    #[error("path is too long (max {MAX_PATH_CHARS} characters)")]
    TooLong,
    #[error("path is too deep (max {MAX_DEPTH} segments)")]
    TooDeep,
    #[error("invalid path segment")]
    InvalidSegment,
    #[error("query parameters are only allowed on dynamic views")]
    QueryOnStaticPath,
    #[error("malformed query string")]
    BadQuery,
    #[error("unknown query parameter {0:?}")]
    UnknownQueryKey(String),
    #[error("invalid value for query parameter {0:?}")]
    BadQueryValue(&'static str),
}

/// 查询排序。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Order {
    #[default]
    Asc,
    Desc,
}

/// 动态视图查询参数（设计 §6.3）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewQuery {
    /// 与 monitor `when` 同一套谓词语法；此处只保存原文，求值在 Phase 4。
    pub filter: Option<String>,
    /// 默认 50，上限 500。
    pub limit: usize,
    pub offset: usize,
    pub order: Order,
}

impl Default for ViewQuery {
    fn default() -> Self {
        Self {
            filter: None,
            limit: 50,
            offset: 0,
            order: Order::Asc,
        }
    }
}

/// 解析结果：路径 + 可选查询。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTarget {
    pub path: VfsPath,
    pub query: Option<ViewQuery>,
}

/// 动态视图前缀：只有这些路径允许查询串。
const DYNAMIC_ROOTS: &[&str] = &[
    "inbox", "messages", "sessions", "friends", "groups", "config",
];

impl ParsedTarget {
    /// 解析带查询串的目标。
    ///
    /// `?` 之后的查询串只允许出现在动态视图上；静态文件带查询直接报错。
    pub fn parse(raw: &str) -> Result<Self, PathError> {
        match raw.split_once('?') {
            None => Ok(Self {
                path: VfsPath::parse(raw)?,
                query: None,
            }),
            Some((path_part, query_part)) => {
                let path = VfsPath::parse(path_part)?;
                if !is_dynamic(&path) {
                    return Err(PathError::QueryOnStaticPath);
                }
                Ok(Self {
                    path,
                    query: Some(ViewQuery::parse(query_part)?),
                })
            }
        }
    }
}

/// 判断路径是否属于动态视图。
pub fn is_dynamic(path: &VfsPath) -> bool {
    path.segments()
        .first()
        .is_some_and(|first| DYNAMIC_ROOTS.contains(&first.as_str()))
}

impl ViewQuery {
    /// 解析 `k=v&k=v` 形式。
    pub fn parse(raw: &str) -> Result<Self, PathError> {
        let mut query = Self::default();
        for pair in raw.split('&').filter(|pair| !pair.is_empty()) {
            let Some((key, value)) = pair.split_once('=') else {
                return Err(PathError::BadQuery);
            };
            let value = percent_decode(value)?;
            match key {
                "filter" => {
                    if value.trim().is_empty() {
                        return Err(PathError::BadQueryValue("filter"));
                    }
                    query.filter = Some(value);
                }
                "limit" => {
                    let limit: usize = value
                        .parse()
                        .map_err(|_| PathError::BadQueryValue("limit"))?;
                    if !(1..=500).contains(&limit) {
                        return Err(PathError::BadQueryValue("limit"));
                    }
                    query.limit = limit;
                }
                "offset" => {
                    query.offset = value
                        .parse()
                        .map_err(|_| PathError::BadQueryValue("offset"))?;
                }
                "order" => {
                    query.order = match value.as_str() {
                        "asc" => Order::Asc,
                        "desc" => Order::Desc,
                        _ => return Err(PathError::BadQueryValue("order")),
                    };
                }
                other => return Err(PathError::UnknownQueryKey(other.to_owned())),
            }
        }
        Ok(query)
    }
}

/// 最小百分号解码：只处理 UTF-8 百分号转义，不做 `+` → 空格转换。
fn percent_decode(raw: &str) -> Result<String, PathError> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(PathError::BadQuery);
            }
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3])
                .map_err(|_| PathError::BadQuery)?;
            let byte = u8::from_str_radix(hex, 16).map_err(|_| PathError::BadQuery)?;
            out.push(byte);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).map_err(|_| PathError::BadQuery)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_displays_absolute_paths() {
        let path = VfsPath::parse("/memory/topics/rust.md").unwrap();
        assert_eq!(path.segments(), ["memory", "topics", "rust.md"]);
        assert_eq!(path.to_string(), "/memory/topics/rust.md");
    }

    #[test]
    fn root_is_just_slash() {
        let root = VfsPath::parse("/").unwrap();
        assert!(root.is_root());
        assert_eq!(root.to_string(), "/");
        assert_eq!(root.parent(), None);
    }

    #[test]
    fn trailing_slash_is_tolerated() {
        assert_eq!(VfsPath::parse("/notes/").unwrap().to_string(), "/notes");
    }

    #[test]
    fn rejects_traversal_and_relative_paths() {
        for raw in [
            "",
            "notes",
            "/../etc",
            "/notes/../x",
            "/./x",
            "//x",
            "/a//b",
        ] {
            assert!(VfsPath::parse(raw).is_err(), "{raw} should be rejected");
        }
    }

    #[test]
    fn rejects_windows_forms() {
        assert_eq!(VfsPath::parse(r"/a\b").unwrap_err(), PathError::Backslash);
        assert_eq!(VfsPath::parse("/C:/x").unwrap_err(), PathError::DriveLetter);
        assert_eq!(VfsPath::parse("/a\0b").unwrap_err(), PathError::Nul);
    }

    #[test]
    fn enforces_length_and_depth() {
        let long = format!("/{}", "a".repeat(MAX_PATH_CHARS));
        assert_eq!(VfsPath::parse(&long).unwrap_err(), PathError::TooLong);

        let deep = format!("/{}", vec!["a"; MAX_DEPTH + 1].join("/"));
        assert_eq!(VfsPath::parse(&deep).unwrap_err(), PathError::TooDeep);
    }

    #[test]
    fn normalizes_to_nfc() {
        // U+0041 U+030A（组合）应折叠为 U+00C5。
        let decomposed = "/A\u{030A}";
        let normalized = VfsPath::parse(decomposed).unwrap();
        assert_eq!(normalized.segments(), ["Å"]);
    }

    #[test]
    fn parent_and_join_are_consistent() {
        let path = VfsPath::parse("/a/b/c").unwrap();
        assert_eq!(path.parent().unwrap().to_string(), "/a/b");
        assert_eq!(
            path.parent().unwrap().join("d").unwrap().to_string(),
            "/a/b/d"
        );
        assert!(path.join("..").is_err());
    }

    #[test]
    fn query_only_on_dynamic_views() {
        assert!(ParsedTarget::parse("/AGENTS.md?limit=5").is_err());
        let target = ParsedTarget::parse("/inbox?filter=unread&limit=20&order=desc").unwrap();
        let query = target.query.unwrap();
        assert_eq!(query.filter.as_deref(), Some("unread"));
        assert_eq!(query.limit, 20);
        assert_eq!(query.order, Order::Desc);
    }

    #[test]
    fn query_limits_are_enforced() {
        assert!(ViewQuery::parse("limit=0").is_err());
        assert!(ViewQuery::parse("limit=501").is_err());
        assert!(ViewQuery::parse("order=sideways").is_err());
        assert!(ViewQuery::parse("nope=1").is_err());
    }

    #[test]
    fn query_values_are_percent_decoded() {
        let query = ViewQuery::parse("filter=contains%28%22%E6%B4%BB%E5%8A%A8%22%29").unwrap();
        assert_eq!(query.filter.as_deref(), Some("contains(\"活动\")"));
    }

    #[test]
    fn starts_with_prefix_works() {
        let path = VfsPath::parse("/memory/topics/x.md").unwrap();
        let prefix = VfsPath::parse("/memory").unwrap();
        assert!(path.starts_with(&prefix));
        assert!(!prefix.starts_with(&path));
    }
}
