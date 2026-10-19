//! `ClipboardStore` 的 redb 实现。
//!
//! 所有跨表写入都在单个写事务内完成，保证 [`crate::schema`] 的 I1–I6。
//! 图片 blob 先落盘、后提交；删除则相反，提交成功后才删文件。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, Table};
use serde::{Deserialize, Serialize};

use crate::schema::{
    decode_u64, decode_usage, encode_u64, encode_usage, meta_key, CONFIG, CONFIG_KEY,
    CONTENT_INDEX, ENTRIES, META, PINNED, SCHEMA_VERSION, SEQ_INDEX, TEXTS, USAGE_ORDER,
};
use crate::sensitive::{HeuristicDetector, SecretDetector};
use crate::traits::{ClipboardStore, StoreError, StoreResult};
use crate::types::*;

const PREVIEW_CHARS: usize = 500;
/// 正文搜索最多扫描多少条。解码整段文本比匹配预览贵一个数量级，
/// 没有上限的话一次冷搜索会把存储线程占住。
const MAX_BODY_SCAN: usize = 2000;
/// 硬上限：超过直接丢弃，避免一次 500 MB 粘贴把进程打爆。
const HARD_MAX_BYTES: usize = 32 * 1024 * 1024;

/// `texts` 表里存的内容体。图片只存尺寸，像素在 blob 文件里。
#[derive(Debug, Clone, Serialize, Deserialize)]
enum StoredBody {
    Text(String),
    Rich {
        plain: String,
        html: Option<String>,
    },
    Files(Vec<PathBuf>),
    Image {
        width: u32,
        height: u32,
    },
}

struct Prepared {
    kind: Kind,
    body: StoredBody,
    preview: String,
    hash_input: Vec<u8>,
    blob_bytes: Option<Vec<u8>>,
    truncated: bool,
    sensitive: bool,
}

enum PreparedOutcome {
    Ready(Prepared),
    Skip(SkipReason),
}

pub struct RedbStore {
    db: Database,
    db_path: PathBuf,
    blob_dir: PathBuf,
    detector: Arc<dyn SecretDetector>,
}

impl RedbStore {
    pub fn open(
        db_path: impl AsRef<Path>,
        blob_dir: impl AsRef<Path>,
        detector: Arc<dyn SecretDetector>,
    ) -> StoreResult<Self> {
        let db_path = db_path.as_ref().to_path_buf();
        let blob_dir = blob_dir.as_ref().to_path_buf();

        if let Some(parent) = db_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::create_dir_all(&blob_dir)?;

        let db = match Database::builder().create(&db_path) {
            Ok(db) => db,
            Err(e) => {
                let e: redb::Error = e.into();
                return Err(match e {
                    redb::Error::DatabaseAlreadyOpen => StoreError::AlreadyOpen,
                    redb::Error::UpgradeRequired(_) => StoreError::UpgradeRequired,
                    other => StoreError::Db(other),
                });
            }
        };

        let store = Self { db, db_path, blob_dir, detector };
        store.ensure_schema()?;
        Ok(store)
    }

    /// 用占位检测器打开。生产环境应改用 `open` 注入 secretshape 实现。
    pub fn open_default(
        db_path: impl AsRef<Path>,
        blob_dir: impl AsRef<Path>,
    ) -> StoreResult<Self> {
        Self::open(db_path, blob_dir, Arc::new(HeuristicDetector))
    }

    fn ensure_schema(&self) -> StoreResult<()> {
        let w = self.db.begin_write()?;
        {
            let mut meta = w.open_table(META)?;
            let version = match meta.get(meta_key::SCHEMA_VERSION)? {
                Some(g) => decode_u64(g.value()).unwrap_or(0),
                None => 0,
            };
            if version == 0 {
                meta.insert(meta_key::SCHEMA_VERSION, &encode_u64(SCHEMA_VERSION as u64)[..])?;
                meta.insert(meta_key::NEXT_ENTRY_ID, &encode_u64(1)[..])?;
                meta.insert(meta_key::NEXT_SEQ, &encode_u64(1)[..])?;
                meta.insert(meta_key::NON_PINNED_COUNT, &encode_u64(0)[..])?;
                meta.insert(meta_key::CLEANUP_CURSOR, &encode_u64(0)[..])?;
                meta.insert(meta_key::LAST_TS, &encode_u64(0)[..])?;
            } else if version > SCHEMA_VERSION as u64 {
                return Err(StoreError::Codec(format!(
                    "数据库 schema v{version} 高于程序支持的 v{SCHEMA_VERSION}"
                )));
            }
        }
        w.commit()?;

        // 建表：读事务打开不存在的表会报错，所以启动时先确保七张表都存在。
        let w = self.db.begin_write()?;
        {
            w.open_table(ENTRIES)?;
            w.open_table(TEXTS)?;
            w.open_table(CONTENT_INDEX)?;
            w.open_table(USAGE_ORDER)?;
            w.open_table(SEQ_INDEX)?;
            w.open_table(PINNED)?;
            w.open_table(CONFIG)?;
        }
        w.commit()?;
        Ok(())
    }

    fn prepare(&self, payload: Payload, cfg: &Config) -> StoreResult<PreparedOutcome> {
        let kind = payload.kind();
        match payload {
            Payload::Text(text) => {
                if text.trim().is_empty() {
                    return Ok(PreparedOutcome::Skip(SkipReason::Empty));
                }
                if text.len() > HARD_MAX_BYTES {
                    return Ok(PreparedOutcome::Skip(SkipReason::TooLarge));
                }
                let sensitive = self.detector.looks_sensitive(&text);
                let (stored, truncated) = truncate_bytes(&text, cfg.max_text_bytes);
                Ok(PreparedOutcome::Ready(Prepared {
                    kind,
                    body: StoredBody::Text(stored.to_string()),
                    preview: preview_of(stored),
                    // 哈希用未截断的全文：尾部不同的两条内容不会因为截断而误判重复。
                    hash_input: text.as_bytes().to_vec(),
                    blob_bytes: None,
                    truncated,
                    sensitive,
                }))
            }
            Payload::RichText { plain, html } => {
                if plain.trim().is_empty() && html.is_none() {
                    return Ok(PreparedOutcome::Skip(SkipReason::Empty));
                }
                if plain.len() > HARD_MAX_BYTES {
                    return Ok(PreparedOutcome::Skip(SkipReason::TooLarge));
                }
                let sensitive = self.detector.looks_sensitive(&plain);
                let (stored, truncated) = truncate_bytes(&plain, cfg.max_text_bytes);
                let mut hash_input = plain.as_bytes().to_vec();
                hash_input.push(0);
                if let Some(h) = &html {
                    hash_input.extend_from_slice(h.as_bytes());
                }
                Ok(PreparedOutcome::Ready(Prepared {
                    kind,
                    body: StoredBody::Rich {
                        plain: stored.to_string(),
                        html,
                    },
                    preview: preview_of(stored),
                    hash_input,
                    blob_bytes: None,
                    truncated,
                    sensitive,
                }))
            }
            Payload::FileList(paths) => {
                if paths.is_empty() {
                    return Ok(PreparedOutcome::Skip(SkipReason::Empty));
                }
                let mut hash_input = Vec::new();
                for p in &paths {
                    hash_input.extend_from_slice(p.to_string_lossy().as_bytes());
                    hash_input.push(0);
                }
                if hash_input.len() > HARD_MAX_BYTES {
                    return Ok(PreparedOutcome::Skip(SkipReason::TooLarge));
                }
                let preview = files_preview(&paths);
                Ok(PreparedOutcome::Ready(Prepared {
                    kind,
                    body: StoredBody::Files(paths),
                    preview,
                    hash_input,
                    blob_bytes: None,
                    truncated: false,
                    sensitive: false,
                }))
            }
            Payload::Image { png, width, height } => {
                if png.is_empty() {
                    return Ok(PreparedOutcome::Skip(SkipReason::Empty));
                }
                if png.len() > HARD_MAX_BYTES {
                    return Ok(PreparedOutcome::Skip(SkipReason::TooLarge));
                }
                Ok(PreparedOutcome::Ready(Prepared {
                    kind,
                    body: StoredBody::Image { width, height },
                    preview: format!("{width}×{height} 图片"),
                    hash_input: png.clone(),
                    blob_bytes: Some(png),
                    truncated: false,
                    sensitive: false,
                }))
            }
        }
    }

    fn write_blob(&self, hash: &ContentHash, bytes: &[u8]) -> StoreResult<PathBuf> {
        let final_path = self.blob_dir.join(hex(hash));
        if final_path.exists() {
            return Ok(final_path);
        }
        let tmp = self.blob_dir.join(format!("{}.tmp", hex(hash)));
        fs::write(&tmp, bytes)?;
        fs::rename(&tmp, &final_path)?;
        Ok(final_path)
    }

    fn remove_blob(&self, hash: &ContentHash) {
        let _ = fs::remove_file(self.blob_dir.join(hex(hash)));
    }

    fn persist_config(&self, config: &Config) -> StoreResult<()> {
        let encoded = serde_json::to_vec(config).map_err(|e| StoreError::Codec(e.to_string()))?;
        let w = self.db.begin_write()?;
        {
            let mut t = w.open_table(CONFIG)?;
            t.insert(CONFIG_KEY, &encoded[..])?;
        }
        w.commit()?;
        Ok(())
    }

    fn next_timestamp(&self, meta: &mut Table<&str, &[u8]>, candidate: i64) -> StoreResult<i64> {
        let last = meta_get(meta, meta_key::LAST_TS)? as i64;
        let ts = candidate.max(last);
        meta_set(meta, meta_key::LAST_TS, ts as u64)?;
        Ok(ts)
    }

    /// 组装返回给 UI 的视图。图片顺带带上 blob 路径，列表画缩略图时直接用。
    fn view(&self, entry: EntryRecord, seq: Seq, last_used_at: i64) -> EntryView {
        let blob_path = entry.blob_ref.map(|hash| self.blob_dir.join(hex(&hash)));
        EntryView {
            entry,
            seq,
            last_used_at,
            blob_path,
        }
    }
}

impl ClipboardStore for RedbStore {
    fn schema_version(&self) -> u32 {
        SCHEMA_VERSION
    }

    fn capture(&self, item: CapturedItem) -> StoreResult<CaptureOutcome> {
        let cfg = self.config()?;

        if let Some(app) = item.source_app.as_deref() {
            if cfg.excluded_apps.iter().any(|x| x.eq_ignore_ascii_case(app)) {
                return Ok(CaptureOutcome::Skipped(SkipReason::ExcludedApp));
            }
        }

        let mut prepared = match self.prepare(item.payload, &cfg)? {
            PreparedOutcome::Ready(p) => p,
            PreparedOutcome::Skip(reason) => return Ok(CaptureOutcome::Skipped(reason)),
        };

        if prepared.sensitive {
            if cfg.skip_sensitive {
                return Ok(CaptureOutcome::Skipped(SkipReason::Sensitive));
            }
            if cfg.mask_sensitive_preview {
                prepared.preview = "••••••（敏感内容已隐藏）".to_string();
            }
        }

        let hash: ContentHash = *blake3::hash(&prepared.hash_input).as_bytes();

        let w = self.db.begin_write()?;
        let mut evicted_blobs = Vec::new();
        let outcome;
        {
            let mut entries = w.open_table(ENTRIES)?;
            let mut texts = w.open_table(TEXTS)?;
            let mut content_index = w.open_table(CONTENT_INDEX)?;
            let mut usage = w.open_table(USAGE_ORDER)?;
            let mut seq_index = w.open_table(SEQ_INDEX)?;
            let mut meta = w.open_table(META)?;

            let now = self.next_timestamp(&mut meta, item.captured_at)?;
            let existing = content_index.get(&hash[..])?.map(|g| g.value());

            if let Some(id) = existing {
                let seq = seq_index
                    .get(id)?
                    .map(|g| g.value())
                    .ok_or_else(|| StoreError::Codec("seq_index 缺失".into()))?;
                let rec = read_record(&entries, id)?;
                if cfg.replay_reorders {
                    usage.remove(seq)?;
                    seq_index.remove(id)?;
                    let new_seq = meta_get(&meta, meta_key::NEXT_SEQ)?;
                    meta_set(&mut meta, meta_key::NEXT_SEQ, new_seq + 1)?;
                    usage.insert(new_seq, &encode_usage(id, now)[..])?;
                    seq_index.insert(id, new_seq)?;
                    outcome = CaptureOutcome::Moved {
                        entry: self.view(rec, new_seq, now),
                    };
                } else {
                    outcome = CaptureOutcome::Moved {
                        entry: self.view(rec, seq, now),
                    };
                }
            } else {
                let id = meta_get(&meta, meta_key::NEXT_ENTRY_ID)?;
                meta_set(&mut meta, meta_key::NEXT_ENTRY_ID, id + 1)?;
                let seq = meta_get(&meta, meta_key::NEXT_SEQ)?;
                meta_set(&mut meta, meta_key::NEXT_SEQ, seq + 1)?;

                if let Some(png) = prepared.blob_bytes.as_ref() {
                    self.write_blob(&hash, png)?;
                }

                let rec = EntryRecord {
                    id,
                    hash,
                    kind: prepared.kind,
                    preview: prepared.preview,
                    created_at: item.captured_at,
                    source_app: item.source_app,
                    pinned: false,
                    blob_ref: if prepared.kind == Kind::Image { Some(hash) } else { None },
                    truncated: prepared.truncated,
                };
                entries.insert(id, &enc(&rec)?[..])?;
                texts.insert(id, &enc(&prepared.body)?[..])?;
                content_index.insert(&hash[..], id)?;
                usage.insert(seq, &encode_usage(id, now)[..])?;
                seq_index.insert(id, seq)?;

                let count = meta_get(&meta, meta_key::NON_PINNED_COUNT)?;
                meta_set(&mut meta, meta_key::NON_PINNED_COUNT, count + 1)?;

                let evicted = cleanup(
                    &mut entries,
                    &mut texts,
                    &mut content_index,
                    &mut usage,
                    &mut seq_index,
                    &mut meta,
                    &cfg,
                    now,
                    &mut evicted_blobs,
                )?;

                outcome = CaptureOutcome::Inserted {
                    entry: self.view(rec, seq, now),
                    evicted,
                };
            }
        }
        w.commit()?;

        for h in &evicted_blobs {
            self.remove_blob(h);
        }
        Ok(outcome)
    }

    fn page(&self, cursor: Option<Seq>, limit: usize) -> StoreResult<Page> {
        let r = self.db.begin_read()?;
        let usage = r.open_table(USAGE_ORDER)?;
        let entries = r.open_table(ENTRIES)?;
        let seq_index = r.open_table(SEQ_INDEX)?;
        let pinned = r.open_table(PINNED)?;

        let mut items: Vec<EntryView> = Vec::with_capacity(limit);
        let mut last_seq = None;

        // 固定条目置顶。数量通常很少，全取再按 seq 倒序即可。
        // 不这样做的话，固定只是"不被清理"，在时间流里照样会被淹没。
        let mut pinned_ids = Vec::new();
        for item in pinned.range(0u64..)? {
            let (k, _) = item?;
            pinned_ids.push(k.value());
        }
        let mut pinned_views = Vec::with_capacity(pinned_ids.len());
        for id in pinned_ids {
            let Some(seq) = seq_index.get(id)?.map(|g| g.value()) else {
                continue;
            };
            let Some((pid, ts)) = usage
                .get(seq)?
                .and_then(|g| decode_usage(g.value()))
            else {
                continue;
            };
            if let Some(rec) = read_record_opt(&entries, pid)? {
                pinned_views.push(self.view(rec, seq, ts));
            }
        }
        pinned_views.sort_by_key(|e| std::cmp::Reverse(e.seq));
        items.extend(pinned_views.into_iter().take(limit));
        if items.len() >= limit {
            return Ok(Page {
                items,
                next_cursor: None,
                total_visible: entries.len()? as usize,
            });
        }

        let range = match cursor {
            Some(c) => usage.range(..c)?,
            None => usage.range(0u64..)?,
        };
        for item in range.rev() {
            let (k, v) = item?;
            let seq = k.value();
            let (id, last_used_at) = decode_usage(v.value())
                .ok_or_else(|| StoreError::Codec("usage slot 长度错误".into()))?;
            // 已经作为固定条目排到前面了，这里跳过。
            if items.iter().any(|e| e.entry.id == id) {
                continue;
            }
            if let Some(rec) = read_record_opt(&entries, id)? {
                items.push(self.view(rec, seq, last_used_at));
                last_seq = Some(seq);
            }
            if items.len() >= limit {
                break;
            }
        }

        let next_cursor = if items.len() >= limit { last_seq } else { None };
        Ok(Page {
            items,
            next_cursor,
            total_visible: entries.len()? as usize,
        })
    }

    fn search(&self, query: &str, limit: usize) -> StoreResult<Vec<EntryView>> {
        let needle = query.to_lowercase();
        if needle.is_empty() {
            return Ok(Vec::new());
        }
        let r = self.db.begin_read()?;
        let usage = r.open_table(USAGE_ORDER)?;
        let entries = r.open_table(ENTRIES)?;
        let texts = r.open_table(TEXTS)?;

        let mut out: Vec<EntryView> = Vec::new();

        // 第一遍：预览。命中率最高的场景，不碰 texts。
        for item in usage.range(0u64..)?.rev() {
            let (k, v) = item?;
            let (id, last_used_at) = decode_usage(v.value())
                .ok_or_else(|| StoreError::Codec("usage slot 长度错误".into()))?;
            if let Some(rec) = read_record_opt(&entries, id)? {
                if rec.preview.to_lowercase().contains(&needle) {
                    out.push(self.view(rec, k.value(), last_used_at));
                    if out.len() >= limit {
                        return Ok(out);
                    }
                }
            }
        }

        // 第二遍：正文。预览只有前 500 字符，正文里才有的内容靠这一遍捞。
        // 解码整段正文比较贵，所以设了扫描上限——宁可结果不全，也不能让
        // 一次搜索把存储线程卡住几秒。
        let mut scanned = 0usize;
        for item in usage.range(0u64..)?.rev() {
            let (k, v) = item?;
            let (id, last_used_at) = decode_usage(v.value())
                .ok_or_else(|| StoreError::Codec("usage slot 长度错误".into()))?;
            if out.iter().any(|e| e.entry.id == id) {
                continue;
            }
            scanned += 1;
            if scanned > MAX_BODY_SCAN {
                break;
            }

            let Some(g) = texts.get(id)? else { continue };
            // 单条解码失败只跳过这条，不让整次搜索失败。
            let Ok(body) = dec::<StoredBody>(g.value()) else {
                continue;
            };
            if body_matches(&body, &needle) {
                if let Some(rec) = read_record_opt(&entries, id)? {
                    out.push(self.view(rec, k.value(), last_used_at));
                    if out.len() >= limit {
                        break;
                    }
                }
            }
        }

        Ok(out)
    }

    fn get_content(&self, id: EntryId) -> StoreResult<Option<Content>> {
        let r = self.db.begin_read()?;
        let entries = r.open_table(ENTRIES)?;
        let Some(rec) = read_record_opt(&entries, id)? else {
            return Ok(None);
        };
        let texts = r.open_table(TEXTS)?;
        let body = match texts.get(id)? {
            Some(g) => dec::<StoredBody>(g.value())?,
            None => return Ok(None),
        };
        Ok(Some(match body {
            StoredBody::Text(t) => Content::Text(t),
            StoredBody::Rich { plain, html } => Content::RichText { plain, html },
            StoredBody::Files(files) => Content::FileList(files),
            StoredBody::Image { width, height } => Content::Image {
                path: self.blob_dir.join(hex(&rec.hash)),
                width,
                height,
            },
        }))
    }

    fn blob_path(&self, hash: ContentHash) -> Option<PathBuf> {
        Some(self.blob_dir.join(hex(&hash)))
    }

    fn touch(&self, id: EntryId) -> StoreResult<TouchOutcome> {
        let cfg = self.config()?;
        if !cfg.replay_reorders {
            return Ok(TouchOutcome::Unchanged);
        }

        let w = self.db.begin_write()?;
        let out;
        {
            let mut usage = w.open_table(USAGE_ORDER)?;
            let mut seq_index = w.open_table(SEQ_INDEX)?;
            let mut meta = w.open_table(META)?;

            let Some(old) = seq_index.get(id)?.map(|g| g.value()) else {
                return Ok(TouchOutcome::NotFound);
            };
            let now = self.next_timestamp(&mut meta, now_ms())?;
            usage.remove(old)?;
            seq_index.remove(id)?;

            let seq = meta_get(&meta, meta_key::NEXT_SEQ)?;
            meta_set(&mut meta, meta_key::NEXT_SEQ, seq + 1)?;
            usage.insert(seq, &encode_usage(id, now)[..])?;
            seq_index.insert(id, seq)?;
            out = TouchOutcome::Moved { seq, last_used_at: now };
        }
        w.commit()?;
        Ok(out)
    }

    fn set_pinned(&self, id: EntryId, pinned: bool) -> StoreResult<Vec<EntryId>> {
        let cfg = self.config()?;
        let w = self.db.begin_write()?;
        let mut evicted_blobs = Vec::new();
        let evicted;
        {
            let mut entries = w.open_table(ENTRIES)?;
            let mut texts = w.open_table(TEXTS)?;
            let mut content_index = w.open_table(CONTENT_INDEX)?;
            let mut usage = w.open_table(USAGE_ORDER)?;
            let mut seq_index = w.open_table(SEQ_INDEX)?;
            let mut pinned_index = w.open_table(PINNED)?;
            let mut meta = w.open_table(META)?;

            let Some(seq) = seq_index.get(id)?.map(|g| g.value()) else {
                return Ok(Vec::new());
            };
            let mut rec = read_record(&entries, id)?;
            if rec.pinned == pinned {
                return Ok(Vec::new());
            }
            rec.pinned = pinned;
            entries.insert(id, &enc(&rec)?[..])?;
            if pinned {
                pinned_index.insert(id, id)?;
            } else {
                pinned_index.remove(id)?;
            }

            let count = meta_get(&meta, meta_key::NON_PINNED_COUNT)?;
            if pinned {
                meta_set(&mut meta, meta_key::NON_PINNED_COUNT, count.saturating_sub(1))?;
            } else {
                meta_set(&mut meta, meta_key::NON_PINNED_COUNT, count + 1)?;
                // 游标可能已经越过这条记录（固定条目会被跳过），必须回退。
                let cursor = meta_get(&meta, meta_key::CLEANUP_CURSOR)?;
                meta_set(&mut meta, meta_key::CLEANUP_CURSOR, cursor.min(seq))?;
            }

            evicted = cleanup(
                &mut entries,
                &mut texts,
                &mut content_index,
                &mut usage,
                &mut seq_index,
                &mut meta,
                &cfg,
                now_ms(),
                &mut evicted_blobs,
            )?;
        }
        w.commit()?;
        for h in &evicted_blobs {
            self.remove_blob(h);
        }
        Ok(evicted)
    }

    fn delete(&self, id: EntryId) -> StoreResult<bool> {
        let w = self.db.begin_write()?;
        let mut evicted_blobs = Vec::new();
        {
            let mut entries = w.open_table(ENTRIES)?;
            let mut texts = w.open_table(TEXTS)?;
            let mut content_index = w.open_table(CONTENT_INDEX)?;
            let mut usage = w.open_table(USAGE_ORDER)?;
            let mut seq_index = w.open_table(SEQ_INDEX)?;
            let mut pinned_index = w.open_table(PINNED)?;
            let mut meta = w.open_table(META)?;

            let Some(seq) = seq_index.get(id)?.map(|g| g.value()) else {
                return Ok(false);
            };
            pinned_index.remove(id)?;
            let pinned = is_pinned(&entries, id)?;
            remove_entry(
                &mut entries,
                &mut texts,
                &mut content_index,
                &mut usage,
                &mut seq_index,
                id,
                seq,
                &mut evicted_blobs,
            )?;
            if !pinned {
                let count = meta_get(&meta, meta_key::NON_PINNED_COUNT)?;
                meta_set(&mut meta, meta_key::NON_PINNED_COUNT, count.saturating_sub(1))?;
            }
        }
        w.commit()?;
        for h in &evicted_blobs {
            self.remove_blob(h);
        }
        Ok(true)
    }

    fn clear_unpinned(&self) -> StoreResult<Vec<EntryId>> {
        let w = self.db.begin_write()?;
        let mut evicted_blobs = Vec::new();
        let mut removed = Vec::new();
        {
            let mut entries = w.open_table(ENTRIES)?;
            let mut texts = w.open_table(TEXTS)?;
            let mut content_index = w.open_table(CONTENT_INDEX)?;
            let mut usage = w.open_table(USAGE_ORDER)?;
            let mut seq_index = w.open_table(SEQ_INDEX)?;
            let mut meta = w.open_table(META)?;

            let mut victims = Vec::new();
            for item in usage.range(0u64..)? {
                let (k, v) = item?;
                let (id, _) = decode_usage(v.value())
                    .ok_or_else(|| StoreError::Codec("usage slot 长度错误".into()))?;
                if !is_pinned(&entries, id)? {
                    victims.push((k.value(), id));
                }
            }
            for (seq, id) in victims {
                remove_entry(
                    &mut entries,
                    &mut texts,
                    &mut content_index,
                    &mut usage,
                    &mut seq_index,
                    id,
                    seq,
                    &mut evicted_blobs,
                )?;
                removed.push(id);
            }
            meta_set(&mut meta, meta_key::NON_PINNED_COUNT, 0)?;
            meta_set(&mut meta, meta_key::CLEANUP_CURSOR, 0)?;
        }
        w.commit()?;
        for h in &evicted_blobs {
            self.remove_blob(h);
        }
        Ok(removed)
    }

    fn config(&self) -> StoreResult<Config> {
        let r = self.db.begin_read()?;
        let t = r.open_table(CONFIG)?;
        let raw = t.get(CONFIG_KEY)?.map(|g| g.value().to_vec());
        // 读事务必须在写回之前结束，否则会撞上 redb 的写锁。
        drop(t);

        let Some(bytes) = raw else {
            return Ok(Config::default());
        };

        // 配置用 JSON 而不是 postcard：postcard 不自描述，给 Config 加一个字段
        // 就会让旧 blob 整块解不出来。JSON + `#[serde(default)]` 能容忍新增字段。
        match serde_json::from_slice(&bytes) {
            Ok(config) => Ok(config),
            Err(e) => {
                eprintln!("25zai: 配置解码失败，已重置为默认值（{e}）");
                let default = Config::default();
                // 立刻写回，否则每次启动都会重复这条警告。
                self.persist_config(&default)?;
                Ok(default)
            }
        }
    }

    fn update_config(&self, config: Config) -> StoreResult<Vec<EntryId>> {
        let mut config = config;
        // 下限放到 1 是为了让测试能构造小上限；设置页自己限制在 100–5000。
        config.limit = config.limit.clamp(1, 100_000);

        let w = self.db.begin_write()?;
        let mut evicted_blobs = Vec::new();
        let evicted;
        {
            let mut cfg_table = w.open_table(CONFIG)?;
            let encoded =
                serde_json::to_vec(&config).map_err(|e| StoreError::Codec(e.to_string()))?;
            cfg_table.insert(CONFIG_KEY, &encoded[..])?;

            let mut entries = w.open_table(ENTRIES)?;
            let mut texts = w.open_table(TEXTS)?;
            let mut content_index = w.open_table(CONTENT_INDEX)?;
            let mut usage = w.open_table(USAGE_ORDER)?;
            let mut seq_index = w.open_table(SEQ_INDEX)?;
            let mut meta = w.open_table(META)?;

            evicted = cleanup(
                &mut entries,
                &mut texts,
                &mut content_index,
                &mut usage,
                &mut seq_index,
                &mut meta,
                &config,
                now_ms(),
                &mut evicted_blobs,
            )?;
        }
        w.commit()?;
        for h in &evicted_blobs {
            self.remove_blob(h);
        }
        Ok(evicted)
    }

    fn stats(&self) -> StoreResult<Stats> {
        let r = self.db.begin_read()?;
        let entries = r.open_table(ENTRIES)?;
        let total = entries.len()? as usize;
        let mut pinned = 0usize;
        for item in entries.range(0u64..)? {
            let (_, v) = item?;
            if dec::<EntryRecord>(v.value())?.pinned {
                pinned += 1;
            }
        }
        let db_bytes = fs::metadata(&self.db_path).map(|m| m.len()).unwrap_or(0);
        let mut blob_bytes = 0u64;
        if let Ok(dir) = fs::read_dir(&self.blob_dir) {
            for e in dir.flatten() {
                if let Ok(m) = e.metadata() {
                    blob_bytes += m.len();
                }
            }
        }
        Ok(Stats {
            total_entries: total,
            pinned_entries: pinned,
            db_bytes,
            blob_bytes,
        })
    }

    fn sweep_orphan_blobs(&self) -> StoreResult<usize> {
        let r = self.db.begin_read()?;
        let ci = r.open_table(CONTENT_INDEX)?;
        let mut removed = 0usize;
        for entry in fs::read_dir(&self.blob_dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".tmp") {
                let _ = fs::remove_file(entry.path());
                continue;
            }
            let Some(hash) = unhex(&name) else { continue };
            if ci.get(&hash[..])?.is_none() {
                fs::remove_file(entry.path())?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

// ---------------------------------------------------------------------------
// 事务内辅助函数
// ---------------------------------------------------------------------------

// 读写事务返回的表类型不同（Table / ReadOnlyTable），统一走 ReadableTable 约束。
fn read_record_opt<T: ReadableTable<u64, &'static [u8]>>(
    t: &T,
    id: EntryId,
) -> StoreResult<Option<EntryRecord>> {
    match t.get(id)? {
        Some(g) => Ok(Some(dec(g.value())?)),
        None => Ok(None),
    }
}

fn read_record<T: ReadableTable<u64, &'static [u8]>>(
    t: &T,
    id: EntryId,
) -> StoreResult<EntryRecord> {
    read_record_opt(t, id)?.ok_or_else(|| StoreError::Codec(format!("entry {id} 不存在")))
}

fn is_pinned<T: ReadableTable<u64, &'static [u8]>>(t: &T, id: EntryId) -> StoreResult<bool> {
    Ok(read_record_opt(t, id)?.map(|r| r.pinned).unwrap_or(false))
}

#[allow(clippy::too_many_arguments)]
fn remove_entry(
    entries: &mut Table<u64, &[u8]>,
    texts: &mut Table<u64, &[u8]>,
    content_index: &mut Table<&[u8], u64>,
    usage: &mut Table<u64, &[u8]>,
    seq_index: &mut Table<u64, u64>,
    id: EntryId,
    seq: Seq,
    evicted_blobs: &mut Vec<ContentHash>,
) -> StoreResult<()> {
    usage.remove(seq)?;
    seq_index.remove(id)?;
    texts.remove(id)?;
    let removed = entries.remove(id)?;
    if let Some(g) = removed {
        let rec: EntryRecord = dec(g.value())?;
        content_index.remove(&rec.hash[..])?;
        if let Some(b) = rec.blob_ref {
            evicted_blobs.push(b);
        }
    }
    Ok(())
}

/// 清理：条数上限与时间上限同时生效，从游标处找最老的未固定条目。
///
/// `non_pinned_count` 让"是否超限"是 O(1)；`cleanup_cursor` 让扫描摊还 O(1)。
/// 两者缺一，每次插入都会退化成 O(n)。
#[allow(clippy::too_many_arguments)]
fn cleanup(
    entries: &mut Table<u64, &[u8]>,
    texts: &mut Table<u64, &[u8]>,
    content_index: &mut Table<&[u8], u64>,
    usage: &mut Table<u64, &[u8]>,
    seq_index: &mut Table<u64, u64>,
    meta: &mut Table<&str, &[u8]>,
    cfg: &Config,
    now: i64,
    evicted_blobs: &mut Vec<ContentHash>,
) -> StoreResult<Vec<EntryId>> {
    let mut removed = Vec::new();
    let mut count = meta_get(meta, meta_key::NON_PINNED_COUNT)? as usize;
    let mut cursor = meta_get(meta, meta_key::CLEANUP_CURSOR)?;
    let retention = cfg.retention_days;

    loop {
        let over_count = count > cfg.limit;
        if !over_count && retention.is_none() {
            break;
        }

        let mut victim: Option<(Seq, EntryId, i64)> = None;
        let mut last_scanned: Option<Seq> = None;
        {
            for item in usage.range(cursor..)? {
                let (k, v) = item?;
                let seq = k.value();
                let (id, ts) = decode_usage(v.value())
                    .ok_or_else(|| StoreError::Codec("usage slot 长度错误".into()))?;
                last_scanned = Some(seq);
                if is_pinned(entries, id)? {
                    continue;
                }
                victim = Some((seq, id, ts));
                break;
            }
        }

        let Some((seq, id, ts)) = victim else {
            // 剩下的全是固定条目，游标推到末尾，避免下次重扫
            if let Some(last) = last_scanned {
                cursor = cursor.max(last + 1);
            }
            break;
        };

        if !over_count {
            match retention {
                Some(days) if expired(ts, now, days) => {}
                _ => {
                    // 最老的未固定条目还没过期，游标停在它之前
                    cursor = seq;
                    break;
                }
            }
        }

        remove_entry(
            entries,
            texts,
            content_index,
            usage,
            seq_index,
            id,
            seq,
            evicted_blobs,
        )?;
        removed.push(id);
        count = count.saturating_sub(1);
        cursor = seq + 1;
    }

    meta_set(meta, meta_key::NON_PINNED_COUNT, count as u64)?;
    meta_set(meta, meta_key::CLEANUP_CURSOR, cursor)?;
    Ok(removed)
}

fn meta_get(meta: &Table<&str, &[u8]>, key: &str) -> StoreResult<u64> {
    match meta.get(key)? {
        Some(g) => decode_u64(g.value())
            .ok_or_else(|| StoreError::Codec(format!("meta {key} 长度错误"))),
        None => Ok(0),
    }
}

fn meta_set(meta: &mut Table<&str, &[u8]>, key: &str, v: u64) -> StoreResult<()> {
    meta.insert(key, &encode_u64(v)[..])?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 编码与格式化
// ---------------------------------------------------------------------------

fn enc<T: Serialize>(v: &T) -> StoreResult<Vec<u8>> {
    postcard::to_allocvec(v).map_err(|e| StoreError::Codec(e.to_string()))
}

fn dec<'a, T: Deserialize<'a>>(b: &'a [u8]) -> StoreResult<T> {
    postcard::from_bytes(b).map_err(|e| StoreError::Codec(e.to_string()))
}

/// 正文匹配。只匹配"人能看见的字"，不碰图片 blob。
fn body_matches(body: &StoredBody, needle: &str) -> bool {
    match body {
        StoredBody::Text(text) => text.to_lowercase().contains(needle),
        StoredBody::Rich { plain, html } => {
            plain.to_lowercase().contains(needle)
                || html
                    .as_deref()
                    .is_some_and(|h| h.to_lowercase().contains(needle))
        }
        StoredBody::Files(paths) => paths
            .iter()
            .any(|p| p.to_string_lossy().to_lowercase().contains(needle)),
        StoredBody::Image { .. } => false,
    }
}

fn expired(ts: i64, now: i64, days: u32) -> bool {
    now.saturating_sub(ts) > (days as i64) * 86_400_000
}

fn truncate_bytes(s: &str, max: usize) -> (&str, bool) {
    if s.len() <= max {
        return (s, false);
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], true)
}

fn preview_of(s: &str) -> String {
    let mut out: String = s.chars().take(PREVIEW_CHARS).collect();
    if s.chars().nth(PREVIEW_CHARS).is_some() {
        out.push('…');
    }
    out
}

fn files_preview(paths: &[PathBuf]) -> String {
    let names: Vec<String> = paths
        .iter()
        .take(3)
        .map(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string())
        })
        .collect();
    if paths.len() > 3 {
        format!("{} 等 {} 个文件", names.join(", "), paths.len())
    } else {
        names.join(", ")
    }
}

fn hex(hash: &ContentHash) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(64);
    for &b in hash.iter() {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 0x0f) as usize] as char);
    }
    s
}

fn unhex(s: &str) -> Option<ContentHash> {
    if s.len() != 64 {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = [0u8; 32];
    for i in 0..32 {
        let hi = (bytes[i * 2] as char).to_digit(16)?;
        let lo = (bytes[i * 2 + 1] as char).to_digit(16)?;
        out[i] = (hi * 16 + lo) as u8;
    }
    Some(out)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
