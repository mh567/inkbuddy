//! AI 额度插件的核心逻辑：快照数据结构、文本格式化、app-server 客户端。
//!
//! 协议循环与页面渲染由后续任务接入。

pub mod app_server;
pub mod formatting;
pub mod locate;
pub mod model;
pub mod render;
pub mod rpc;
