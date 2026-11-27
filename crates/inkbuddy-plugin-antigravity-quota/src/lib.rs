//! Antigravity 额度插件的核心逻辑。
//!
//! 数据来自本机正在运行的反重力语言服务器：先按进程命令行找到端口和 CSRF
//! 令牌，再向回环地址发两次 Connect RPC（`RetrieveUserQuotaSummary` 取额度，
//! `GetUserStatus` 取套餐名）。
//!
//! 命令行版（`agy`）的令牌留在进程内，外部读不到，此时退到它自己的
//! `agy -p "/usage"` 命令取额度。

pub mod cli;
pub mod discovery;
pub mod formatting;
pub mod http;
pub mod model;
pub mod quota;
pub mod render;
pub mod service;
pub mod store;
