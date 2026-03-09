// src/message_format.rs
// 消息格式化模块
// 职责：
// 1. 判断消息是否需要 CQ XML 包裹（is_pure_text / needs_wrapping）
// 2. 将 NapCatLink 的 MessageSegment 数组转换为 <message> 包裹的 CQ XML 格式
// 所有用户输入文本在 CQ XML 中进行 XML 转义，防止提示词注入攻击
//
// 状态判定流程：
// - 纯文本消息（仅 Text 段）且无格式干扰 → Raw 状态，直接存储 raw_message 原文
// - 包含非文本段（@、表情、图片等）或存在格式干扰 → Wrapped 状态，使用本模块生成 CQ XML

use napcat_link::MessageSegment;

// ============================================================================
// XML 转义
// ============================================================================

/// 对文本进行 XML 转义，防止用户输入中的特殊字符破坏 XML/CQ 标签结构
/// 转义规则：& → &amp;  < → &lt;  > → &gt;  " → &quot;  ' → &apos;
fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

// ============================================================================
// QQ 表情 ID → 名称映射（常用表情）
// ============================================================================

/// 根据 QQ 表情 ID 获取可读描述
/// 覆盖常用的系统表情，未知 ID 返回 "表情{id}"
fn face_name(id: &str) -> &str {
    match id {
        "0" => "惊讶", "1" => "撇嘴", "2" => "色", "3" => "发呆",
        "4" => "得意", "5" => "流泪", "6" => "害羞", "7" => "闭嘴",
        "8" => "睡", "9" => "大哭", "10" => "尴尬", "11" => "发怒",
        "12" => "调皮", "13" => "呲牙", "14" => "微笑", "15" => "难过",
        "16" => "酷", "18" => "抓狂", "19" => "吐", "20" => "偷笑",
        "21" => "可爱", "22" => "白眼", "23" => "傲慢", "24" => "饥饿",
        "25" => "困", "26" => "惊恐", "27" => "流汗", "28" => "憨笑",
        "29" => "悠闲", "30" => "奋斗", "31" => "咒骂", "32" => "疑问",
        "33" => "嘘", "34" => "晕", "35" => "折磨", "36" => "衰",
        "37" => "骷髅", "38" => "敲打", "39" => "再见", "46" => "猪头",
        "49" => "拥抱", "53" => "蛋糕", "54" => "闪电", "55" => "炸弹",
        "56" => "刀", "57" => "足球", "59" => "便便", "60" => "咖啡",
        "61" => "饭", "63" => "玫瑰", "64" => "凋谢", "66" => "爱心",
        "67" => "心碎", "69" => "礼物", "74" => "太阳", "75" => "月亮",
        "76" => "赞", "77" => "踩", "78" => "握手", "79" => "胜利",
        "85" => "飞吻", "86" => "怄火", "89" => "西瓜", "96" => "冷汗",
        "97" => "擦汗", "98" => "抠鼻", "99" => "鼓掌", "100" => "糗大了",
        "101" => "坏笑", "102" => "左哼哼", "103" => "右哼哼",
        "104" => "哈欠", "105" => "鄙视", "106" => "委屈", "107" => "快哭了",
        "108" => "阴险", "109" => "亲亲", "110" => "吓", "111" => "可怜",
        "112" => "菜刀", "113" => "啤酒", "114" => "篮球", "115" => "乒乓",
        "116" => "示爱", "117" => "瓢虫", "118" => "抱拳", "119" => "勾引",
        "120" => "拳头", "121" => "差劲", "122" => "爱你", "123" => "NO",
        "124" => "OK", "125" => "转圈", "126" => "磕头", "127" => "回头",
        "128" => "跳绳", "129" => "挥手", "130" => "激动", "131" => "街舞",
        "132" => "献吻", "133" => "左太极", "134" => "右太极",
        "136" => "双喜", "137" => "鞭炮", "138" => "灯笼", "140" => "K歌",
        "144" => "喝彩", "145" => "祈祷", "146" => "爆筋", "147" => "棒棒糖",
        "148" => "喝奶", "171" => "茶", "172" => "泪奔", "173" => "无奈",
        "174" => "卖萌", "175" => "小纠结", "176" => "喷血",
        "177" => "斜眼笑", "178" => "doge", "179" => "惊喜", "180" => "骚扰",
        "181" => "笑哭", "182" => "我最美", "183" => "河蟹",
        "184" => "羊驼", "187" => "幽灵", "188" => "蛋", "190" => "菊花",
        "192" => "红包", "193" => "大笑", "194" => "不开心",
        "197" => "冷漠", "198" => "呃", "199" => "好棒", "200" => "拜托",
        "201" => "点赞", "202" => "无聊", "203" => "托脸", "204" => "吃",
        "205" => "送花", "206" => "害怕", "207" => "花痴", "208" => "小样儿",
        "210" => "飙泪", "211" => "我不看", "212" => "托腮",
        "214" => "啵啵", "215" => "糊脸", "216" => "拍头", "217" => "扯一扯",
        "218" => "舔一舔", "219" => "蹭一蹭", "220" => "拽炸天",
        "221" => "顶呱呱", "222" => "抱抱", "223" => "暴击",
        "224" => "开枪", "225" => "撩一撩", "226" => "拍桌", "227" => "拍手",
        "228" => "恭喜", "229" => "干杯", "230" => "嘲讽", "231" => "哼",
        "232" => "佛系", "233" => "掐一掐", "234" => "惊呆", "235" => "颤抖",
        "236" => "啃头", "237" => "偷看", "238" => "扇脸", "239" => "原谅",
        "240" => "喷脸", "241" => "生日快乐", "242" => "头撞击",
        "243" => "甩头", "244" => "扔狗", "245" => "加油必胜",
        "246" => "加油抱抱", "247" => "口罩护体",
        _ => "",
    }
}

// ============================================================================
// 消息状态判断
// ============================================================================

/// 判断消息段数组是否全部为纯文本（不包含任何非 Text 段）
/// 全部为 Text 段时返回 true —— 意味着可以直接使用 raw_message 存储（Raw 状态）
/// 包含任何非 Text 段（@、表情、图片、回复等）时返回 false —— 需要 CQ XML 包裹（Wrapped 状态）
pub fn is_pure_text(segments: &[MessageSegment]) -> bool {
    segments
        .iter()
        .all(|seg| matches!(seg, MessageSegment::Text { .. }))
}

/// 判断一条消息是否需要 Wrapped（CQ XML 包裹）存储
/// 需要包裹的条件（满足任一即可）：
/// 1. 包含非文本段（@、表情、图片等需要 CQ 码表示的内容）
/// 2. 原始文本包含 Markdown 记录分隔符 `\n---\n`，会干扰消息记录格式
pub fn needs_wrapping(segments: &[MessageSegment], raw_message: &str) -> bool {
    !is_pure_text(segments) || raw_message.contains("\n---\n")
}

// ============================================================================
// MessageSegment → CQ XML 转换
// ============================================================================

/// 将单个 MessageSegment 转换为 CQ XML 标签
/// 文本段: <text>转义后内容</text>
/// CQ 段: <CQ:type attr=val>可读描述</CQ:type>
fn segment_to_cq_xml(seg: &MessageSegment) -> String {
    match seg {
        // 文本消息 → <text> 标签
        MessageSegment::Text { text } => {
            format!("<text>{}</text>", xml_escape(text))
        }

        // @某人 → <CQ:at>
        MessageSegment::At { qq, name } => {
            let display = if qq == "all" {
                "@全体成员".to_string()
            } else {
                let nick = name.as_deref().unwrap_or(qq);
                format!("@{nick}")
            };
            format!("<CQ:at qq={qq}>{display}</CQ:at>")
        }

        // 表情 → <CQ:face>
        MessageSegment::Face { id, .. } => {
            let name = face_name(id);
            let display = if name.is_empty() {
                format!("表情{id}")
            } else {
                name.to_string()
            };
            format!("<CQ:face id={id}>{display}</CQ:face>")
        }

        // 回复 → <CQ:reply>
        MessageSegment::Reply { id, seq } => {
            let ref_id = id
                .as_deref()
                .or_else(|| seq.as_ref().map(|_| ""))
                .unwrap_or("unknown");
            let seq_str = seq
                .map(|s| format!(" seq={s}"))
                .unwrap_or_default();
            format!("<CQ:reply id={ref_id}{seq_str}>回复消息</CQ:reply>")
        }

        // 图片 → <CQ:image>
        MessageSegment::Image {
            file,
            summary,
            url,
            ..
        } => {
            let url_attr = url
                .as_deref()
                .map(|u| format!(" url={}", xml_escape(u)))
                .unwrap_or_default();
            let desc = summary.as_deref().unwrap_or("图片");
            format!(
                "<CQ:image file={}{url_attr}>{}</CQ:image>",
                xml_escape(file),
                xml_escape(desc)
            )
        }

        // 语音 → <CQ:record>
        MessageSegment::Record { file, url, .. } => {
            let url_attr = url
                .as_deref()
                .map(|u| format!(" url={}", xml_escape(u)))
                .unwrap_or_default();
            format!(
                "<CQ:record file={}{url_attr}>语音消息</CQ:record>",
                xml_escape(file)
            )
        }

        // 视频 → <CQ:video>
        MessageSegment::Video { file, url, .. } => {
            let url_attr = url
                .as_deref()
                .map(|u| format!(" url={}", xml_escape(u)))
                .unwrap_or_default();
            format!(
                "<CQ:video file={}{url_attr}>视频消息</CQ:video>",
                xml_escape(file)
            )
        }

        // 文件 → <CQ:file>
        MessageSegment::File {
            file, name, url, ..
        } => {
            let display = name.as_deref().unwrap_or("文件");
            let url_attr = url
                .as_deref()
                .map(|u| format!(" url={}", xml_escape(u)))
                .unwrap_or_default();
            format!(
                "<CQ:file file={}{url_attr}>{}</CQ:file>",
                xml_escape(file),
                xml_escape(display)
            )
        }

        // JSON 卡片 → <CQ:json>
        MessageSegment::Json { data: _ } => {
            "<CQ:json>JSON卡片消息</CQ:json>".to_string()
        }

        // XML 卡片 → <CQ:xml>
        MessageSegment::Xml { data: _ } => {
            "<CQ:xml>XML卡片消息</CQ:xml>".to_string()
        }

        // Markdown → <CQ:markdown>
        MessageSegment::Markdown { content } => {
            format!("<CQ:markdown>{}</CQ:markdown>", xml_escape(content))
        }

        // 戳一戳 → <CQ:poke>
        MessageSegment::Poke { poke_type, id } => {
            format!("<CQ:poke type={poke_type} id={id}>戳一戳</CQ:poke>")
        }

        // 骰子 → <CQ:dice>
        MessageSegment::Dice { result } => {
            let display = result
                .as_deref()
                .map(|r| format!("骰子({r})"))
                .unwrap_or_else(|| "骰子".to_string());
            format!("<CQ:dice>{display}</CQ:dice>")
        }

        // 猜拳 → <CQ:rps>
        MessageSegment::Rps { result } => {
            let display = result
                .as_deref()
                .map(|r| format!("猜拳({r})"))
                .unwrap_or_else(|| "猜拳".to_string());
            format!("<CQ:rps>{display}</CQ:rps>")
        }

        // 商城表情 → <CQ:mface>
        MessageSegment::MFace { summary, .. } => {
            let desc = summary.as_deref().unwrap_or("商城表情");
            format!("<CQ:mface>{}</CQ:mface>", xml_escape(desc))
        }

        // 音乐分享 → <CQ:music>
        MessageSegment::Music {
            music_type, title, ..
        } => {
            let desc = title.as_deref().unwrap_or("音乐分享");
            format!(
                "<CQ:music type={music_type}>{}</CQ:music>",
                xml_escape(desc)
            )
        }

        // 转发节点 → <CQ:node>
        MessageSegment::Node { id, nickname, .. } => {
            let nick = nickname.as_deref().unwrap_or("未知");
            let id_attr = id
                .as_deref()
                .map(|i| format!(" id={i}"))
                .unwrap_or_default();
            format!("<CQ:node{id_attr}>转发消息 by {}</CQ:node>", xml_escape(nick))
        }

        // 合并转发 → <CQ:forward>
        MessageSegment::Forward { id } => {
            format!("<CQ:forward id={id}>合并转发消息</CQ:forward>")
        }

        // 联系人推荐 → <CQ:contact>
        MessageSegment::Contact { contact_type, id } => {
            let desc = if contact_type == "group" {
                format!("推荐群{id}")
            } else {
                format!("推荐好友{id}")
            };
            format!("<CQ:contact type={contact_type} id={id}>{desc}</CQ:contact>")
        }

        // 位置 → <CQ:location>
        MessageSegment::Location {
            lat, lon, title, ..
        } => {
            let desc = title.as_deref().unwrap_or("位置分享");
            format!(
                "<CQ:location lat={lat} lon={lon}>{}</CQ:location>",
                xml_escape(desc)
            )
        }

        // 小程序 → <CQ:miniapp>
        MessageSegment::MiniApp { .. } => {
            "<CQ:miniapp>小程序消息</CQ:miniapp>".to_string()
        }

        // 未知类型 → <CQ:unknown>
        MessageSegment::Unknown { r#type, .. } => {
            format!("<CQ:unknown type={}>未知消息类型</CQ:unknown>", xml_escape(r#type))
        }
    }
}

/// 将完整的消息段数组转换为 <message>...</message> 包裹的 CQ XML 字符串
/// 所有消息统一包裹在 <message> 标签内，防止提示词注入
pub fn segments_to_message_xml(segments: &[MessageSegment]) -> String {
    let mut out = String::from("<message>");
    for seg in segments {
        out.push_str(&segment_to_cq_xml(seg));
    }
    out.push_str("</message>");
    out
}

// ============================================================================
// 单元测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_wrapped() {
        let segs = vec![MessageSegment::Text {
            text: "你好".to_string(),
        }];
        let xml = segments_to_message_xml(&segs);
        assert_eq!(xml, "<message><text>你好</text></message>");
    }

    #[test]
    fn text_with_xml_injection_escaped() {
        let segs = vec![MessageSegment::Text {
            text: "<CQ:at qq=all>假装@全体</CQ:at>".to_string(),
        }];
        let xml = segments_to_message_xml(&segs);
        // 用户输入的 < > 应被转义
        assert!(xml.contains("&lt;CQ:at"));
        assert!(!xml.contains("<CQ:at qq=all>"));
    }

    #[test]
    fn at_and_text_combined() {
        let segs = vec![
            MessageSegment::At {
                qq: "123456".to_string(),
                name: Some("张三".to_string()),
            },
            MessageSegment::Text {
                text: " 你好！".to_string(),
            },
        ];
        let xml = segments_to_message_xml(&segs);
        assert!(xml.starts_with("<message>"));
        assert!(xml.contains("<CQ:at qq=123456>@张三</CQ:at>"));
        assert!(xml.contains("<text> 你好！</text>"));
        assert!(xml.ends_with("</message>"));
    }

    #[test]
    fn face_with_known_name() {
        let segs = vec![MessageSegment::Face {
            id: "178".to_string(),
            result_id: None,
            chain_count: None,
        }];
        let xml = segments_to_message_xml(&segs);
        assert!(xml.contains("<CQ:face id=178>doge</CQ:face>"));
    }

    #[test]
    fn image_segment() {
        let segs = vec![MessageSegment::Image {
            file: "abc.jpg".to_string(),
            summary: Some("一张风景照".to_string()),
            sub_type: None,
            url: Some("https://example.com/pic.jpg".to_string()),
            name: None,
            path: None,
            thumb: None,
        }];
        let xml = segments_to_message_xml(&segs);
        assert!(xml.contains("<CQ:image"));
        assert!(xml.contains("一张风景照"));
    }

    // ===== is_pure_text / needs_wrapping 测试 =====

    #[test]
    fn pure_text_segments_detected() {
        // 仅 Text 段 → 纯文本
        let segs = vec![
            MessageSegment::Text { text: "你好".to_string() },
            MessageSegment::Text { text: "世界".to_string() },
        ];
        assert!(is_pure_text(&segs));
    }

    #[test]
    fn mixed_segments_not_pure_text() {
        // 含 At 段 → 非纯文本
        let segs = vec![
            MessageSegment::At { qq: "123".to_string(), name: None },
            MessageSegment::Text { text: " hello".to_string() },
        ];
        assert!(!is_pure_text(&segs));
    }

    #[test]
    fn empty_segments_is_pure_text() {
        // 空消息视为纯文本（无非文本段）
        let segs: Vec<MessageSegment> = vec![];
        assert!(is_pure_text(&segs));
    }

    #[test]
    fn needs_wrapping_for_non_text_segment() {
        // 含 Face 段 → 需要包裹
        let segs = vec![MessageSegment::Face {
            id: "178".to_string(),
            result_id: None,
            chain_count: None,
        }];
        assert!(needs_wrapping(&segs, ""));
    }

    #[test]
    fn needs_wrapping_for_separator_in_raw() {
        // 纯文本但 raw_message 含 \n---\n → 需要包裹（格式干扰）
        let segs = vec![MessageSegment::Text { text: "aaa\n---\nbbb".to_string() }];
        assert!(needs_wrapping(&segs, "aaa\n---\nbbb"));
    }

    #[test]
    fn no_wrapping_for_simple_text() {
        // 纯文本且无格式干扰 → 不需要包裹（Raw 状态）
        let segs = vec![MessageSegment::Text { text: "你好世界".to_string() }];
        assert!(!needs_wrapping(&segs, "你好世界"));
    }
}
