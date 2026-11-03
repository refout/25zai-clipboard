//! 排序（v0.2.1）：创建时间、按类型、手动顺序。

use std::path::PathBuf;

use erwuzai_store::*;

fn tmp(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("25zai-sort-{tag}-{nanos}"));
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

fn files(name: &str, ts: i64) -> CapturedItem {
    CapturedItem {
        payload: Payload::FileList(vec![PathBuf::from(format!("C:/{name}"))]),
        source_app: Some("tester".into()),
        captured_at: ts,
    }
}

fn previews(page: &Page) -> Vec<&str> {
    page.items.iter().map(|e| e.entry.preview.as_str()).collect()
}

#[test]
fn created_time_orders_both_ways() {
    let dir = tmp("created");
    let store = RedbStore::open_default(dir.join("25zai.db"), dir.join("blobs")).unwrap();

    store.capture(text("old", 1_000)).unwrap();
    store.capture(text("mid", 2_000)).unwrap();
    store.capture(text("new", 3_000)).unwrap();

    let desc = store
        .page(Scope::History, SortMode::CreatedDesc, None, 10)
        .unwrap();
    assert_eq!(previews(&desc), vec!["new", "mid", "old"]);

    let asc = store
        .page(Scope::History, SortMode::CreatedAsc, None, 10)
        .unwrap();
    assert_eq!(previews(&asc), vec!["old", "mid", "new"]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn kind_groups_text_before_files() {
    let dir = tmp("kind");
    let store = RedbStore::open_default(dir.join("25zai.db"), dir.join("blobs")).unwrap();

    store.capture(files("a.txt", 1_000)).unwrap();
    store.capture(text("最早", 2_000)).unwrap();
    store.capture(files("b.txt", 3_000)).unwrap();
    store.capture(text("最新", 4_000)).unwrap();

    let page = store
        .page(Scope::History, SortMode::Kind, None, 10)
        .unwrap();
    let kinds: Vec<Kind> = page.items.iter().map(|e| e.entry.kind).collect();
    assert_eq!(
        kinds,
        vec![Kind::Text, Kind::Text, Kind::FileList, Kind::FileList]
    );
    // 组内按最近使用倒序。
    assert_eq!(previews(&page)[0], "最新");

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn manual_order_round_trips_and_new_items_go_last() {
    let dir = tmp("manual");
    let store = RedbStore::open_default(dir.join("25zai.db"), dir.join("blobs")).unwrap();

    let a = store.capture(text("a", 1_000)).unwrap();
    let b = store.capture(text("b", 2_000)).unwrap();
    let c = store.capture(text("c", 3_000)).unwrap();
    let (a, b, c) = match (a, b, c) {
        (
            CaptureOutcome::Inserted { entry: a, .. },
            CaptureOutcome::Inserted { entry: b, .. },
            CaptureOutcome::Inserted { entry: c, .. },
        ) => (a.entry.id, b.entry.id, c.entry.id),
        _ => panic!("预期都是新插入"),
    };

    store.set_manual_order(&[c, a, b]).unwrap();
    let page = store
        .page(Scope::History, SortMode::Manual, None, 10)
        .unwrap();
    assert_eq!(previews(&page), vec!["c", "a", "b"]);

    // 新条目没有序号，排在已排好的后面。
    store.capture(text("d", 4_000)).unwrap();
    let page = store
        .page(Scope::History, SortMode::Manual, None, 10)
        .unwrap();
    assert_eq!(previews(&page), vec!["c", "a", "b", "d"]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn manual_mode_does_not_hoist_pinned() {
    let dir = tmp("manual-pin");
    let store = RedbStore::open_default(dir.join("25zai.db"), dir.join("blobs")).unwrap();

    let a = store.capture(text("a", 1_000)).unwrap();
    let b = store.capture(text("b", 2_000)).unwrap();
    let (a, b) = match (a, b) {
        (
            CaptureOutcome::Inserted { entry: a, .. },
            CaptureOutcome::Inserted { entry: b, .. },
        ) => (a.entry.id, b.entry.id),
        _ => panic!("预期都是新插入"),
    };
    store.set_pinned(a, true).unwrap();
    store.set_manual_order(&[a, b]).unwrap();

    // 手动模式下 a 虽然固定，但仍按序号排在前面——这里 a 本来就在前，
    // 所以再把顺序反过来验证固定不会把它顶上去。
    store.set_manual_order(&[b, a]).unwrap();
    let page = store
        .page(Scope::History, SortMode::Manual, None, 10)
        .unwrap();
    assert_eq!(previews(&page), vec!["b", "a"]);

    // 其它模式下固定条目仍然置顶。
    let recent = store
        .page(Scope::History, SortMode::Recent, None, 10)
        .unwrap();
    assert_eq!(previews(&recent)[0], "a");

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn manual_order_is_per_scope_by_construction() {
    let dir = tmp("manual-scope");
    let store = RedbStore::open_default(dir.join("25zai.db"), dir.join("blobs")).unwrap();

    store.capture(text("clip", 1_000)).unwrap();
    store.insert_note("note").unwrap();

    // 笔记永远排在历史的后面（按 id 顺序写序号），但两个视图各自只看到自己的条目。
    let note = store.page(Scope::Notes, SortMode::Manual, None, 10).unwrap();
    let clip = store
        .page(Scope::History, SortMode::Manual, None, 10)
        .unwrap();
    assert_eq!(previews(&note), vec!["note"]);
    assert_eq!(previews(&clip), vec!["clip"]);

    let _ = std::fs::remove_dir_all(dir);
}
