//! QQ 接入层。
//!
//! 设计依据：设计 §4 / §10。
//!
//! - [`client`]：NapCatLink 封装与 action 调用；
//! - [`event`]：事件归一化；
//! - [`format`]：入站信封、内部标签剥离、出站消息段；
//! - [`filter`]：准入门（拒收）；
//! - [`resync`]：断线补消息与游标。

pub mod client;
pub mod event;
pub mod filter;
pub mod format;
pub mod resync;

pub use client::{LoginInfo, QqClient};
pub use event::{ChatId, QqEvent, QqMessage, Segment, Sender};
pub use filter::{FilterVerdict, MessageFilter};
