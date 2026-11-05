//! LLM 公共类型：与具体协议无关的用量与停止原因。
//!
//! 放在 `agent` 之外是为了让协议适配层不必依赖 agent：`providers` 与 `agent`
//! 都只依赖这里，依赖方向保持单向。

pub mod types;
