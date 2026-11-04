//! `API` 工具的 action 注册表。
//!
//! 这里只有数据：action 名、参数 schema、分级、作用域与示例。执行逻辑在
//! `api.rs`；拆开是为了让 40 个 action 的表与门禁/发送逻辑各自可读。
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
    pub examples: &'static [&'static str],
}

impl ActionSpec {
    pub fn namespace(&self) -> &'static str {
        self.action.split_once('.').map(|(ns, _)| ns).unwrap_or("")
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
        examples,
    }
}

/// 全部内置 action。
pub fn specs() -> &'static [ActionSpec] {
    use std::sync::OnceLock;
    static SPECS: OnceLock<Vec<ActionSpec>> = OnceLock::new();
    SPECS.get_or_init(|| {
        let mut list = Vec::new();

        // ---- napcat: message ----
        list.push(send_spec(
            "napcat.send_group_msg",
            "Send a message to a group.",
            "group_id",
            json!({
                "group_id": {"type": "number"},
                "message": {"description": "Message segment array or plain string."}
            }),
            &[r#"{"group_id":123,"message":[{"type":"text","data":{"text":"hello"}}]}"#],
        ));
        list.push(send_spec(
            "napcat.send_private_msg",
            "Send a message to a friend.",
            "user_id",
            json!({
                "user_id": {"type": "number"},
                "message": {"description": "Message segment array or plain string."}
            }),
            &[r#"{"user_id":10001,"message":"hello"}"#],
        ));
        list.push(send_spec(
            "napcat.send_msg",
            "Send a message by message_type/chat target.",
            "user_id",
            json!({
                "message_type": {"type": "string"},
                "user_id": {"type": "number"},
                "group_id": {"type": "number"},
                "message": {"description": "Message segment array or plain string."}
            }),
            &[],
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
            examples: &[r#"{"message_id":55555}"#],
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
            examples: &[r#"{"message_id":55555,"emoji_id":"128077"}"#],
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
            examples: &[r#"{"user_id":10001,"group_id":123}"#],
        });

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
                examples: Box::leak(Box::new([example])),
            });
        }

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
                examples: Box::leak(Box::new([example])),
            });
        }

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
                examples: Box::leak(Box::new([example])),
            });
        }

        // ---- napcat: file ----
        for (action, summary, params, example) in [
            (
                "napcat.upload_group_file",
                "Upload a file to a group.",
                json!({"group_id": {"type": "number"}, "file": {"type": "string"}, "name": {"type": "string"}}),
                r#"{"group_id":123,"file":"/data/x.bin","name":"x.bin"}"#,
            ),
            (
                "napcat.upload_private_file",
                "Upload a file to a friend.",
                json!({"user_id": {"type": "number"}, "file": {"type": "string"}, "name": {"type": "string"}}),
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
            list.push(ActionSpec {
                action,
                category: Category::File,
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
                examples: Box::leak(Box::new([example])),
            });
        }

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
                json!({"file": {"type": "string"}}),
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
                examples: Box::leak(Box::new([example])),
            });
        }

        // ---- agent ----
        for (action, summary) in [
            ("agent.status", "Hold state, recent verdicts, budget usage and scopes."),
            ("agent.resume", "Re-check the current context; clears the hold when it passes."),
            ("agent.quarantine", "Move a suspicious excerpt into /quarantine/ and mark it non-executable."),
            ("agent.compact", "Run compaction now."),
            ("agent.clear_context", "Discard the in-memory context and start a new segment."),
        ] {
            list.push(ActionSpec {
                action,
                category: Category::Agent,
                summary,
                params: if action == "agent.quarantine" {
                    json!({
                        "message_id": {"type": "number"},
                        "text": {"type": "string"},
                        "reason": {"type": "string"}
                    })
                } else {
                    json!({})
                },
                returns: "JSON object.",
                requires_owner_intent: false,
                scope_field: None,
                target_field: None,
                destructive: false,
                examples: &[],
            });
        }

        // ---- media (local, no NapCat) ----
        for (action, summary, params, example) in [
            (
                "media.image_info",
                "Inspect an image in the VFS (dimensions, format, bytes).",
                json!({"path": {"type": "string"}}),
                r#"{"path":"/tmp/a.png"}"#,
            ),
            (
                "media.image_resize",
                "Resize an image into the VFS to exactly width x height.",
                json!({"path": {"type": "string"}, "out": {"type": "string"}, "width": {"type": "number"}, "height": {"type": "number"}}),
                r#"{"path":"/tmp/a.jpg","out":"/tmp/a-small.jpg","width":640,"height":640}"#,
            ),
            (
                "media.image_crop",
                "Crop an image into the VFS.",
                json!({"path": {"type": "string"}, "out": {"type": "string"}, "x": {"type": "number"}, "y": {"type": "number"}, "width": {"type": "number"}, "height": {"type": "number"}}),
                r#"{"path":"/tmp/a.jpg","out":"/tmp/face.jpg","x":100,"y":50,"width":512,"height":512}"#,
            ),
            (
                "media.image_watermark",
                "Draw an ASCII text watermark near the bottom-right corner.",
                json!({"path": {"type": "string"}, "out": {"type": "string"}, "text": {"type": "string"}}),
                r#"{"path":"/tmp/a.jpg","out":"/tmp/wm.jpg","text":"sbot"}"#,
            ),
            (
                "media.image_rotate",
                "Rotate an image clockwise by 90/180/270 degrees.",
                json!({"path": {"type": "string"}, "out": {"type": "string"}, "degrees": {"type": "number"}}),
                r#"{"path":"/tmp/a.jpg","out":"/tmp/a-rot.jpg","degrees":90}"#,
            ),
            (
                "media.image_thumbnail",
                "Fit an image inside max_width x max_height, preserving aspect ratio.",
                json!({"path": {"type": "string"}, "out": {"type": "string"}, "max_width": {"type": "number"}, "max_height": {"type": "number"}}),
                r#"{"path":"/tmp/a.jpg","out":"/tmp/thumb.jpg","max_width":512,"max_height":512}"#,
            ),
            (
                "media.image_compress",
                "Re-encode an image as JPEG with a quality value (1-100).",
                json!({"path": {"type": "string"}, "out": {"type": "string"}, "quality": {"type": "number"}}),
                r#"{"path":"/tmp/a.png","out":"/tmp/a.jpg","quality":80}"#,
            ),
            (
                "media.image_convert",
                "Convert an image (png/jpeg/webp) into the VFS.",
                json!({"path": {"type": "string"}, "out": {"type": "string"}}),
                r#"{"path":"/tmp/a.webp","out":"/tmp/a.png"}"#,
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
                examples: Box::leak(Box::new([example])),
            });
        }

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
            examples: &[r#"{"namespace":"napcat","category":"admin"}"#],
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
            examples: &[r#"{"action":"napcat.set_group_ban"}"#],
        });

        list
    })
}
