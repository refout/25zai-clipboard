//! 图片路径：blob 落盘、去重、回读、删除、孤儿回收。
//!
//! 存储层不解码像素，所以这里的"图片"用任意字节即可——要验证的是
//! blob 生命周期，不是 PNG 编解码。

use std::path::PathBuf;

use erwuzai_store::*;

fn tmp(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("25zai-img-{tag}-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn image_item(bytes: Vec<u8>, ts: i64) -> CapturedItem {
    CapturedItem {
        payload: Payload::Image {
            png: bytes,
            width: 8,
            height: 8,
        },
        source_app: Some("tester".into()),
        captured_at: ts,
    }
}

#[test]
fn image_blob_lifecycle() {
    let dir = tmp("lifecycle");
    let blobs = dir.join("blobs");
    let store = RedbStore::open_default(dir.join("25zai.db"), &blobs).unwrap();

    let bytes = vec![0x89, b'P', b'N', b'G', 1, 2, 3, 4];
    let entry = match store.capture(image_item(bytes.clone(), 1)).unwrap() {
        CaptureOutcome::Inserted { entry, .. } => entry,
        other => panic!("expected Inserted, got {other:?}"),
    };

    assert_eq!(entry.entry.kind, Kind::Image);
    assert_eq!(
        entry.entry.blob_ref,
        Some(entry.entry.hash),
        "图片的 blob_ref 必须等于内容哈希"
    );

    let path = store.blob_path(entry.entry.hash).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), bytes, "blob 必须已落盘");

    match store.get_content(entry.entry.id).unwrap().unwrap() {
        Content::Image {
            path: p,
            width,
            height,
        } => {
            assert_eq!(std::fs::read(p).unwrap(), bytes);
            assert_eq!((width, height), (8, 8), "尺寸存在 texts 的 body 里");
        }
        other => panic!("expected Image content, got {other:?}"),
    }

    // 同样的字节再采集一次：不新增条目，只换 seq
    assert!(matches!(
        store.capture(image_item(bytes.clone(), 2)).unwrap(),
        CaptureOutcome::Moved { .. }
    ));
    assert_eq!(store.page(None, 10).unwrap().total_visible, 1);

    // 删除后 blob 文件必须消失
    assert!(store.delete(entry.entry.id).unwrap());
    assert!(!path.exists(), "删除条目要一并删掉 blob");

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn orphan_and_temp_blobs_are_swept() {
    let dir = tmp("orphan");
    let blobs = dir.join("blobs");
    std::fs::create_dir_all(&blobs).unwrap();
    let store = RedbStore::open_default(dir.join("25zai.db"), &blobs).unwrap();

    let live = match store.capture(image_item(vec![7, 7, 7], 1)).unwrap() {
        CaptureOutcome::Inserted { entry, .. } => entry.entry.hash,
        _ => unreachable!(),
    };
    let live_path = store.blob_path(live).unwrap();

    // 一个没被任何条目引用的合法名字
    let orphan = blobs.join("00".repeat(32));
    std::fs::write(&orphan, b"orphan").unwrap();
    // 上次写 blob 崩在中途留下的临时文件
    let temp = blobs.join("deadbeef.tmp");
    std::fs::write(&temp, b"partial").unwrap();

    let removed = store.sweep_orphan_blobs().unwrap();

    assert_eq!(removed, 1, "只算真正被删掉的孤儿");
    assert!(!orphan.exists());
    assert!(!temp.exists(), ".tmp 残留也要清掉");
    assert!(live_path.exists(), "被引用的 blob 不能动");

    let _ = std::fs::remove_dir_all(dir);
}
