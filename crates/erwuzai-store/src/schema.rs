//! redb 表定义与槽位编码。
//!
//! 值统一用 `&[u8]` 承载 postcard 编码的结构体，键用定长整型或 `&[u8]`。
//! 这样不依赖 redb 的自定义 Value derive，跨版本升级时面最小。
//!
//! # 不变量（任何写事务提交后必须同时成立）
//!
//! - **I1** `seq_index` 的键集 == `usage_order` 槽位里的 entry_id 集，
//!   且 `seq_index[id] == seq` 与 `usage_order[seq].entry_id == id` 互为逆。
//! - **I2** `content_index` 的键集 == `entries` 的 hash 集，一一对应。
//! - **I3** `texts` 的键集 ⊆ `entries` 的键集。
//! - **I4** `meta[non_pinned_count] == count(entries where !pinned)`。
//! - **I5** seq 与 last_used_at 同向单调：`seq_a < seq_b ⇒ last_used_at_a <= last_used_at_b`。
//!   靠写入时把时间戳钳制成 `max(now, 当前最大 last_used_at)` 来抵御时钟回拨。
//!   这条不变量是"时间清理 = 前缀扫描"的前提，不能省。
//! - **I6** `meta[cleanup_cursor]` 只前进不后退。
//!
//! 违反 I1–I4 中的任意一条都会让清理、去重、回放中的至少一个出错，
//! 所以所有涉及多表的写入必须在**同一个写事务**内完成。

use redb::TableDefinition;

use crate::types::{EntryId, Seq};

/// 落盘的 `EntryRecord`（postcard）。
pub const ENTRIES: TableDefinition<u64, &[u8]> = TableDefinition::new("entries");

/// 全文 / 完整路径列表。列表查询不触达这张表。
pub const TEXTS: TableDefinition<u64, &[u8]> = TableDefinition::new("texts");

/// BLAKE3 → entry_id。去重入口。
pub const CONTENT_INDEX: TableDefinition<&[u8], u64> = TableDefinition::new("content_index");

/// seq → 16 字节槽位 `{ entry_id, last_used_at }`。列表顺序的唯一依据。
pub const USAGE_ORDER: TableDefinition<u64, &[u8]> = TableDefinition::new("usage_order");

/// entry_id → seq。反向索引，回放和清理都需要它。
pub const SEQ_INDEX: TableDefinition<u64, u64> = TableDefinition::new("seq_index");

/// 固定条目的 entry_id 集合。
///
/// 存 id 而不是 seq：`touch` 会把条目的 seq 换掉，id 不会。
pub const PINNED: TableDefinition<u64, u64> = TableDefinition::new("pinned");

/// 计数器与版本号。
pub const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

/// 单条配置（整块 postcard）。
pub const CONFIG: TableDefinition<&str, &[u8]> = TableDefinition::new("config");

/// 当前 schema 版本。加表 / 改槽位布局时递增，并写迁移分支。
pub const SCHEMA_VERSION: u32 = 1;

pub mod meta_key {
    pub const SCHEMA_VERSION: &str = "schema_version";
    pub const NEXT_ENTRY_ID: &str = "next_entry_id";
    pub const NEXT_SEQ: &str = "next_seq";
    pub const NON_PINNED_COUNT: &str = "non_pinned_count";
    pub const CLEANUP_CURSOR: &str = "cleanup_cursor";
    /// 已分配的最大 last_used_at，用于抵御时钟回拨（不变量 I5）。
    pub const LAST_TS: &str = "last_ts";
}

pub const CONFIG_KEY: &str = "config";

/// `usage_order` 的 16 字节槽位：entry_id(8, BE) + last_used_at(8, BE)。
///
/// 把 last_used_at 放在这里而不是 `entries`，是因为 `entries` 必须不可变
/// （回放只改顺序，不该重写整条记录）。副作用是时间清理也不必回表。
pub const USAGE_SLOT_LEN: usize = 16;

pub fn encode_usage(entry_id: EntryId, last_used_at: i64) -> [u8; USAGE_SLOT_LEN] {
    let mut out = [0u8; USAGE_SLOT_LEN];
    out[..8].copy_from_slice(&entry_id.to_be_bytes());
    out[8..].copy_from_slice(&last_used_at.to_be_bytes());
    out
}

pub fn decode_usage(bytes: &[u8]) -> Option<(EntryId, i64)> {
    if bytes.len() != USAGE_SLOT_LEN {
        return None;
    }
    let entry_id = u64::from_be_bytes(bytes[..8].try_into().ok()?);
    let last_used_at = i64::from_be_bytes(bytes[8..].try_into().ok()?);
    Some((entry_id, last_used_at))
}

/// `meta` 里的 u64 值编码。
pub fn encode_u64(v: u64) -> [u8; 8] {
    v.to_be_bytes()
}

pub fn decode_u64(bytes: &[u8]) -> Option<u64> {
    bytes.try_into().ok().map(u64::from_be_bytes)
}

/// 有序表遍历时的分页单位。
pub type PageCursor = Option<Seq>;
