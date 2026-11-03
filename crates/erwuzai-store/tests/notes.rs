//! 笔记（v0.2.0）的存储层行为。

use std::path::PathBuf;

use erwuzai_store::*;

fn tmp(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("25zai-notes-{tag}-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn text(s: &str, ts: i64) -> CapturedItem {
    CapturedItem {
        payload: Payload::Text(s.to_string()),
        source_app: Some("tester".into()),
        captured_at: ts,
    }
}

fn previews(page: &Page) -> Vec<&str> {
    page.items.iter().map(|e| e.entry.preview.as_str()).collect()
}

#[test]
fn notes_do_not_show_up_in_history() {
    let dir = tmp("scope");
    let store = RedbStore::open_default(dir.join("25zai.db"), dir.join("blobs")).unwrap();

    store.capture(text("clip", 1_000)).unwrap();
    store.insert_note("想法").unwrap();

    let history = store.page(Scope::History, SortMode::Recent, None, 10).unwrap();
    assert_eq!(previews(&history), vec!["clip"]);
    assert_eq!(history.total_visible, 1);

    let notes = store.page(Scope::Notes, SortMode::Recent, None, 10).unwrap();
    assert_eq!(previews(&notes), vec!["想法"]);
    assert_eq!(notes.total_visible, 1);
    assert!(notes.items[0].entry.kind.is_note());
    assert!(notes.items[0].entry.pinned);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn same_note_twice_is_two_entries() {
    let dir = tmp("dup");
    let store = RedbStore::open_default(dir.join("25zai.db"), dir.join("blobs")).unwrap();

    let a = store.insert_note("同一句话").unwrap().unwrap();
    let b = store.insert_note("同一句话").unwrap().unwrap();
    assert_ne!(a.entry.id, b.entry.id);
    assert_eq!(store.page(Scope::Notes, SortMode::Recent, None, 10).unwrap().total_visible, 2);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn notes_are_not_evicted_by_limit() {
    let dir = tmp("limit");
    let store = RedbStore::open_default(dir.join("25zai.db"), dir.join("blobs")).unwrap();

    let mut cfg = store.config().unwrap();
    cfg.limit = 1;
    store.update_config(cfg).unwrap();

    store.insert_note("留着").unwrap();
    for i in 0..5 {
        store.capture(text(&format!("clip{i}"), 1_000 + i)).unwrap();
    }

    let notes = store.page(Scope::Notes, SortMode::Recent, None, 10).unwrap();
    assert_eq!(previews(&notes), vec!["留着"]);
    // 笔记不占 limit 配额，历史仍然只留 1 条。
    assert_eq!(store.page(Scope::History, SortMode::Recent, None, 10).unwrap().total_visible, 1);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn notes_are_searchable_in_their_own_scope() {
    let dir = tmp("search");
    let store = RedbStore::open_default(dir.join("25zai.db"), dir.join("blobs")).unwrap();

    store.capture(text("普通剪贴板内容", 1_000)).unwrap();
    store.insert_note("明天买牛奶").unwrap();

    let hits = store.search(Scope::Notes, "牛奶", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].entry.kind.is_note());

    // 历史视图里搜不到笔记，反之亦然。
    assert!(store.search(Scope::History, "牛奶", 10).unwrap().is_empty());
    assert!(store
        .search(Scope::Notes, "剪贴板", 10)
        .unwrap()
        .is_empty());

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn update_note_keeps_position_and_changes_text() {
    let dir = tmp("update");
    let store = RedbStore::open_default(dir.join("25zai.db"), dir.join("blobs")).unwrap();

    let first = store.insert_note("第一条").unwrap().unwrap();
    store.insert_note("第二条").unwrap();

    let updated = store.update_note(first.entry.id, "改过了").unwrap().unwrap();
    assert_eq!(updated.seq, first.seq);
    assert_eq!(updated.entry.preview, "改过了");

    // 位置不变：仍是新的在上，改过的那条还在后面。
    let page = store.page(Scope::Notes, SortMode::Recent, None, 10).unwrap();
    assert_eq!(previews(&page), vec!["第二条", "改过了"]);

    // 正文也换了，旧词搜不到、新词搜得到。
    assert!(store.search(Scope::Notes, "第一条", 10).unwrap().is_empty());
    assert_eq!(store.search(Scope::Notes, "改过", 10).unwrap().len(), 1);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn update_rejects_non_note_entries() {
    let dir = tmp("update-clip");
    let store = RedbStore::open_default(dir.join("25zai.db"), dir.join("blobs")).unwrap();

    let clipped = match store.capture(text("clip", 1_000)).unwrap() {
        CaptureOutcome::Inserted { entry, .. } => entry,
        other => panic!("预期插入，得到 {other:?}"),
    };
    assert!(store.update_note(clipped.entry.id, "改").unwrap().is_none());

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn delete_note_updates_total() {
    let dir = tmp("delete");
    let store = RedbStore::open_default(dir.join("25zai.db"), dir.join("blobs")).unwrap();

    let note = store.insert_note("删掉我").unwrap().unwrap();
    assert!(store.delete(note.entry.id).unwrap());
    assert_eq!(store.page(Scope::Notes, SortMode::Recent, None, 10).unwrap().total_visible, 0);
    // 历史视图的计数不能被笔记的增删带偏。
    assert_eq!(store.page(Scope::History, SortMode::Recent, None, 10).unwrap().total_visible, 0);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn empty_note_is_ignored() {
    let dir = tmp("empty");
    let store = RedbStore::open_default(dir.join("25zai.db"), dir.join("blobs")).unwrap();

    assert!(store.insert_note("   ").unwrap().is_none());
    assert_eq!(store.page(Scope::Notes, SortMode::Recent, None, 10).unwrap().total_visible, 0);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn note_hotkey_has_default_and_old_config_still_loads() {
    let dir = tmp("config");
    let store = RedbStore::open_default(dir.join("25zai.db"), dir.join("blobs")).unwrap();
    assert_eq!(store.config().unwrap().note_hotkey, "Alt+N");

    // 老配置（没有 note_hotkey 字段）读进来要退回默认值，不能整块反序列化失败。
    let legacy = serde_json::json!({
        "limit": 500,
        "retention_days": null,
        "skip_sensitive": true,
        "mask_sensitive_preview": true,
        "excluded_apps": [],
        "replay_reorders": true,
        "replay_as_plain": false,
        "paste_on_click": true,
        "max_text_bytes": 1048576,
        "hotkey": "Alt+V",
        "autostart": true,
        "tray_icon": null,
        "theme": "system",
        "accent": [58, 110, 165],
        "font_scale": 1.0,
        "popup_position": "cursor",
        "language": "auto"
    });
    let cfg: Config = serde_json::from_value(legacy).unwrap();
    assert_eq!(cfg.note_hotkey, "Alt+N");

    let _ = std::fs::remove_dir_all(dir);
}
