//! 握手半边 —— 桩。
//!
//! Agent24 的 `initialize` 线格式是 ME-3b-2b，今天还没写。已经落地的只有版本
//! 协商（纯函数）和 framing（NDJSON 单帧 + 单行上限）。
//!
//! **这个文件故意保持不能编译成能用的东西**：照一个不存在的协议写实现，等于
//! 给自己安排一次返工。等 Agent24 v0.5.0 的模块侧 SDK（或 wire 文档定稿）
//! 出来再填 —— 跟踪 Agent24 的 T0.2。
//!
//! 已经能确定的两条约束，写业务代码时就可以按它们来：
//!
//! 1. **单帧上限 1 MiB，超限断连、不降级。** 别把大 payload 塞进单帧。
//! 2. **版本区间协商；不报区间 = 不兼容。** 沉默不会被当成同意 —— 你的
//!    `initialize` 必须报 `[min, max]`。

/// 本模块能说的协议版本区间。到定稿时由 SDK 校验。
pub const PROTOCOL_MIN: u32 = 1;
/// 见 [`PROTOCOL_MIN`]。
pub const PROTOCOL_MAX: u32 = 1;

#[cfg(test)]
mod tests {
    use super::*;

    /// 区间本身不能是倒的。看着琐碎，但「不报区间 = 不兼容」意味着这两个数是
    /// 模块唯一的握手声明 —— 写反了，失败会发生在连接建立时，而不是这里。
    #[test]
    fn the_declared_range_is_not_inverted() {
        assert!(PROTOCOL_MIN <= PROTOCOL_MAX);
    }
}
