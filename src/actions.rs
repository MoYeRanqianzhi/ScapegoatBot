//! `API` 工具的 action 注册表。
//!
//! 这里只有数据：action 名、参数 schema、分级、作用域与示例。执行逻辑在
//! `tools/api.rs`；拆开是为了让 action 表与门禁/发送逻辑各自可读。
//!
//! 命名是分层的，第一段说明效果落在哪里：`napcat.*`（QQ）、`local.*`（只碰 VFS 的本地
//! 工具：`local.media.image.*`、`local.net.download`）、`agent.*`（对自己）、`api.*`
//! （查注册表）。
//!
//! `specs()` 是唯一事实来源：`api.list` / `api.help`、发送范围校验与
//! owner 保护都从这张表推导。

use serde_json::{Value, json};

/// action 类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Message,
    Query,
    Request,
    Admin,
    File,
    Profile,
    Agent,
    Media,
    Meta,
}

impl Category {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Query => "query",
            Self::Request => "request",
            Self::Admin => "admin",
            Self::File => "file",
            Self::Profile => "profile",
            Self::Agent => "agent",
            Self::Media => "media",
            Self::Meta => "meta",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "message" => Self::Message,
            "query" => Self::Query,
            "request" => Self::Request,
            "admin" => Self::Admin,
            "file" => Self::File,
            "profile" => Self::Profile,
            "agent" => Self::Agent,
            "media" => Self::Media,
            "meta" => Self::Meta,
            _ => return None,
        })
    }

    /// 是否会产生对外影响。
    pub fn is_outbound(self) -> bool {
        matches!(
            self,
            Self::Message | Self::Request | Self::Admin | Self::File | Self::Profile
        )
    }
}

/// 一个 action 的元数据。
#[derive(Debug, Clone)]
pub struct ActionSpec {
    /// 完整名，例如 `napcat.send_group_msg`。
    pub action: &'static str,
    pub category: Category,
    pub summary: &'static str,
    /// 参数 JSON Schema。
    pub params: Value,
    /// 返回字段说明。
    pub returns: &'static str,
    /// 是否需要 owner intent（发送、请求处理、破坏性操作）。
    pub requires_owner_intent: bool,
    /// 作用域字段（用于 send_scopes 与 chat 解析）。
    pub scope_field: Option<&'static str>,
    /// 目标 QQ 字段（用于 owner/master 保护）；与 scope_field 不同。
    pub target_field: Option<&'static str>,
    /// 是否属于 admin/destructive（目标为 owner 时恒拒绝）。
    pub destructive: bool,
    pub examples: Vec<&'static str>,
}

impl ActionSpec {
    /// 顶层命名空间（第一段）：`napcat`、`local`、`agent`、`api`。
    pub fn namespace(&self) -> &'static str {
        self.action.split_once('.').map(|(ns, _)| ns).unwrap_or("")
    }

    /// 是否属于某个命名空间或其下层，例如 `local.media` 包含 `local.media.image.info`。
    pub fn in_namespace(&self, prefix: &str) -> bool {
        let prefix = prefix.trim_end_matches('.');
        self.action
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('.'))
    }

    /// `hold` 期间是否可用。
    pub fn hold_allowed(&self) -> bool {
        self.namespace() != "napcat"
    }
}

fn send_spec(
    action: &'static str,
    summary: &'static str,
    scope_field: &'static str,
    params: Value,
    examples: &'static [&'static str],
) -> ActionSpec {
    ActionSpec {
        action,
        category: Category::Message,
        summary,
        params,
        returns: "OneBot response; `data.message_id` for sends.",
        requires_owner_intent: true,
        scope_field: Some(scope_field),
        target_field: if scope_field == "user_id" {
            Some("user_id")
        } else {
            None
        },
        destructive: false,
        examples: examples.to_vec(),
    }
}

/// 全部内置 action。
pub fn specs() -> &'static [ActionSpec] {
    use std::sync::OnceLock;
    static SPECS: OnceLock<Vec<ActionSpec>> = OnceLock::new();
    SPECS.get_or_init(|| {
        let mut list = Vec::new();
        specs_napcat_message(&mut list);
        specs_napcat_query(&mut list);
        specs_napcat_request(&mut list);
        specs_napcat_admin_destructive(&mut list);
        specs_napcat_file(&mut list);
        specs_napcat_profile(&mut list);
        specs_agent(&mut list);
        specs_local_media_image(&mut list);
        specs_local_net(&mut list);
        specs_api(&mut list);
        list
    })
}

fn specs_napcat_message(list: &mut Vec<ActionSpec>) {
    // ---- napcat: message ----
    list.push(send_spec(
        "napcat.send_group_msg",
        "Send a message to a group.",
        "group_id",
        json!({
            "group_id": {"type": "number"},
            "message": {"description": "Message segment array or plain string. Media segments (image/record/video/file) take `file`: a VFS path, a public http(s) URL, or base64:// data."}
        }),
        &[r#"{"group_id":123,"message":[{"type":"text","data":{"text":"hello"}}]}"#],
    ));
    list.push(send_spec(
        "napcat.send_private_msg",
        "Send a message to a friend.",
        "user_id",
        json!({
            "user_id": {"type": "number"},
            "message": {"description": "Message segment array or plain string. Media segments (image/record/video/file) take `file`: a VFS path, a public http(s) URL, or base64:// data."}
        }),
        &[r#"{"user_id":10001,"message":"hello"}"#],
    ));
    // `send_msg` 的目标由 `chat_of` 的专门分支解析（group_id 或 user_id 二选一）；
    // 这里的 `user_id` 只决定 owner/master 保护读取哪个字段。
    list.push(send_spec(
        "napcat.send_msg",
        "Send a message by message_type/chat target. Give exactly one of group_id \
         (message_type \"group\") or user_id (message_type \"private\").",
        "user_id",
        json!({
            "message_type": {"type": "string", "description": "\"group\" or \"private\"; may be omitted."},
            "user_id": {"type": "number"},
            "group_id": {"type": "number"},
            "message": {"description": "Message segment array or plain string. Media segments (image/record/video/file) take `file`: a VFS path, a public http(s) URL, or base64:// data."}
        }),
        &[r#"{"message_type":"group","group_id":123,"message":"hello"}"#],
    ));

    list.push(ActionSpec {
        action: "napcat.delete_msg",
        category: Category::Message,
        summary: "Recall a message by id.",
        params: json!({"message_id": {"type": "number"}}),
        returns: "OneBot response.",
        requires_owner_intent: false,
        scope_field: None,
        target_field: None,
        destructive: false,
        examples: vec![r#"{"message_id":55555}"#],
    });
    list.push(ActionSpec {
        action: "napcat.set_msg_emoji_like",
        category: Category::Message,
        summary: "React to a message with an emoji.",
        params: json!({"message_id": {"type": "number"}, "emoji_id": {"type": "string"}}),
        returns: "OneBot response.",
        requires_owner_intent: false,
        scope_field: None,
        target_field: None,
        destructive: false,
        examples: vec![r#"{"message_id":55555,"emoji_id":"128077"}"#],
    });
    list.push(ActionSpec {
        action: "napcat.send_poke",
        category: Category::Message,
        summary: "Poke someone.",
        params: json!({"user_id": {"type": "number"}, "group_id": {"type": "number"}}),
        returns: "OneBot response.",
        requires_owner_intent: false,
        scope_field: Some("user_id"),
        target_field: Some("user_id"),
        destructive: false,
        examples: vec![r#"{"user_id":10001,"group_id":123}"#],
    });
}

fn specs_napcat_query(list: &mut Vec<ActionSpec>) {
    // ---- napcat: query ----
    for (action, summary, params, example) in [
        (
            "napcat.get_friend_list",
            "List friends.",
            json!({}),
            r#"{}"#,
        ),
        (
            "napcat.get_group_list",
            "List joined groups.",
            json!({}),
            r#"{}"#,
        ),
        (
            "napcat.get_group_member_list",
            "List members of a group.",
            json!({"group_id": {"type": "number"}}),
            r#"{"group_id":123}"#,
        ),
        (
            "napcat.get_group_member_info",
            "Get one member's profile.",
            json!({"group_id": {"type": "number"}, "user_id": {"type": "number"}}),
            r#"{"group_id":123,"user_id":10001}"#,
        ),
        (
            "napcat.get_group_info",
            "Get group profile.",
            json!({"group_id": {"type": "number"}}),
            r#"{"group_id":123}"#,
        ),
        (
            "napcat.get_msg",
            "Get one message by id.",
            json!({"message_id": {"type": "number"}}),
            r#"{"message_id":55555}"#,
        ),
        (
            "napcat.get_group_msg_history",
            "Get group message history.",
            json!({"group_id": {"type": "number"}, "message_seq": {"type": "number"}, "count": {"type": "number"}}),
            r#"{"group_id":123,"count":50}"#,
        ),
        (
            "napcat.get_friend_msg_history",
            "Get private message history.",
            json!({"user_id": {"type": "number"}, "message_seq": {"type": "number"}, "count": {"type": "number"}}),
            r#"{"user_id":10001,"count":50}"#,
        ),
        (
            "napcat.get_stranger_info",
            "Get a stranger's profile.",
            json!({"user_id": {"type": "number"}}),
            r#"{"user_id":10001}"#,
        ),
        (
            "napcat.get_login_info",
            "Get the logged-in account.",
            json!({}),
            r#"{}"#,
        ),
        (
            "napcat.get_version_info",
            "Get the NapCat version.",
            json!({}),
            r#"{}"#,
        ),
        (
            "napcat.get_status",
            "Get online status.",
            json!({}),
            r#"{}"#,
        ),
    ] {
        list.push(ActionSpec {
            action,
            category: Category::Query,
            summary,
            params,
            returns: "OneBot response; see NapCat docs for data fields.",
            requires_owner_intent: false,
            scope_field: None,
            target_field: None,
            destructive: false,
            examples: vec![example],
        });
    }
}

fn specs_napcat_request(list: &mut Vec<ActionSpec>) {
    // ---- napcat: request ----
    for (action, summary, params, example) in [
        (
            "napcat.set_friend_add_request",
            "Approve or reject a friend request.",
            json!({"flag": {"type": "string"}, "approve": {"type": "boolean"}, "remark": {"type": "string"}}),
            r#"{"flag":"f1","approve":true}"#,
        ),
        (
            "napcat.set_group_add_request",
            "Approve or reject a group request.",
            json!({"flag": {"type": "string"}, "sub_type": {"type": "string"}, "approve": {"type": "boolean"}, "reason": {"type": "string"}}),
            r#"{"flag":"f1","sub_type":"add","approve":true}"#,
        ),
    ] {
        list.push(ActionSpec {
            action,
            category: Category::Request,
            summary,
            params,
            returns: "OneBot response.",
            requires_owner_intent: true,
            scope_field: None,
            target_field: None,
            destructive: false,
            examples: vec![example],
        });
    }
}

fn specs_napcat_admin_destructive(list: &mut Vec<ActionSpec>) {
    // ---- napcat: admin / destructive ----
    for (action, summary, params, example) in [
        (
            "napcat.set_group_kick",
            "Kick a member from a group.",
            json!({"group_id": {"type": "number"}, "user_id": {"type": "number"}, "reject_add_request": {"type": "boolean"}}),
            r#"{"group_id":123,"user_id":10001}"#,
        ),
        (
            "napcat.set_group_ban",
            "Mute a member in a group.",
            json!({"group_id": {"type": "number"}, "user_id": {"type": "number"}, "duration": {"type": "number"}}),
            r#"{"group_id":123,"user_id":10001,"duration":600}"#,
        ),
        (
            "napcat.set_group_whole_ban",
            "Mute the whole group.",
            json!({"group_id": {"type": "number"}, "enable": {"type": "boolean"}}),
            r#"{"group_id":123,"enable":true}"#,
        ),
        (
            "napcat.set_group_leave",
            "Leave a group.",
            json!({"group_id": {"type": "number"}, "is_dismiss": {"type": "boolean"}}),
            r#"{"group_id":123}"#,
        ),
        (
            "napcat.set_group_dismiss",
            "Dismiss a group (owner only).",
            json!({"group_id": {"type": "number"}}),
            r#"{"group_id":123}"#,
        ),
        (
            "napcat.set_group_card",
            "Change a member's group card.",
            json!({"group_id": {"type": "number"}, "user_id": {"type": "number"}, "card": {"type": "string"}}),
            r#"{"group_id":123,"user_id":10001,"card":"new"}"#,
        ),
        (
            "napcat.set_group_name",
            "Change a group's name.",
            json!({"group_id": {"type": "number"}, "group_name": {"type": "string"}}),
            r#"{"group_id":123,"group_name":"new"}"#,
        ),
        (
            "napcat.set_group_admin",
            "Grant or revoke group admin.",
            json!({"group_id": {"type": "number"}, "user_id": {"type": "number"}, "enable": {"type": "boolean"}}),
            r#"{"group_id":123,"user_id":10001,"enable":true}"#,
        ),
        (
            "napcat.send_group_notice",
            "Publish a group notice.",
            json!({"group_id": {"type": "number"}, "content": {"type": "string"}}),
            r#"{"group_id":123,"content":"hello"}"#,
        ),
    ] {
        // 只有真正带 `user_id` 的动作才有“目标用户”；全员禁言/退群/解散群
        // 作用于整个群，不冒充成用户目标。
        let member_target = matches!(
            action,
            "napcat.set_group_kick"
                | "napcat.set_group_ban"
                | "napcat.set_group_card"
                | "napcat.set_group_admin"
        );
        // 收缩暴露面（踢/禁言/退群/解散）agent 可自主执行；
        // 改名片、改群名、发公告、授予权限属于扩张，需要 owner intent。
        let expands = matches!(
            action,
            "napcat.set_group_card"
                | "napcat.set_group_name"
                | "napcat.set_group_admin"
                | "napcat.send_group_notice"
                // 全员禁言/退群/解散会直接改变主人所在群的可达性，
                // 同样要求 owner intent（没有用户目标可做硬保护）。
                | "napcat.set_group_whole_ban"
                | "napcat.set_group_leave"
                | "napcat.set_group_dismiss"
        );
        list.push(ActionSpec {
            action,
            category: Category::Admin,
            summary,
            params,
            returns: "OneBot response.",
            requires_owner_intent: expands,
            scope_field: Some("group_id"),
            target_field: if member_target { Some("user_id") } else { None },
            destructive: true,
            examples: vec![example],
        });
    }
}

fn specs_napcat_file(list: &mut Vec<ActionSpec>) {
    // ---- napcat: file ----
    for (action, summary, params, example) in [
        (
            "napcat.upload_group_file",
            "Upload a file to a group.",
            json!({"group_id": {"type": "number"}, "file": {"type": "string", "description": "A VFS path, a public http(s) URL, or base64:// data."}, "name": {"type": "string"}}),
            r#"{"group_id":123,"file":"/data/x.bin","name":"x.bin"}"#,
        ),
        (
            "napcat.upload_private_file",
            "Upload a file to a friend.",
            json!({"user_id": {"type": "number"}, "file": {"type": "string", "description": "A VFS path, a public http(s) URL, or base64:// data."}, "name": {"type": "string"}}),
            r#"{"user_id":10001,"file":"/data/x.bin","name":"x.bin"}"#,
        ),
        (
            "napcat.get_group_file_url",
            "Get a group file download URL.",
            json!({"group_id": {"type": "number"}, "file_id": {"type": "string"}}),
            r#"{"group_id":123,"file_id":"abc"}"#,
        ),
        (
            "napcat.get_group_file_system_info",
            "Get group file system info.",
            json!({"group_id": {"type": "number"}}),
            r#"{"group_id":123}"#,
        ),
        (
            "napcat.delete_group_file",
            "Delete a group file.",
            json!({"group_id": {"type": "number"}, "file_id": {"type": "string"}}),
            r#"{"group_id":123,"file_id":"abc"}"#,
        ),
    ] {
        // 取下载链接与查看空间只是读取：归 Query，受 receive_scopes 约束，
        // approve 模式也不需要主人确认。上传与删除才会改变群文件。
        let read_only = action.starts_with("napcat.get_");
        list.push(ActionSpec {
            action,
            category: if read_only {
                Category::Query
            } else {
                Category::File
            },
            summary,
            params,
            returns: "OneBot response.",
            requires_owner_intent: action.starts_with("napcat.upload_"),
            scope_field: if action.contains("private") {
                Some("user_id")
            } else {
                Some("group_id")
            },
            target_field: if action.contains("private") {
                Some("user_id")
            } else {
                None
            },
            destructive: action.ends_with("delete_group_file"),
            examples: vec![example],
        });
    }
}

fn specs_napcat_profile(list: &mut Vec<ActionSpec>) {
    // ---- napcat: profile ----
    for (action, summary, params, example) in [
        (
            "napcat.set_qq_profile",
            "Update the account profile.",
            json!({"nickname": {"type": "string"}}),
            r#"{"nickname":"new"}"#,
        ),
        (
            "napcat.set_online_status",
            "Set online status.",
            json!({"status": {"type": "number"}, "ext_status": {"type": "number"}}),
            r#"{"status":10}"#,
        ),
        (
            "napcat.set_qq_avatar",
            "Change the avatar.",
            json!({"file": {"type": "string", "description": "A VFS path, a public http(s) URL, or base64:// data."}}),
            r#"{"file":"/data/a.png"}"#,
        ),
    ] {
        list.push(ActionSpec {
            action,
            category: Category::Profile,
            summary,
            params,
            returns: "OneBot response.",
            requires_owner_intent: true,
            scope_field: None,
            target_field: None,
            destructive: false,
            examples: vec![example],
        });
    }
}

fn specs_agent(list: &mut Vec<ActionSpec>) {
    // ---- agent ----
    for (action, summary) in [
        (
            "agent.status",
            "Hold state and its reason, recent security events, model, scopes and VFS usage.",
        ),
        (
            "agent.resume",
            "Re-check your current context and lift the hold if it is clean. If it refuses, \
             quarantine the offending excerpt (or clear the context) and call it again.",
        ),
        (
            "agent.quarantine",
            "Copy a suspicious excerpt (`text`, verbatim) into /quarantine/ and replace it in \
             your context with a placeholder.",
        ),
        (
            "agent.compact",
            "Run compaction now: summarise older context into a new segment. There is one \
             transcript for every chat, so it takes no parameters.",
        ),
        (
            "agent.clear_context",
            "Discard the in-memory context and start a new segment.",
        ),
    ] {
        list.push(ActionSpec {
            action,
            category: Category::Agent,
            summary,
            params: match action {
                "agent.quarantine" => json!({
                    "text": {"type": "string"},
                    "reason": {"type": "string"}
                }),
                _ => json!({}),
            },
            returns: "JSON object.",
            requires_owner_intent: false,
            scope_field: None,
            target_field: None,
            destructive: false,
            examples: vec![],
        });
    }
}

/// `local.media.image.*`：本地工具 → 媒体 → 图片。
fn specs_local_media_image(list: &mut Vec<ActionSpec>) {
    for (action, summary, params, example) in [
        (
            "local.media.image.info",
            "Inspect an image in the VFS (dimensions, format, bytes).",
            json!({"path": {"type": "string"}}),
            r#"{"path":"/tmp/a.png"}"#,
        ),
        (
            "local.media.image.transform",
            "Crop, rotate by any angle, and/or resize an image in one pass.",
            json!({
                "path": {"type": "string"},
                "out": {"type": "string"},
                "crop": {"type": "object", "properties": {
                    "x": {"type": "number"}, "y": {"type": "number"},
                    "width": {"type": "number"}, "height": {"type": "number"}
                }},
                "rotate": {"type": "number"},
                "width": {"type": "number"},
                "height": {"type": "number"},
                "max_width": {"type": "number"},
                "max_height": {"type": "number"},
                "quality": {"type": "number"}
            }),
            r#"{"path":"/tmp/a.jpg","out":"/tmp/a.png","crop":{"x":0,"y":0,"width":640,"height":640},"rotate":15,"max_width":800,"max_height":800}"#,
        ),
        (
            "local.media.image.convert",
            "Re-encode as PNG, JPEG or WebP based on the output extension; quality applies to JPEG.",
            json!({"path": {"type": "string"}, "out": {"type": "string"}, "quality": {"type": "number"}}),
            r#"{"path":"/tmp/a.png","out":"/tmp/a.jpg","quality":80}"#,
        ),
        (
            "local.media.image.text",
            "Draw one line of text on an image at any position with font, size, color and opacity. Without font, an installed font that can draw the whole text is picked; a font that lacks some of the characters is an error naming them.",
            json!({
                "path": {"type": "string"},
                "out": {"type": "string"},
                "text": {"type": "string"},
                "font": {"type": "string"},
                "x": {"type": "number"},
                "y": {"type": "number"},
                "size": {"type": "number"},
                "color": {"type": "string"},
                "opacity": {"type": "number"}
            }),
            r##"{"path":"/tmp/a.jpg","out":"/tmp/a-wm.jpg","text":"hello","font":"DejaVu Sans","x":24,"y":24,"size":48,"color":"#ffffff","opacity":0.8}"##,
        ),
        (
            "local.media.image.fonts",
            "List installed font families (paths included when available).",
            json!({"query": {"type": "string"}, "limit": {"type": "number"}}),
            r#"{"query":"sans","limit":20}"#,
        ),
    ] {
        list.push(ActionSpec {
            action,
            category: Category::Media,
            summary,
            params,
            returns: "JSON with the source/output path and image metadata.",
            requires_owner_intent: false,
            scope_field: None,
            target_field: None,
            destructive: false,
            examples: vec![example],
        });
    }
}

fn specs_local_net(list: &mut Vec<ActionSpec>) {
    // ---- local.net ----
    // 类别是 `Media`：它和图片工具一样只在 VFS 里产生文件，不是会影响 QQ 的 `File`
    // （`File` 属于对外动作，会进门禁与发送范围检查）。
    list.push(ActionSpec {
        action: "local.net.download",
        category: Category::Media,
        summary: "Download an http(s) URL into the VFS. Public hosts only: private and loopback \
                  addresses are refused, redirects are re-checked, and the size is capped. \
                  Small UTF-8 text is stored as a text file, everything else as binary. It does \
                  not send anything; use a napcat.* send for that.",
        params: json!({
            "url": {"type": "string"},
            "path": {"type": "string", "description": "Absolute VFS path to write."},
            "max_bytes": {"type": "number", "description": "Optional lower size limit."}
        }),
        returns: "JSON with path, bytes, mime, kind and hash.",
        requires_owner_intent: false,
        scope_field: None,
        target_field: None,
        destructive: false,
        examples: vec![r#"{"url":"https://example.com/a.png","path":"/tmp/a.png"}"#],
    });
}

fn specs_api(list: &mut Vec<ActionSpec>) {
    // ---- api ----
    list.push(ActionSpec {
        action: "api.list",
        category: Category::Meta,
        summary: "List registered actions, optionally filtered and paginated.",
        params: json!({
            "namespace": {"type": "string"},
            "category": {"type": "string"},
            "query": {"type": "string"},
            "offset": {"type": "number"},
            "limit": {"type": "number"}
        }),
        returns: "{total, offset, items, next_offset}.",
        requires_owner_intent: false,
        scope_field: None,
        target_field: None,
        destructive: false,
        examples: vec![r#"{"namespace":"napcat","category":"admin"}"#],
    });
    list.push(ActionSpec {
        action: "api.help",
        category: Category::Meta,
        summary: "Show one action's schema, returns, gate attributes and examples.",
        params: json!({"action": {"type": "string"}}),
        returns: "JSON object.",
        requires_owner_intent: false,
        scope_field: None,
        target_field: None,
        destructive: false,
        examples: vec![r#"{"action":"napcat.set_group_ban"}"#],
    });
}

/// 把 QQ 号参数解析成整数：兼容数字与数字字符串。
pub(crate) fn qq_number(value: &serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::Number(number) => number
            .as_u64()
            .or_else(|| number.as_i64().and_then(|value| u64::try_from(value).ok())),
        serde_json::Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

/// 从 action 参数推导目标会话（发送范围、owner intent 与跨群规则用）。
///
/// 返回 `None` 表示没有目标或目标有歧义；有作用域字段的动作此时会被拒绝。
pub(crate) fn chat_of(
    spec: &ActionSpec,
    params: &serde_json::Value,
) -> Option<crate::qq::event::ChatId> {
    use crate::qq::event::ChatId;
    // 读取一个 id 字段；0 不是合法的群号或 QQ 号。
    let id = |field: &str| params.get(field).and_then(qq_number).filter(|id| *id != 0);
    match spec.action {
        // NapCat `SendMsg` 的 `createContext`：私聊模式下 user_id 查不到且带了
        // group_id 时会改发到群里；不指定 message_type 时 group_id 优先。
        // 这里只认“恰好一个目标字段”的写法，NapCat 的选择与本函数不可能不一致。
        "napcat.send_msg" => {
            let message_type = params
                .get("message_type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            match (message_type, id("group_id"), id("user_id")) {
                ("group" | "", Some(group), None) => Some(ChatId::Group(group)),
                ("private" | "", None, Some(user)) => Some(ChatId::Private(user)),
                _ => None,
            }
        }
        // 带 group_id 的戳一戳发生在群里，作用于那个群。
        "napcat.send_poke" => id("group_id")
            .map(ChatId::Group)
            .or_else(|| id("user_id").map(ChatId::Private)),
        _ => match spec.scope_field {
            Some("group_id") => id("group_id").map(ChatId::Group),
            Some("user_id") => id("user_id").map(ChatId::Private),
            _ => None,
        },
    }
}

/// 该 action 是否携带 `message`（真正把内容发进 chat 的发送）。
///
/// 只有这类发送成功后才写一条 `self` 消息进归档；戳一戳、召回、表情回应不是消息。
pub(crate) fn carries_message(spec: &ActionSpec) -> bool {
    spec.params.get("message").is_some()
}

/// 发布动作里写给人看的正文字段：消息是 `message`，群公告是 `content`。
///
/// 发送前的内部标签剥离与 BeforeSend `rewrite` 只改这个字段；上传文件没有正文。
pub(crate) fn published_text_field(spec: &ActionSpec) -> Option<&'static str> {
    if carries_message(spec) {
        return Some("message");
    }
    match spec.action {
        "napcat.send_group_notice" => Some("content"),
        _ => None,
    }
}

/// 发布动作的统一视图：形如 `{"message": ...}`，供 `[gate]` 的发送规则与发送记录使用。
///
/// 发送规则（重复、群发、搬运、媒体来源、@全体、每 chat 上限）原本只认消息的
/// `message` 字段；群公告与上传文件同样把内容公开到 chat，这里把它们投影成
/// 同一形状——公告正文是一段文本，上传的文件是一个 `file` 段——规则因此不必
/// 为每种动作各写一份。不公开内容的动作（戳一戳、召回、群管理……）返回 `None`。
pub(crate) fn published_view<'a>(
    spec: &ActionSpec,
    params: &'a serde_json::Value,
) -> Option<std::borrow::Cow<'a, serde_json::Value>> {
    use std::borrow::Cow;
    // 字段缺失时投影成 null：规则看到的是“空内容”，与原始请求一致。
    let field = |name: &str| params.get(name).cloned().unwrap_or_default();
    if carries_message(spec) {
        return Some(Cow::Borrowed(params));
    }
    match spec.action {
        "napcat.send_group_notice" => Some(Cow::Owned(json!({"message": field("content")}))),
        "napcat.upload_group_file" | "napcat.upload_private_file" => Some(Cow::Owned(json!({
            "message": [{"type": "file", "data": {"file": field("file"), "name": field("name")}}]
        }))),
        _ => None,
    }
}
