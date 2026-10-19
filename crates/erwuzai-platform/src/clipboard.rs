//! 剪贴板读写与监听。
//!
//! 关键约束：[`ClipboardBackend::watch`] 的回调运行在平台自己的线程上，
//! 实现方不得在回调里做哈希、解码或数据库操作——只搬字节。

use std::path::PathBuf;
use std::sync::Arc;

use crate::PlatformResult;

/// 平台读出来的原始内容，尚未哈希、尚未判断敏感。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawClipboard {
    Text(String),
    Rich {
        plain: String,
        html: Option<String>,
    },
    Files(Vec<PathBuf>),
    /// 已编码的 PNG，避免在监听线程上解码。
    Image {
        png: Vec<u8>,
        width: u32,
        height: u32,
    },
}

pub trait ClipboardBackend: Send + Sync {
    fn read(&self) -> PlatformResult<Option<RawClipboard>>;

    fn write(&self, content: &RawClipboard) -> PlatformResult<()>;

    /// 平台是否提供单调的剪贴板序列号。
    ///
    /// Windows 用 `GetClipboardSequenceNumber`，macOS 用 `changeCount`。
    /// 返回 `None` 的平台只能靠哈希窗口做回声判定（Linux）。
    fn sequence(&self) -> Option<u64>;

    /// 启动监听。回调只在内容发生变化时被调用。
    fn watch(
        &self,
        sink: Arc<dyn Fn(RawClipboard) + Send + Sync>,
    ) -> PlatformResult<Box<dyn Watcher>>;
}

pub trait Watcher: Send {
    fn stop(&mut self);
}
