//! 剪贴板来源应用。
//!
//! 能力差异很大，设置页必须如实标注：
//!
//! | 平台 | 可靠度 |
//! |---|---|
//! | Windows | `GetClipboardOwner` → PID → exe，可靠 |
//! | macOS | 拿不到写入者，只能用变更时的前台应用做启发式 |
//! | Wayland | 不可用 |
//!
//! 所以这里返回的是 `Option<String>`，调用方不能假设它一定存在。

pub trait SourceAppResolver: Send + Sync {
    /// 返回可执行文件名（如 `KeePassXC.exe`），无法判断时返回 `None`。
    fn resolve(&self) -> Option<String>;
}

/// 平台不支持时使用。排除应用功能在这类平台上必须显示为"不可用"。
#[derive(Debug, Default, Clone, Copy)]
pub struct NoSourceApp;

impl SourceAppResolver for NoSourceApp {
    fn resolve(&self) -> Option<String> {
        None
    }
}
