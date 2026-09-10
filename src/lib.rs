//! Sin90 —— 个人助理的领域 OS，同时是「怎么写一个 Agent24 领域 OS」的参考实现。
//!
//! 两个半边，状态不同，所以分成两个模块：
//!
//! - [`routes`]：业务半边。**现在就能写**，形状不会因 ME-3 怎么定而改变。
//! - [`handshake`]：握手半边。**桩** —— 协议未定稿，见 `docs/STATUS.md`。

pub mod handshake;
pub mod routes;
pub mod store;
