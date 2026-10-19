//! 平台边界。
//!
//! 主 crate 只依赖这里的 trait，不直接 `use` 任何第三方实现。
//! 这样 `clipboard-rs` 的 Wayland 后端、`global-hotkey` 的 fork 分支
//! 都被挡在边界外：换实现不动业务代码，也不必把未验证的依赖
//! 提升成编译期硬依赖。
//!
//! 这一层**不认识存储层的类型**。监听侧产出 [`RawClipboard`]，
//! 由应用层转换成 `CapturedItem` 再入库。

pub mod clipboard;
pub mod echo;
pub mod hotkey;
pub mod input;
pub mod menu;
pub mod source;
pub mod system;
pub mod tray;
pub mod window;

pub use clipboard::{ClipboardBackend, RawClipboard, Watcher};
pub use echo::EchoGuard;
pub use hotkey::{HotkeyBackend, HotkeySpec, Modifiers};
pub use input::{foreground_is_ours, foreground_window, paste_into};
pub use menu::set_menu_dark_mode;
pub use source::{NoSourceApp, SourceAppResolver};
pub use system::{cursor_placement, CursorPlacement, SystemClipboard, SystemHotkey, SystemSourceApp};
pub use tray::{SystemTray, TrayEvent};
pub use window::window_rect;

use thiserror::Error;

pub type PlatformResult<T> = std::result::Result<T, PlatformError>;

#[derive(Debug, Error)]
pub enum PlatformError {
    #[error("平台能力不可用：{0}")]
    Unsupported(&'static str),
    #[error("剪贴板读取失败：{0}")]
    Clipboard(String),
    #[error("热键注册失败：{0}")]
    Hotkey(String),
    #[error("格式错误：{0}")]
    Malformed(String),
    #[error("托盘：{0}")]
    Tray(String),
    #[error("io：{0}")]
    Io(#[from] std::io::Error),
}
