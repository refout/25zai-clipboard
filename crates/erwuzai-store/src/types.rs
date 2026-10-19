//! 存储层的数据类型。纯数据，不依赖 redb、不依赖 GPUI。
//!
//! 关键区分：[`EntryRecord`] 是落盘内容（永远不因排序变化而重写），
//! [`EntryView`] 是"记录 + 它在 `usage_order` 里的位置"，只在返回给调用方时组装。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 条目 id。单调递增，删除后不复用。
pub type EntryId = u64;

/// 使用顺序序号。单调递增，列表顺序的唯一依据。
pub type Seq = u64;

/// BLAKE3-256 内容哈希。
pub type ContentHash = [u8; 32];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Text,
    RichText,
    Image,
    FileList,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::RichText => "rich_text",
            Kind::Image => "image",
            Kind::FileList => "file_list",
        }
    }

    /// 二进制载荷不进 `texts`，只走 blob。
    pub fn is_binary(self) -> bool {
        matches!(self, Kind::Image)
    }
}

/// 落盘的内容元数据。**不包含 seq / last_used_at**。
///
/// 重新复制、回放、清理都不会重写这条记录（清理是删除，不是修改）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryRecord {
    pub id: EntryId,
    pub hash: ContentHash,
    pub kind: Kind,
    /// 截断到 `PREVIEW_CHARS` 个字符；文件列表存"前 3 个文件名 + 总数"。
    /// 敏感条目在写入时即完成脱敏，库里不留原文。
    pub preview: String,
    /// 首次采集时间（Unix 毫秒）。**不是**最近使用时间。
    pub created_at: i64,
    pub source_app: Option<String>,
    pub pinned: bool,
    /// 图片的 blob 哈希；非图片为 `None`。
    pub blob_ref: Option<ContentHash>,
    /// 载荷超过 `Config::max_text_bytes` 被截断。
    pub truncated: bool,
}

/// 返回给 UI 的视图：记录 + 当前位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryView {
    pub entry: EntryRecord,
    /// 在 `usage_order` 中的位置。
    pub seq: Seq,
    /// 最近一次被使用（采集命中 / 回放）的时间，Unix 毫秒。
    /// 由 `usage_order` 的槽位提供，因此比 `entry.created_at` 新。
    pub last_used_at: i64,
    /// 图片条目的 blob 文件路径。列表要画缩略图，必须能直接读到文件；
    /// 走存储线程再取一次正文会把每一行都变成一次跨线程往返。
    /// 非图片为 `None`。
    pub blob_path: Option<PathBuf>,
}

/// 监听线程交给存储线程的原始载荷（已解码，未哈希）。
#[derive(Debug, Clone)]
pub enum Payload {
    Text(String),
    RichText {
        plain: String,
        html: Option<String>,
    },
    /// `png` 是已编码的 PNG 字节，存储层只负责落盘，不解码。
    Image {
        png: Vec<u8>,
        width: u32,
        height: u32,
    },
    FileList(Vec<PathBuf>),
}

impl Payload {
    pub fn kind(&self) -> Kind {
        match self {
            Payload::Text(_) => Kind::Text,
            Payload::RichText { .. } => Kind::RichText,
            Payload::Image { .. } => Kind::Image,
            Payload::FileList(_) => Kind::FileList,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CapturedItem {
    pub payload: Payload,
    /// 来源进程名（Windows 可靠，macOS 为启发式，Wayland 为 `None`）。
    pub source_app: Option<String>,
    /// Unix 毫秒。存储层会做单调钳制，见 `schema` 中的不变量 I5。
    pub captured_at: i64,
}

/// 回放时读出的完整内容。
#[derive(Debug, Clone)]
pub enum Content {
    Text(String),
    RichText {
        plain: String,
        html: Option<String>,
    },
    FileList(Vec<PathBuf>),
    Image {
        path: PathBuf,
        width: u32,
        height: u32,
    },
}

#[derive(Debug, Clone, Default)]
pub struct Page {
    pub items: Vec<EntryView>,
    /// 下一页游标（本页最后一条的 seq）。`None` 表示到底。
    pub next_cursor: Option<Seq>,
    /// 当前可见条目总数（含固定）。虚拟列表用它估算滚动高度，
    /// 避免"分页 + 虚拟列表"两套加载路径。
    pub total_visible: usize,
}

#[derive(Debug, Clone)]
pub enum CaptureOutcome {
    /// 新内容入库。`evicted` 是本次清理删掉的条目。
    Inserted {
        entry: EntryView,
        evicted: Vec<EntryId>,
    },
    /// 去重命中，内容记录原封不动，只换了一个更靠前的 seq。
    Moved { entry: EntryView },
    Skipped(SkipReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// 空内容。
    Empty,
    /// 超过 `max_text_bytes` 且策略为丢弃。
    TooLarge,
    /// `secretshape` 判定为凭据，且 `Config::skip_sensitive` 为真。
    Sensitive,
    /// 命中 `Config::excluded_apps`。
    ExcludedApp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchOutcome {
    Moved { seq: Seq, last_used_at: i64 },
    /// `Config::replay_reorders == false`，顺序未变。
    Unchanged,
    NotFound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PopupPosition {
    Cursor,
    Center,
    Remembered,
}

// 含 f32（字号缩放），所以只能 PartialEq，不能 Eq。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 非固定条目上限。固定条目**不占**这个配额。
    pub limit: usize,
    /// 可选的时间清理；与 `limit` 同时生效，先到先删。
    pub retention_days: Option<u32>,
    /// 敏感内容默认直接丢弃，而不是"脱敏后保留"。
    pub skip_sensitive: bool,
    /// `skip_sensitive == false` 时，预览是否脱敏。
    pub mask_sensitive_preview: bool,
    pub excluded_apps: Vec<String>,
    /// 回放 / 重新复制是否把条目排到第一。
    pub replay_reorders: bool,
    /// 回放时只写纯文本，丢掉 HTML / RTF 格式。
    pub replay_as_plain: bool,
    /// 点条目后直接把内容粘贴到唤起前的窗口（写完剪贴板补一次 Ctrl+V）。
    ///
    /// 结构体整体标了 `#[serde(default)]`，缺字段时用的是 **字段类型** 的默认值
    /// （bool 就是 false），所以这里必须单独给个 true，否则老配置读进来这个
    /// 功能是关的。
    #[serde(default = "default_true")]
    pub paste_on_click: bool,
    /// 单条文本 / 路径列表上限，超出截断并标记 `truncated`。
    pub max_text_bytes: usize,
    pub hotkey: String,
    /// 自定义托盘图标路径；`None` 用内置图标。
    pub tray_icon: Option<String>,
    /// 明亮 / 黑暗 / 跟随系统。
    pub theme: ThemeMode,
    /// 主题色（选中高亮、强调按钮）。
    pub accent: [u8; 3],
    /// 全局字号缩放，1.0 为标准。
    pub font_scale: f32,
    pub popup_position: PopupPosition,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            limit: 500,
            retention_days: None,
            skip_sensitive: true,
            mask_sensitive_preview: true,
            excluded_apps: Vec::new(),
            replay_reorders: true,
            replay_as_plain: false,
            paste_on_click: true,
            max_text_bytes: 1024 * 1024,
            hotkey: "Alt+V".to_string(),
            tray_icon: None,
            theme: ThemeMode::System,
            // 和默认托盘图标同一个蓝，视觉上是一套
            accent: [58, 110, 165],
            font_scale: 1.0,
            popup_position: PopupPosition::Cursor,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    pub total_entries: usize,
    pub pinned_entries: usize,
    /// 数据库文件字节数（redb 是 COW，删除不会自动缩小）。
    pub db_bytes: u64,
    /// blob 目录字节数。
    pub blob_bytes: u64,
}

/// 服务线程推给 UI 的事件。UI 只消费事件，不自行查库确认。
#[derive(Debug, Clone)]
pub enum StoreEvent {
    Added(Vec<EntryView>),
    Moved {
        id: EntryId,
        seq: Seq,
        last_used_at: i64,
    },
    Removed(Vec<EntryId>),
    ConfigChanged(Box<Config>),
    /// 非致命问题：通道丢弃、孤儿 blob、快捷键注册失败等。
    Warning(String),
}
