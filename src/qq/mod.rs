//! QQ 接入层。
//!
//! 设计依据：设计 §4 / §10。
//!
//! - [`client`]：NapCatLink 封装与 action 调用；
//! - [`event`]：事件归一化；
//! - [`format`](mod@format)：入站信封、内部标签剥离、出站消息段；
//! - [`filter`]：准入门（拒收）；
//! - [`ledger`]：发送账本，区分本进程发出的消息与同账号上手打的消息。

pub mod client;
pub mod event;
pub mod filter;
pub mod format;
pub mod ledger;

pub use client::{LoginInfo, QqClient};
pub use event::{ChatId, QqEvent, QqMessage, Segment, Sender};
pub use filter::{FilterVerdict, MessageFilter};
