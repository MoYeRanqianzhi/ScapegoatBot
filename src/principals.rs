//! 身份三元组：登录账号、主人、主人的私线。
//!
//! 这个 agent 无人值守地运行，主人**只通过 QQ** 与它交互，所以“哪条 QQ 消息是主人
//! 在对 agent 说话”必须有唯一、可验证的定义。以前这个判断散落在 runner、monitor 与
//! 唤醒预设里，各写各的；stand_in 模式下主人等于本账号，主人的消息全是 `is_self`，
//! 结果主人根本没有渠道对 agent 说话。这里把定义集中起来：
//!
//! - `account_qq`：NapCat 登录的账号；它发出的消息（`is_self`）永远不是主人的声音，
//!   因为它们与模型通过 API 发出的消息无法区分。
//! - `owner_qq`：主人。companion 模式下是另一个账号；stand_in 模式下就是本账号
//!   （除非配置显式指定）。
//! - `master_qq`：主人的私线——主人用来给 agent 发指令、接收报告的账号。
//!   `[agent].master = "owner"`（默认）时等于 `owner_qq`。
//!
//! “主人之声”（owner voice）= 非 `is_self`、发送者不是本账号，并且
//! 发送者是 `owner_qq`（任何会话），或者是 `master_qq` 且在与它的私聊里。

use anyhow::Result;

use crate::config::{AgentMode, SbotConfig};
use crate::qq::event::{ChatId, QqMessage};

/// 解析后的身份三元组；全部是 QQ 号，0 表示“没有”。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Principals {
    /// 本账号（NapCat 登录的 QQ）。
    pub account_qq: u64,
    /// 主人。
    pub owner_qq: u64,
    /// 主人的私线：报告发到这里，这里的私聊也算主人在说话。
    pub master_qq: u64,
}

impl Principals {
    /// 按配置与登录账号解析。
    ///
    /// - stand_in：`owner_qq` 缺省为本账号；
    /// - companion：`owner_qq` 必须非 0 且不同于本账号；
    /// - `master`：`"owner"` = `owner_qq`，`"self"` = 本账号，其余是 QQ 号。
    pub fn resolve(config: &SbotConfig, account_qq: u64) -> Result<Self> {
        let agent = &config.agent;
        let owner_qq = match agent.mode {
            AgentMode::StandIn => {
                if agent.owner_qq != 0 {
                    agent.owner_qq
                } else {
                    account_qq
                }
            }
            AgentMode::Companion => {
                if agent.owner_qq == 0 {
                    anyhow::bail!("companion mode requires a non-zero owner_qq");
                }
                if account_qq != 0 && agent.owner_qq == account_qq {
                    anyhow::bail!("companion mode requires owner_qq to differ from the account");
                }
                agent.owner_qq
            }
        };
        let master_qq = match agent.master.as_str() {
            "owner" => owner_qq,
            "self" => account_qq,
            other => other.parse().map_err(|_| {
                anyhow::anyhow!("agent.master must be \"owner\", \"self\" or a QQ number")
            })?,
        };
        Ok(Self {
            account_qq,
            owner_qq,
            master_qq,
        })
    }

    /// `qq` 是否是“另一个账号”：非 0，且不是本账号。
    fn is_other_account(&self, qq: u64) -> bool {
        qq != 0 && qq != self.account_qq
    }

    /// 这条消息是不是主人在对 agent 说话（见模块文档）。
    pub fn is_owner_voice(&self, message: &QqMessage) -> bool {
        if message.is_self {
            return false;
        }
        let Some(sender) = message.sender.qq else {
            return false;
        };
        if !self.is_other_account(sender) {
            return false;
        }
        if sender == self.owner_qq {
            return true;
        }
        sender == self.master_qq && message.chat == ChatId::Private(sender)
    }

    /// 主人有没有任何能让 agent 听见的渠道。
    ///
    /// stand_in 且 `master` 指向本账号时为 `false`：主人只能在宿主机上用控制通道。
    pub fn owner_can_reach_agent(&self) -> bool {
        self.is_other_account(self.owner_qq) || self.is_other_account(self.master_qq)
    }

    /// 报告/通知能不能真正送到主人手里（私线是另一个账号）。
    pub fn has_private_line(&self) -> bool {
        self.is_other_account(self.master_qq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qq::event::Sender;

    fn message(chat: ChatId, sender: u64, is_self: bool) -> QqMessage {
        QqMessage {
            message_id: 1,
            chat,
            sender: Sender {
                qq: Some(sender),
                nickname: String::new(),
                card: String::new(),
                role: String::new(),
            },
            time: chrono::Utc::now(),
            segments: Vec::new(),
            reply_to: None,
            is_self,
            backfilled: false,
            raw: serde_json::Value::Null,
        }
    }

    fn config(mode: AgentMode, owner_qq: u64, master: &str) -> SbotConfig {
        let mut config = SbotConfig::default();
        config.agent.mode = mode;
        config.agent.owner_qq = owner_qq;
        config.agent.master = master.to_owned();
        config
    }

    #[test]
    fn stand_in_owner_speaks_through_the_master_private_line() {
        let principals =
            Principals::resolve(&config(AgentMode::StandIn, 0, "20000"), 10000).unwrap();
        assert_eq!(principals.owner_qq, 10000);
        // 私线私聊：主人之声。
        assert!(principals.is_owner_voice(&message(ChatId::Private(20000), 20000, false)));
        // 私线账号在群里说话：只是普通群成员。
        assert!(!principals.is_owner_voice(&message(ChatId::Group(1), 20000, false)));
        // 主人在手机上手打（is_self）：不是对 agent 说话。
        assert!(!principals.is_owner_voice(&message(ChatId::Private(30000), 10000, true)));
        assert!(principals.owner_can_reach_agent());
        assert!(principals.has_private_line());
    }

    #[test]
    fn stand_in_with_master_self_has_no_qq_channel() {
        for master in ["owner", "self"] {
            let principals =
                Principals::resolve(&config(AgentMode::StandIn, 0, master), 10000).unwrap();
            assert_eq!(principals.master_qq, 10000);
            assert!(!principals.owner_can_reach_agent());
            assert!(!principals.has_private_line());
            // 即使伪造成非 self 的“本账号消息”也不算。
            assert!(!principals.is_owner_voice(&message(ChatId::Private(10000), 10000, false)));
        }
    }

    #[test]
    fn companion_owner_speaks_anywhere_and_master_defaults_to_owner() {
        let principals =
            Principals::resolve(&config(AgentMode::Companion, 20000, "owner"), 10000).unwrap();
        assert_eq!(principals.master_qq, 20000);
        assert!(principals.is_owner_voice(&message(ChatId::Group(1), 20000, false)));
        assert!(principals.is_owner_voice(&message(ChatId::Private(20000), 20000, false)));
        assert!(!principals.is_owner_voice(&message(ChatId::Group(1), 30000, false)));
        assert!(principals.has_private_line());
    }

    #[test]
    fn companion_rejects_a_missing_or_self_owner() {
        assert!(Principals::resolve(&config(AgentMode::Companion, 0, "owner"), 10000).is_err());
        assert!(Principals::resolve(&config(AgentMode::Companion, 10000, "owner"), 10000).is_err());
    }
}
