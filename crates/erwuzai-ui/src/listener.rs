//! 监听线程 → 存储层的桥。
//!
//! 职责边界很清楚：平台层只负责把原始内容搬过来，这里做回声判定、
//! 转换成存储层的 `CapturedItem`，然后投递命令。**不阻塞监听线程**——
//! `submit` 只是往通道里放一条命令，真正的解码 / 哈希 / 入库在存储线程。

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use erwuzai_platform::{
    ClipboardBackend, EchoGuard, PlatformResult, RawClipboard, SourceAppResolver,
    SystemClipboard, SystemSourceApp, Watcher,
};
use erwuzai_store::{CapturedItem, Payload, StoreCommand, StoreHandle};

pub fn start(
    handle: StoreHandle,
    guard: Arc<Mutex<EchoGuard>>,
) -> PlatformResult<Box<dyn Watcher>> {
    let sink = Arc::new(move |raw: RawClipboard| {
        let backend = SystemClipboard::new();
        let hash = hash_raw(&raw);
        // 序列号在回调当下取：它和"这次写剪贴板"是同一时刻的值，
        // 回放侧 note_sequence 记的也是同一个数。
        let sequence = backend.sequence();
        let suppress = guard
            .lock()
            .map(|mut g| g.should_suppress(&hash, sequence))
            .unwrap_or(false);
        if suppress {
            return;
        }
        // 来源应用要尽早取：晚一步原进程可能已退出或剪贴板已易主。
        let source_app = SystemSourceApp.resolve();
        let (cmd, _reply) = StoreCommand::capture(to_captured(raw, source_app));
        handle.submit(cmd);
    });

    SystemClipboard::new().watch(sink)
}

/// 回声判定用的规范哈希。**必须和回放写入时用的是同一个函数**，
/// 否则守卫永远匹配不上。注意它和存储层的 BLAKE3 不是一回事：
/// 存储层哈希的是规范化后的载荷，这里哈希的是"将要写进剪贴板的字节"。
pub fn hash_raw(raw: &RawClipboard) -> [u8; 32] {
    let mut buf = Vec::new();
    match raw {
        RawClipboard::Text(text) => buf.extend_from_slice(text.as_bytes()),
        RawClipboard::Rich { plain, html } => {
            buf.extend_from_slice(plain.as_bytes());
            buf.push(0);
            if let Some(html) = html {
                buf.extend_from_slice(html.as_bytes());
            }
        }
        RawClipboard::Files(paths) => {
            for path in paths {
                buf.extend_from_slice(path.to_string_lossy().as_bytes());
                buf.push(0);
            }
        }
        RawClipboard::Image { png, .. } => buf.extend_from_slice(png),
    }
    *blake3::hash(&buf).as_bytes()
}

fn to_captured(raw: RawClipboard, source_app: Option<String>) -> CapturedItem {
    let payload = match raw {
        RawClipboard::Text(text) => Payload::Text(text),
        RawClipboard::Rich { plain, html } => Payload::RichText { plain, html },
        RawClipboard::Files(paths) => Payload::FileList(paths),
        RawClipboard::Image { png, width, height } => Payload::Image { png, width, height },
    };
    CapturedItem {
        payload,
        source_app,
        captured_at: now_ms(),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
