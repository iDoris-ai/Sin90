//! 业务半边 —— 你主要改的就是这里。
//!
//! 内核的受约束代理是**透明转发**：`/api/v1/<name>/*` 原样转给这个进程。所以
//! 路由**写相对路径**（`/directions`，不是 `/api/v1/sin90/directions`）——
//! 命名空间由内核加，在这里再写一遍会变成 `/api/v1/sin90/api/v1/sin90/...`。
//!
//! 这一层的形状**不会因为 ME-3 怎么定而改变**，所以现在就可以写。

/// Sin90 的三层：方向 / 节奏 / 任务。
///
/// 这里只留形状；真正的 handler 按你的框架（axum / actix / 任意）来写。
pub const ROUTES: &[(&str, &str)] = &[
    ("POST|GET", "/directions"),
    ("POST|GET", "/schedule-blocks"),
    ("PATCH", "/schedule-blocks/{id}"),
    ("POST|GET", "/proposals"),
    ("GET", "/proposals/{id}"),
    ("POST", "/proposals/{id}/accept"),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// 路由必须是相对路径。这条判据存在的原因是：写成绝对路径不会报错，只会
    /// 让每一条路由在运行时多一层前缀 —— 而那要等到真正装载时才看得见。
    #[test]
    fn routes_are_relative_not_namespaced() {
        for (_, path) in ROUTES {
            assert!(path.starts_with('/'), "{path}");
            assert!(
                !path.starts_with("/api/"),
                "{path} 带了命名空间前缀；内核会再加一次"
            );
        }
        // 正对照：判据不是恒真的 —— 一个带前缀的路径确实会被上面那条抓到。
        assert!("/api/v1/sin90/directions".starts_with("/api/"));
    }
}
