//! 25zai 存储层。
//!
//! 分层约定（改动前先读这一段）：
//!
//! 1. [`ClipboardStore`] —— 纯同步接口，不持有通道、不依赖 GPUI。
//!    实现可以是 redb，也可以是单测里的内存实现。
//! 2. [`StoreCommand`] / [`StoreHandle`] —— 命令通道，UI 侧非阻塞投递。
//! 3. [`service`] —— 独占 `Box<dyn ClipboardStore>` 的线程，把返回值
//!    翻译成 [`StoreEvent`] 推给 UI。
//!
//! 第 3 层存在的意义：trait 本身永远不产生事件、不写通道，
//! 所以任何一条不变量都能在单测里直接对拍，不需要跑线程。

pub mod redb_store;
pub mod schema;
pub mod sensitive;
pub mod service;
pub mod traits;
pub mod types;
pub mod wire;

pub use redb_store::RedbStore;
pub use sensitive::{HeuristicDetector, SecretDetector};
pub use service::StoreService;
pub use traits::{
    ClipboardStore, Reply, StoreCommand, StoreError, StoreHandle, StoreResult,
};
pub use wire::{to_command, Msg, Remote, UiRequest, WireReply, WireRequest};
pub use types::*;
