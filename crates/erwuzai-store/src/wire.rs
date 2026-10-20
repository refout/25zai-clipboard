//! 进程间协议：UI 进程 ⇄ 守护进程。
//!
//! 拆分之后存储层只存在于守护进程里（redb 的文件锁是独占的，两个进程没法同时开），
//! UI 要拿数据只能走这条连接。协议本身很小：一行一个 JSON，请求带 id，
//! 回复按 id 配对，事件和"唤起"是服务端主动推的。
//!
//! 为什么用 TCP：和 `instance.port` 那套一致，回环 TCP 是各平台都有的最小实现，
//! 而且天然支持"一个连接掉了不影响别的"。只监听 127.0.0.1，端口由系统分配。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::traits::{StoreCommand, StoreError, StoreResult};
use crate::types::*;

/// 守护进程推给 UI 的控制指令。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum UiRequest {
    Show,
    ShowSettings,
    Quit,
}

/// 一帧。请求/回复靠 id 配对，事件与控制指令没有 id。
#[derive(Debug, Serialize, Deserialize)]
pub enum Msg {
    Req { id: u64, body: WireRequest },
    Rep { id: u64, body: WireReply },
    Evt(StoreEvent),
    Ui(UiRequest),
}

/// 不带回执通道的请求体（`StoreCommand` 里的 `Sender` 过不了线）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WireRequest {
    Page { cursor: Option<Seq>, limit: usize },
    Search { query: String, limit: usize },
    Content { id: EntryId },
    Touch { id: EntryId },
    SetPinned { id: EntryId, pinned: bool },
    Delete { id: EntryId },
    ClearUnpinned,
    GetConfig,
    UpdateConfig { config: Config },
    Stats,
    /// 写剪贴板之前的回声抑制：`Some(hash)` 布防，`None` 撤防。
    ArmEcho { hash: Option<ContentHash> },
    /// 改全局快捷键。热键归守护进程所有，UI 只能请求。
    SetHotkey { spec: String },
    /// 托盘右键是 Win32 原生菜单，只认进程级标记——UI 换了主题就通知守护进程。
    SetMenuDark { dark: bool },
    /// 开发用：直接塞一条文本进历史（`25zai --seed`）。
    Seed { text: String },
    /// 退出整个程序：守护进程一起走。UI 设置页的"退出"走这条。
    Quit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ReplyBody {
    None,
    Page(Page),
    Entries(Vec<EntryView>),
    Content(Option<Content>),
    Touched(TouchOutcome),
    Ids(Vec<EntryId>),
    Bool(bool),
    Config(Config),
    Stats(Stats),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireReply {
    pub error: Option<String>,
    pub body: ReplyBody,
}

impl WireReply {
    pub fn ok(body: ReplyBody) -> Self {
        Self { error: None, body }
    }

    pub fn ok_empty() -> Self {
        Self::ok(ReplyBody::None)
    }

    pub fn err_text(message: String) -> Self {
        Self {
            error: Some(message),
            body: ReplyBody::None,
        }
    }

    fn err(e: StoreError) -> Self {
        Self {
            error: Some(e.to_string()),
            body: ReplyBody::None,
        }
    }
}

/// 服务端：把 wire 请求翻译成 `StoreCommand` + 一个"等回执"的闭包。
///
/// 存储层只跑在独占线程上，所以服务端不能直接调 `ClipboardStore`，
/// 必须走命令通道。`ArmEcho` / `SetHotkey` 不碰存储层，返回 `None`，
/// 由守护进程自己处理。
pub type ReplyWaiter = Box<dyn FnOnce() -> WireReply + Send>;

const WAIT: std::time::Duration = std::time::Duration::from_secs(2);

pub fn to_command(req: WireRequest) -> Option<(StoreCommand, ReplyWaiter)> {
    let pair = match req {
        WireRequest::Page { cursor, limit } => {
            let (cmd, rx) = StoreCommand::page(cursor, limit);
            let waiter: ReplyWaiter = Box::new(move || match rx.recv_timeout(WAIT) {
                Ok(Ok(page)) => WireReply::ok(ReplyBody::Page(page)),
                Ok(Err(e)) => WireReply::err(e),
                Err(_) => WireReply::err(StoreError::Remote("存储线程超时".into())),
            });
            (cmd, waiter)
        }
        WireRequest::Search { query, limit } => {
            let (cmd, rx) = StoreCommand::search(query, limit);
            let waiter: ReplyWaiter = Box::new(move || match rx.recv_timeout(WAIT) {
                Ok(Ok(entries)) => WireReply::ok(ReplyBody::Entries(entries)),
                Ok(Err(e)) => WireReply::err(e),
                Err(_) => WireReply::err(StoreError::Remote("存储线程超时".into())),
            });
            (cmd, waiter)
        }
        WireRequest::Content { id } => {
            let (cmd, rx) = StoreCommand::content(id);
            let waiter: ReplyWaiter = Box::new(move || match rx.recv_timeout(WAIT) {
                Ok(Ok(content)) => WireReply::ok(ReplyBody::Content(content)),
                Ok(Err(e)) => WireReply::err(e),
                Err(_) => WireReply::err(StoreError::Remote("存储线程超时".into())),
            });
            (cmd, waiter)
        }
        WireRequest::Touch { id } => {
            let (cmd, rx) = StoreCommand::touch(id);
            let waiter: ReplyWaiter = Box::new(move || match rx.recv_timeout(WAIT) {
                Ok(Ok(outcome)) => WireReply::ok(ReplyBody::Touched(outcome)),
                Ok(Err(e)) => WireReply::err(e),
                Err(_) => WireReply::err(StoreError::Remote("存储线程超时".into())),
            });
            (cmd, waiter)
        }
        WireRequest::SetPinned { id, pinned } => {
            let (cmd, rx) = StoreCommand::set_pinned(id, pinned);
            let waiter: ReplyWaiter = Box::new(move || match rx.recv_timeout(WAIT) {
                Ok(Ok(ids)) => WireReply::ok(ReplyBody::Ids(ids)),
                Ok(Err(e)) => WireReply::err(e),
                Err(_) => WireReply::err(StoreError::Remote("存储线程超时".into())),
            });
            (cmd, waiter)
        }
        WireRequest::Delete { id } => {
            let (cmd, rx) = StoreCommand::delete(id);
            let waiter: ReplyWaiter = Box::new(move || match rx.recv_timeout(WAIT) {
                Ok(Ok(existed)) => WireReply::ok(ReplyBody::Bool(existed)),
                Ok(Err(e)) => WireReply::err(e),
                Err(_) => WireReply::err(StoreError::Remote("存储线程超时".into())),
            });
            (cmd, waiter)
        }
        WireRequest::ClearUnpinned => {
            let (cmd, rx) = StoreCommand::clear_unpinned();
            let waiter: ReplyWaiter = Box::new(move || match rx.recv_timeout(WAIT) {
                Ok(Ok(ids)) => WireReply::ok(ReplyBody::Ids(ids)),
                Ok(Err(e)) => WireReply::err(e),
                Err(_) => WireReply::err(StoreError::Remote("存储线程超时".into())),
            });
            (cmd, waiter)
        }
        WireRequest::GetConfig => {
            let (cmd, rx) = StoreCommand::get_config();
            let waiter: ReplyWaiter = Box::new(move || match rx.recv_timeout(WAIT) {
                Ok(Ok(config)) => WireReply::ok(ReplyBody::Config(config)),
                Ok(Err(e)) => WireReply::err(e),
                Err(_) => WireReply::err(StoreError::Remote("存储线程超时".into())),
            });
            (cmd, waiter)
        }
        WireRequest::UpdateConfig { config } => {
            let (cmd, rx) = StoreCommand::update_config(config);
            let waiter: ReplyWaiter = Box::new(move || match rx.recv_timeout(WAIT) {
                Ok(Ok(ids)) => WireReply::ok(ReplyBody::Ids(ids)),
                Ok(Err(e)) => WireReply::err(e),
                Err(_) => WireReply::err(StoreError::Remote("存储线程超时".into())),
            });
            (cmd, waiter)
        }
        WireRequest::Stats => {
            let (cmd, rx) = StoreCommand::stats();
            let waiter: ReplyWaiter = Box::new(move || match rx.recv_timeout(WAIT) {
                Ok(Ok(stats)) => WireReply::ok(ReplyBody::Stats(stats)),
                Ok(Err(e)) => WireReply::err(e),
                Err(_) => WireReply::err(StoreError::Remote("存储线程超时".into())),
            });
            (cmd, waiter)
        }
        WireRequest::Seed { text } => {
            let (cmd, rx) = StoreCommand::capture(CapturedItem {
                payload: Payload::Text(text),
                source_app: Some("seed".into()),
                captured_at: now_ms(),
            });
            let waiter: ReplyWaiter = Box::new(move || match rx.recv_timeout(WAIT) {
                Ok(Ok(_)) => WireReply::ok_empty(),
                Ok(Err(e)) => WireReply::err(e),
                Err(_) => WireReply::err(StoreError::Remote("存储线程超时".into())),
            });
            (cmd, waiter)
        }
        WireRequest::ArmEcho { .. }
        | WireRequest::SetHotkey { .. }
        | WireRequest::SetMenuDark { .. }
        | WireRequest::Quit => return None,
    };
    Some(pair)
}

// ---------------------------------------------------------------------------
// 客户端
// ---------------------------------------------------------------------------

type Sink = Box<dyn FnOnce(WireReply) + Send>;

/// UI 侧的连接。写入串行化，回复按 id 分派到各自的回执通道。
pub struct Remote {
    out: Mutex<TcpStream>,
    pending: Mutex<HashMap<u64, Sink>>,
    next_id: AtomicU64,
    /// 正在等事件/指令的线程靠它判断连接是否还活着。
    alive: Mutex<bool>,
}

impl Remote {
    /// 建连并起一条读线程。返回 `(连接, 事件流, 控制流)`。
    pub fn connect(addr: &str) -> std::io::Result<(Arc<Self>, Receiver<StoreEvent>, Receiver<UiRequest>)> {
        let stream = TcpStream::connect(addr)?;
        stream.set_nodelay(true).ok();
        let reader = stream.try_clone()?;
        let remote = Arc::new(Self {
            out: Mutex::new(stream),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            alive: Mutex::new(true),
        });

        let (evt_tx, evt_rx) = channel();
        let (ui_tx, ui_rx) = channel();

        let me = remote.clone();
        std::thread::Builder::new()
            .name("25zai-wire".into())
            .spawn(move || {
                let mut lines = BufReader::new(reader);
                let mut buf = String::new();
                loop {
                    buf.clear();
                    match lines.read_line(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let Ok(msg) = serde_json::from_str::<Msg>(buf.trim_end()) else {
                        continue;
                    };
                    match msg {
                        Msg::Rep { id, body } => {
                            if let Some(sink) = me.pending.lock().ok().and_then(|mut m| m.remove(&id)) {
                                sink(body);
                            }
                        }
                        Msg::Evt(e) => {
                            let _ = evt_tx.send(e);
                        }
                        Msg::Ui(cmd) => {
                            let _ = ui_tx.send(cmd);
                        }
                        Msg::Req { .. } => {}
                    }
                }
                if let Ok(mut alive) = me.alive.lock() {
                    *alive = false;
                }
            })?;

        Ok((remote, evt_rx, ui_rx))
    }

    pub fn is_alive(&self) -> bool {
        self.alive.lock().map(|a| *a).unwrap_or(false)
    }

    fn send(&self, msg: &Msg) -> bool {
        let Ok(mut line) = serde_json::to_string(msg) else {
            return false;
        };
        line.push('\n');
        let Ok(mut out) = self.out.lock() else {
            return false;
        };
        out.write_all(line.as_bytes()).is_ok()
    }

    fn call(&self, req: WireRequest, sink: Sink) -> bool {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(id, sink);
        } else {
            return false;
        }
        self.send(&Msg::Req { id, body: req })
    }

    /// 改全局快捷键，同步等结果。热键归守护进程，UI 只能请求。
    pub fn set_hotkey(&self, spec: &str) -> std::result::Result<(), String> {
        let (tx, rx) = channel();
        let ok = self.call(
            WireRequest::SetHotkey {
                spec: spec.to_string(),
            },
            Box::new(move |r| {
                let _ = tx.send(match (r.error, r.body) {
                    (Some(e), _) => Err(e),
                    (None, ReplyBody::None) => Ok(()),
                    _ => Err("回复类型不匹配".into()),
                });
            }),
        );
        if !ok {
            return Err("守护进程连接已断".into());
        }
        rx.recv_timeout(std::time::Duration::from_millis(800))
            .unwrap_or_else(|_| Err("守护进程无响应".into()))
    }

    /// 布防 / 撤防回声抑制。守护进程持有真正的 `EchoGuard`。
    pub fn arm_echo(&self, hash: Option<ContentHash>) -> bool {
        self.send(&Msg::Req {
            id: 0,
            body: WireRequest::ArmEcho { hash },
        })
    }

    /// 请守护进程退出（它会顺带把自己的托盘图标收掉）。
    pub fn quit(&self) -> bool {
        self.send(&Msg::Req {
            id: 0,
            body: WireRequest::Quit,
        })
    }

    /// 开发用：让守护进程直接收下一条文本。
    pub fn seed(&self, text: &str) -> std::result::Result<(), String> {
        let (tx, rx) = channel();
        let ok = self.call(
            WireRequest::Seed {
                text: text.to_string(),
            },
            Box::new(move |r| {
                let _ = tx.send(match (r.error, r.body) {
                    (Some(e), _) => Err(e),
                    (None, ReplyBody::None) => Ok(()),
                    _ => Err("回复类型不匹配".into()),
                });
            }),
        );
        if !ok {
            return Err("守护进程连接已断".into());
        }
        rx.recv_timeout(std::time::Duration::from_secs(3))
            .unwrap_or_else(|_| Err("守护进程无响应".into()))
    }

    /// 同步托盘菜单的明暗（Win32 菜单是进程级的）。
    pub fn set_menu_dark(&self, dark: bool) -> bool {
        self.send(&Msg::Req {
            id: 0,
            body: WireRequest::SetMenuDark { dark },
        })
    }

    /// 把 `StoreCommand` 翻译成 wire 请求。
    pub fn submit(&self, cmd: StoreCommand) -> bool {
        match cmd {
            StoreCommand::Page { cursor, limit, reply } => self.call(
                WireRequest::Page { cursor, limit },
                Box::new(move |r| {
                    let _ = reply.send(expect(r, |b| match b {
                        ReplyBody::Page(p) => Some(p),
                        _ => None,
                    }));
                }),
            ),
            StoreCommand::Search { query, limit, reply } => self.call(
                WireRequest::Search { query, limit },
                Box::new(move |r| {
                    let _ = reply.send(expect(r, |b| match b {
                        ReplyBody::Entries(e) => Some(e),
                        _ => None,
                    }));
                }),
            ),
            StoreCommand::Content { id, reply } => self.call(
                WireRequest::Content { id },
                Box::new(move |r| {
                    let _ = reply.send(expect(r, |b| match b {
                        ReplyBody::Content(c) => Some(c),
                        _ => None,
                    }));
                }),
            ),
            StoreCommand::Touch { id, reply } => self.call(
                WireRequest::Touch { id },
                Box::new(move |r| {
                    let _ = reply.send(expect(r, |b| match b {
                        ReplyBody::Touched(o) => Some(o),
                        _ => None,
                    }));
                }),
            ),
            StoreCommand::SetPinned { id, pinned, reply } => self.call(
                WireRequest::SetPinned { id, pinned },
                Box::new(move |r| {
                    let _ = reply.send(expect(r, |b| match b {
                        ReplyBody::Ids(ids) => Some(ids),
                        _ => None,
                    }));
                }),
            ),
            StoreCommand::Delete { id, reply } => self.call(
                WireRequest::Delete { id },
                Box::new(move |r| {
                    let _ = reply.send(expect(r, |b| match b {
                        ReplyBody::Bool(v) => Some(v),
                        _ => None,
                    }));
                }),
            ),
            StoreCommand::ClearUnpinned { reply } => self.call(
                WireRequest::ClearUnpinned,
                Box::new(move |r| {
                    let _ = reply.send(expect(r, |b| match b {
                        ReplyBody::Ids(ids) => Some(ids),
                        _ => None,
                    }));
                }),
            ),
            StoreCommand::GetConfig { reply } => self.call(
                WireRequest::GetConfig,
                Box::new(move |r| {
                    let _ = reply.send(expect(r, |b| match b {
                        ReplyBody::Config(c) => Some(c),
                        _ => None,
                    }));
                }),
            ),
            StoreCommand::UpdateConfig { config, reply } => self.call(
                WireRequest::UpdateConfig { config },
                Box::new(move |r| {
                    let _ = reply.send(expect(r, |b| match b {
                        ReplyBody::Ids(ids) => Some(ids),
                        _ => None,
                    }));
                }),
            ),
            StoreCommand::Stats { reply } => self.call(
                WireRequest::Stats,
                Box::new(move |r| {
                    let _ = reply.send(expect(r, |b| match b {
                        ReplyBody::Stats(s) => Some(s),
                        _ => None,
                    }));
                }),
            ),
            // 采集只发生在守护进程内部，UI 不会发。
            StoreCommand::Capture { reply, .. } => {
                let _ = reply.send(Err(StoreError::Remote("采集只能由守护进程发起".into())));
                false
            }
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn expect<T>(reply: WireReply, pick: impl FnOnce(ReplyBody) -> Option<T>) -> StoreResult<T> {
    if let Some(e) = reply.error {
        return Err(StoreError::Remote(e));
    }
    match pick(reply.body) {
        Some(v) => Ok(v),
        None => Err(StoreError::Remote("回复类型不匹配".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_and_reply_round_trip() {
        let msg = Msg::Req {
            id: 7,
            body: WireRequest::Search {
                query: "你好".into(),
                limit: 20,
            },
        };
        let line = serde_json::to_string(&msg).unwrap();
        assert!(!line.contains('\n'));
        let back: Msg = serde_json::from_str(&line).unwrap();
        match back {
            Msg::Req { id, body: WireRequest::Search { query, limit } } => {
                assert_eq!(id, 7);
                assert_eq!(query, "你好");
                assert_eq!(limit, 20);
            }
            _ => panic!("解析结果不对"),
        }
    }

    #[test]
    fn events_survive_the_wire() {
        let evt = StoreEvent::Moved {
            id: 3,
            seq: 9,
            last_used_at: 123,
        };
        let line = serde_json::to_string(&Msg::Evt(evt)).unwrap();
        match serde_json::from_str::<Msg>(&line).unwrap() {
            Msg::Evt(StoreEvent::Moved { id, seq, .. }) => {
                assert_eq!((id, seq), (3, 9));
            }
            _ => panic!("事件没解析出来"),
        }
    }
}
