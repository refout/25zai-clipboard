//! `ClipboardStore` 与命令通道。
//!
//! trait 本身是同步的、无事件的、可替换的：实现方只回答"做了什么"，
//! 不负责通知谁。事件由服务线程根据返回值生成，这样存储层可以在
//! 单测里被内存实现整体替换，而不需要搭建通道和线程。

use std::sync::mpsc::{channel, Receiver, Sender};

use crate::types::*;

pub type StoreResult<T> = std::result::Result<T, StoreError>;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Db(#[from] redb::Error),
    #[error("database error: {0}")]
    Database(#[from] redb::DatabaseError),
    #[error("transaction error: {0}")]
    Transaction(#[from] redb::TransactionError),
    #[error("table error: {0}")]
    Table(#[from] redb::TableError),
    #[error("storage error: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("commit error: {0}")]
    Commit(#[from] redb::CommitError),
    /// 磁盘文件由更新版本的 redb 写入。调用方必须备份旧文件、
    /// 建新库或引导迁移，**不允许静默失败或自动删库**。
    #[error("database format upgrade required")]
    UpgradeRequired,
    #[error("database is already open in this process")]
    AlreadyOpen,
    #[error("codec error: {0}")]
    Codec(String),
    /// 来自守护进程的错误（UI 只是原样展示）。
    #[error("remote error: {0}")]
    Remote(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// 存储层契约。
///
/// 实现方保证：任何方法返回 `Ok` 之后，[schema] 里的 I1–I6 全部成立；
/// 返回 `Err` 之后，库内容与调用前一致（写事务要么全提交要么全回滚）。
///
/// 所有方法都是 `&self`：redb 的事务入口本身是 `&`，且实现只会在
/// 单一服务线程上被调用，不需要 `Sync`。
pub trait ClipboardStore: Send {
    /// 库的 schema 版本，供启动时判断迁移。
    fn schema_version(&self) -> u32;

    /// 采集一条内容。内部完成：尺寸检查 → 敏感检测 → BLAKE3 → 去重
    /// → 插入或移动 → 清理。
    ///
    /// 去重命中时**不**重写 `entries` / `texts`，只换 seq。
    fn capture(&self, item: CapturedItem) -> StoreResult<CaptureOutcome>;

    /// 按使用顺序倒序取一页。`cursor == None` 取首屏。
    ///
    /// 实现必须在**同一个读事务**内完成"扫 usage_order + 读 entries"，
    /// 否则中间发生清理会读到指向已删除条目的 seq。
    fn page(&self, cursor: Option<Seq>, limit: usize) -> StoreResult<Page>;

    /// 全量历史搜索，按使用顺序倒序返回。只匹配 `preview`。
    fn search(&self, query: &str, limit: usize) -> StoreResult<Vec<EntryView>>;

    /// 回放用：读出完整内容。
    fn get_content(&self, id: EntryId) -> StoreResult<Option<Content>>;

    /// 图片 blob 的落盘路径。纯函数，不需要事务。
    fn blob_path(&self, hash: ContentHash) -> Option<std::path::PathBuf>;

    /// 把条目移到最新的 seq。重新复制和回放共用这一条路径。
    ///
    /// `Config::replay_reorders == false` 时返回 `Unchanged`。
    fn touch(&self, id: EntryId) -> StoreResult<TouchOutcome>;

    /// 固定 / 取消固定。固定条目不计入上限、不被清理。
    ///
    /// 返回因取消固定而触发清理删掉的条目（取消固定会让非固定计数上升）。
    fn set_pinned(&self, id: EntryId, pinned: bool) -> StoreResult<Vec<EntryId>>;

    /// 删除单条。返回是否真的存在。
    fn delete(&self, id: EntryId) -> StoreResult<bool>;

    /// 清空所有非固定条目，返回被删除的 id。
    fn clear_unpinned(&self) -> StoreResult<Vec<EntryId>>;

    fn config(&self) -> StoreResult<Config>;

    /// 整块替换配置。若收紧上限会立即触发清理，返回被删条目。
    fn update_config(&self, config: Config) -> StoreResult<Vec<EntryId>>;

    fn stats(&self) -> StoreResult<Stats>;

    /// 启动时兜底：删除没有被任何 `entries` 引用的 blob 文件。
    fn sweep_orphan_blobs(&self) -> StoreResult<usize>;
}

// ---------------------------------------------------------------------------
// 命令通道
// ---------------------------------------------------------------------------

/// 一次调用的回执。`Receiver` 阻塞，GPUI 侧用 `background_spawn` 消费，
/// 不要在渲染线程上 `recv()`。
pub type Reply<T> = Receiver<StoreResult<T>>;

pub enum StoreCommand {
    Capture {
        item: CapturedItem,
        reply: Sender<StoreResult<CaptureOutcome>>,
    },
    Page {
        cursor: Option<Seq>,
        limit: usize,
        reply: Sender<StoreResult<Page>>,
    },
    Search {
        query: String,
        limit: usize,
        reply: Sender<StoreResult<Vec<EntryView>>>,
    },
    Content {
        id: EntryId,
        reply: Sender<StoreResult<Option<Content>>>,
    },
    Touch {
        id: EntryId,
        reply: Sender<StoreResult<TouchOutcome>>,
    },
    SetPinned {
        id: EntryId,
        pinned: bool,
        reply: Sender<StoreResult<Vec<EntryId>>>,
    },
    Delete {
        id: EntryId,
        reply: Sender<StoreResult<bool>>,
    },
    ClearUnpinned {
        reply: Sender<StoreResult<Vec<EntryId>>>,
    },
    GetConfig {
        reply: Sender<StoreResult<Config>>,
    },
    UpdateConfig {
        config: Config,
        reply: Sender<StoreResult<Vec<EntryId>>>,
    },
    Stats {
        reply: Sender<StoreResult<Stats>>,
    },
}

impl StoreCommand {
    pub fn capture(item: CapturedItem) -> (Self, Reply<CaptureOutcome>) {
        let (tx, rx) = channel();
        (StoreCommand::Capture { item, reply: tx }, rx)
    }

    pub fn page(cursor: Option<Seq>, limit: usize) -> (Self, Reply<Page>) {
        let (tx, rx) = channel();
        (StoreCommand::Page { cursor, limit, reply: tx }, rx)
    }

    pub fn search(query: impl Into<String>, limit: usize) -> (Self, Reply<Vec<EntryView>>) {
        let (tx, rx) = channel();
        (StoreCommand::Search { query: query.into(), limit, reply: tx }, rx)
    }

    pub fn content(id: EntryId) -> (Self, Reply<Option<Content>>) {
        let (tx, rx) = channel();
        (StoreCommand::Content { id, reply: tx }, rx)
    }

    pub fn touch(id: EntryId) -> (Self, Reply<TouchOutcome>) {
        let (tx, rx) = channel();
        (StoreCommand::Touch { id, reply: tx }, rx)
    }

    pub fn set_pinned(id: EntryId, pinned: bool) -> (Self, Reply<Vec<EntryId>>) {
        let (tx, rx) = channel();
        (StoreCommand::SetPinned { id, pinned, reply: tx }, rx)
    }

    pub fn delete(id: EntryId) -> (Self, Reply<bool>) {
        let (tx, rx) = channel();
        (StoreCommand::Delete { id, reply: tx }, rx)
    }

    pub fn clear_unpinned() -> (Self, Reply<Vec<EntryId>>) {
        let (tx, rx) = channel();
        (StoreCommand::ClearUnpinned { reply: tx }, rx)
    }

    pub fn get_config() -> (Self, Reply<Config>) {
        let (tx, rx) = channel();
        (StoreCommand::GetConfig { reply: tx }, rx)
    }

    pub fn update_config(config: Config) -> (Self, Reply<Vec<EntryId>>) {
        let (tx, rx) = channel();
        (StoreCommand::UpdateConfig { config, reply: tx }, rx)
    }

    pub fn stats() -> (Self, Reply<Stats>) {
        let (tx, rx) = channel();
        (StoreCommand::Stats { reply: tx }, rx)
    }
}

/// UI 侧持有的命令入口。克隆是廉价的（共享同一个 `Sender`）。
///
/// 服务线程构造时返回 `(StoreHandle, Receiver<StoreEvent>)`：
/// 命令下行走 `StoreHandle`，事件上行走独立的 `Receiver`。
#[derive(Clone)]
pub struct StoreHandle {
    tx: Option<Sender<StoreCommand>>,
    remote: Option<std::sync::Arc<crate::wire::Remote>>,
}

impl StoreHandle {
    pub fn new(tx: Sender<StoreCommand>) -> Self {
        Self {
            tx: Some(tx),
            remote: None,
        }
    }

    /// 拆分进程之后 UI 用的入口：命令走 TCP 到守护进程。
    pub fn remote(remote: std::sync::Arc<crate::wire::Remote>) -> Self {
        Self {
            tx: None,
            remote: Some(remote),
        }
    }

    /// 返回 `false` 表示服务线程（或守护进程连接）已断，调用方应停止重试。
    pub fn submit(&self, cmd: StoreCommand) -> bool {
        match (&self.tx, &self.remote) {
            (Some(tx), _) => tx.send(cmd).is_ok(),
            (_, Some(remote)) => remote.submit(cmd),
            _ => false,
        }
    }

    /// 写剪贴板前后的回声抑制。只有拆分模式有意义——本地模式下监听线程
    /// 和写剪贴板在同一个进程里，走的是共享的 `EchoGuard`。
    pub fn arm_echo(&self, hash: Option<[u8; 32]>) -> bool {
        match &self.remote {
            Some(remote) => remote.arm_echo(hash),
            None => false,
        }
    }

    /// 同步托盘菜单的明暗（Win32 菜单是进程级的，托盘在守护进程里）。
    pub fn set_menu_dark(&self, dark: bool) -> bool {
        match &self.remote {
            Some(remote) => remote.set_menu_dark(dark),
            None => false,
        }
    }

    /// 退出整个程序。本地模式下没有守护进程可退，返回 `false`。
    pub fn quit(&self) -> bool {
        match &self.remote {
            Some(remote) => remote.quit(),
            None => false,
        }
    }

    /// 改全局快捷键（守护进程持有热键）。
    pub fn set_hotkey(&self, spec: &str) -> std::result::Result<(), String> {
        match &self.remote {
            Some(remote) => remote.set_hotkey(spec),
            None => Err("本地模式不支持改键".into()),
        }
    }
}
