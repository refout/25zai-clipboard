//! 存储层的行为测试。没有依赖临时目录 crate，用时间戳目录 + 手工清理。

use std::path::PathBuf;

use erwuzai_store::*;

fn tmp(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("25zai-test-{tag}-{nanos}"));
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

fn with_limit(store: &RedbStore, limit: usize) {
    let mut cfg = store.config().unwrap();
    cfg.limit = limit;
    store.update_config(cfg).unwrap();
}

#[test]
fn capture_orders_newest_first() {
    let dir = tmp("order");
    let store = RedbStore::open_default(dir.join("25zai.db"), dir.join("blobs")).unwrap();

    store.capture(text("one", 1_000)).unwrap();
    store.capture(text("two", 2_000)).unwrap();
    store.capture(text("three", 3_000)).unwrap();

    let page = store.page(None, 10).unwrap();
    assert_eq!(previews(&page), vec!["three", "two", "one"]);
    assert_eq!(page.total_visible, 3);
    assert!(page.next_cursor.is_none());

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn duplicate_moves_without_touching_content() {
    let dir = tmp("dedup");
    let store = RedbStore::open_default(dir.join("25zai.db"), dir.join("blobs")).unwrap();

    store.capture(text("alpha", 1_000)).unwrap();
    store.capture(text("beta", 2_000)).unwrap();

    match store.capture(text("alpha", 5_000)).unwrap() {
        CaptureOutcome::Moved { entry } => {
            assert_eq!(entry.entry.preview, "alpha");
            assert_eq!(entry.last_used_at, 5_000);
            // created_at 记录的是首次复制时间，不随回放变化
            assert_eq!(entry.entry.created_at, 1_000);
        }
        other => panic!("expected Moved, got {other:?}"),
    }

    let page = store.page(None, 10).unwrap();
    assert_eq!(page.total_visible, 2, "去重不应产生新条目");
    assert_eq!(previews(&page), vec!["alpha", "beta"]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn touch_moves_entry_to_front() {
    let dir = tmp("touch");
    let store = RedbStore::open_default(dir.join("25zai.db"), dir.join("blobs")).unwrap();

    let id = match store.capture(text("a", 1_000)).unwrap() {
        CaptureOutcome::Inserted { entry, .. } => entry.entry.id,
        _ => unreachable!(),
    };
    store.capture(text("b", 2_000)).unwrap();
    store.capture(text("c", 3_000)).unwrap();

    match store.touch(id).unwrap() {
        TouchOutcome::Moved { .. } => {}
        other => panic!("expected Moved, got {other:?}"),
    }

    let page = store.page(None, 10).unwrap();
    assert_eq!(previews(&page), vec!["a", "c", "b"]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn cleanup_evicts_oldest_unpinned() {
    let dir = tmp("cleanup");
    let store = RedbStore::open_default(dir.join("25zai.db"), dir.join("blobs")).unwrap();
    with_limit(&store, 3);

    for i in 0..6 {
        store.capture(text(&format!("item{i}"), 10 + i)).unwrap();
    }

    let page = store.page(None, 100).unwrap();
    assert_eq!(page.total_visible, 3);
    assert_eq!(previews(&page), vec!["item5", "item4", "item3"]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn pinned_entries_are_exempt_and_do_not_count() {
    let dir = tmp("pinned");
    let store = RedbStore::open_default(dir.join("25zai.db"), dir.join("blobs")).unwrap();
    with_limit(&store, 3);

    let pinned_id = match store.capture(text("keepme", 1)).unwrap() {
        CaptureOutcome::Inserted { entry, .. } => entry.entry.id,
        _ => unreachable!(),
    };
    store.set_pinned(pinned_id, true).unwrap();

    for i in 0..6 {
        store.capture(text(&format!("item{i}"), 10 + i)).unwrap();
    }

    let page = store.page(None, 100).unwrap();
    assert_eq!(page.total_visible, 4, "3 条非固定 + 1 条固定");
    assert!(page.items.iter().any(|e| e.entry.id == pinned_id));
    assert!(!previews(&page).contains(&"item0"));
    assert!(!previews(&page).contains(&"item2"));

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn unpin_rewinds_cleanup_cursor() {
    let dir = tmp("unpin");
    let store = RedbStore::open_default(dir.join("25zai.db"), dir.join("blobs")).unwrap();
    with_limit(&store, 2);

    let pinned_id = match store.capture(text("oldest", 1)).unwrap() {
        CaptureOutcome::Inserted { entry, .. } => entry.entry.id,
        _ => unreachable!(),
    };
    store.set_pinned(pinned_id, true).unwrap();
    store.capture(text("b", 2)).unwrap();
    store.capture(text("c", 3)).unwrap();
    store.capture(text("d", 4)).unwrap();

    // 取消固定后它是最老的未固定条目，必须立刻被清理掉。
    let evicted = store.set_pinned(pinned_id, false).unwrap();
    assert_eq!(evicted, vec![pinned_id]);

    let page = store.page(None, 10).unwrap();
    assert!(!previews(&page).contains(&"oldest"));
    assert_eq!(page.total_visible, 2);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn delete_and_clear_unpinned() {
    let dir = tmp("delete");
    let store = RedbStore::open_default(dir.join("25zai.db"), dir.join("blobs")).unwrap();

    let keep = match store.capture(text("keep", 1)).unwrap() {
        CaptureOutcome::Inserted { entry, .. } => entry.entry.id,
        _ => unreachable!(),
    };
    store.set_pinned(keep, true).unwrap();
    store.capture(text("drop1", 2)).unwrap();
    store.capture(text("drop2", 3)).unwrap();

    assert!(store.delete(keep).unwrap());
    assert!(!store.delete(keep).unwrap());
    assert_eq!(store.page(None, 10).unwrap().total_visible, 2);

    let removed = store.clear_unpinned().unwrap();
    assert_eq!(removed.len(), 2);
    assert_eq!(store.page(None, 10).unwrap().total_visible, 0);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn search_matches_preview_case_insensitively() {
    let dir = tmp("search");
    let store = RedbStore::open_default(dir.join("25zai.db"), dir.join("blobs")).unwrap();

    store.capture(text("Hello World", 1)).unwrap();
    store.capture(text("goodbye", 2)).unwrap();

    let hits = store.search("hello", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].entry.preview, "Hello World");

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn pinned_entries_sort_first() {
    let dir = tmp("pinnedfirst");
    let store = RedbStore::open_default(dir.join("25zai.db"), dir.join("blobs")).unwrap();

    let oldest = match store.capture(text("old", 1)).unwrap() {
        CaptureOutcome::Inserted { entry, .. } => entry.entry.id,
        _ => unreachable!(),
    };
    store.capture(text("mid", 2)).unwrap();
    store.capture(text("new", 3)).unwrap();

    // 固定最旧的一条：它应该跳到最前面，而不是留在时间流末尾
    store.set_pinned(oldest, true).unwrap();

    let page = store.page(None, 10).unwrap();
    assert_eq!(previews(&page), vec!["old", "new", "mid"]);
    assert_eq!(page.total_visible, 3, "置顶不能重复计数");

    // 取消固定后回到时间流原位
    store.set_pinned(oldest, false).unwrap();
    let page = store.page(None, 10).unwrap();
    assert_eq!(previews(&page), vec!["new", "mid", "old"]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn search_finds_text_beyond_preview() {
    let dir = tmp("bodysearch");
    let store = RedbStore::open_default(dir.join("25zai.db"), dir.join("blobs")).unwrap();

    // 关键词落在第 600 个字符，超出 500 字符的预览窗口
    let body = format!("{}独一无二的尾巴", "填充内容".repeat(150));
    store.capture(text(&body, 1)).unwrap();

    let page = store.page(None, 10).unwrap();
    assert!(
        !page.items[0]
            .entry
            .preview
            .to_lowercase()
            .contains("独一无二的尾巴"),
        "前提：预览里不应出现这个词"
    );

    let hits = store.search("独一无二的尾巴", 10).unwrap();
    assert_eq!(hits.len(), 1, "正文命中必须能搜到");

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn preview_hits_rank_before_body_hits() {
    let dir = tmp("ranksearch");
    let store = RedbStore::open_default(dir.join("25zai.db"), dir.join("blobs")).unwrap();

    // 较早的条目命中的是预览
    store.capture(text("PREVIEWMARK 旧条目", 1)).unwrap();
    // 较新的条目只有正文里有关键词
    store
        .capture(text(&format!("{}BODYONLYMARK", "x".repeat(600)), 2))
        .unwrap();

    let hits = store.search("mark", 10).unwrap();
    assert_eq!(hits.len(), 2);
    assert!(
        hits[0].entry.preview.to_lowercase().contains("previewmark"),
        "预览命中应排在正文命中之前，即使它更旧"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn sensitive_content_is_skipped_by_default() {
    let dir = tmp("secret");
    let store = RedbStore::open_default(dir.join("25zai.db"), dir.join("blobs")).unwrap();

    let out = store
        .capture(text("sk-abcdefghijklmnopqrstuvwxyz012345", 1))
        .unwrap();
    assert!(matches!(
        out,
        CaptureOutcome::Skipped(SkipReason::Sensitive)
    ));
    assert_eq!(store.page(None, 10).unwrap().total_visible, 0);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn excluded_app_is_dropped() {
    let dir = tmp("excluded");
    let store = RedbStore::open_default(dir.join("25zai.db"), dir.join("blobs")).unwrap();

    let mut cfg = store.config().unwrap();
    cfg.excluded_apps.push("KeePass.exe".into());
    store.update_config(cfg).unwrap();

    let mut item = text("hunter2", 1);
    item.source_app = Some("KeePass.exe".into());
    assert!(matches!(
        store.capture(item).unwrap(),
        CaptureOutcome::Skipped(SkipReason::ExcludedApp)
    ));

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn survives_reopen() {
    let dir = tmp("persist");
    let db = dir.join("25zai.db");
    let blobs = dir.join("blobs");

    {
        let store = RedbStore::open_default(&db, &blobs).unwrap();
        store.capture(text("persisted", 1)).unwrap();
    }
    {
        let store = RedbStore::open_default(&db, &blobs).unwrap();
        let page = store.page(None, 10).unwrap();
        assert_eq!(previews(&page), vec!["persisted"]);
    }

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn paging_with_cursor_does_not_drift() {
    let dir = tmp("paging");
    let store = RedbStore::open_default(dir.join("25zai.db"), dir.join("blobs")).unwrap();

    for i in 0..25 {
        store.capture(text(&format!("e{i:02}"), i as i64)).unwrap();
    }

    let first = store.page(None, 10).unwrap();
    assert_eq!(first.items.len(), 10);
    assert_eq!(first.items[0].entry.preview, "e24");
    let cursor = first.next_cursor.unwrap();

    // 翻页途中插入新条目，不应影响下一页内容
    store.capture(text("late", 100)).unwrap();

    let second = store.page(Some(cursor), 10).unwrap();
    assert_eq!(second.items.len(), 10);
    assert_eq!(second.items[0].entry.preview, "e14");

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn service_thread_reports_events() {
    let dir = tmp("service");
    let store = RedbStore::open_default(dir.join("25zai.db"), dir.join("blobs")).unwrap();
    let svc = StoreService::spawn(Box::new(store)).unwrap();

    let (cmd, rx) = StoreCommand::capture(text("via service", 1));
    assert!(svc.handle.submit(cmd));
    assert!(rx.recv().unwrap().is_ok());

    match svc.events.recv().unwrap() {
        StoreEvent::Added(items) => assert_eq!(items[0].entry.preview, "via service"),
        other => panic!("unexpected event: {other:?}"),
    }

    let (cmd, rx) = StoreCommand::page(None, 10);
    assert!(svc.handle.submit(cmd));
    assert_eq!(rx.recv().unwrap().unwrap().items.len(), 1);

    drop(svc.handle);
    let _ = std::fs::remove_dir_all(dir);
}
