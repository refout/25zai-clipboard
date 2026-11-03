//! 守护进程：无窗口常驻。
//!
//! 拆分之后它握着全部"不能丢"的东西：实例锁、redb、剪贴板监听、全局热键、
//! 托盘。UI 只是租客——按热键才被拉起来，隐藏即退出。
//!
//! 这样做的原因很直接：eframe 的窗口和 GL 上下文一旦创建就收不回去，
//! 隐藏时那 120 MB 一直挂着。把 UI 变成独立进程之后，闲置时只剩这里的
//! 二十来 MB。
//!
//! 和 UI 之间走回环 TCP（协议见 `erwuzai_store::wire`）：第一条消息是
//! `show` 的算"第二实例敲门"，否则就当成一个 UI 会话，登记下来收事件。

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use erwuzai_core::i18n::tr;
use erwuzai_core::{InstanceError, Paths, SingleInstance};
use erwuzai_platform::{
    pump_messages, set_menu_dark_mode, EchoGuard, HotkeySpec, SystemHotkey, SystemTray, TrayEvent,
    SLOT_MAIN, SLOT_NOTE,
};
use erwuzai_store::{
    to_command, ClipboardStore, Msg, RedbStore, StoreHandle, StoreService, UiRequest, WireReply,
    WireRequest,
};

use crate::{listener, secrets};

type AnyError = Box<dyn std::error::Error + Send + Sync>;

/// 热键管理器不是 Send（里面是 Win32 的裸指针），只能留在主线程上，
/// 服务线程要改键就往这里投一个活。
struct HotkeyJob {
    slot: usize,
    spec: String,
    reply: std::sync::mpsc::Sender<Result<(), String>>,
}

#[derive(Clone)]
struct Ctx {
    handle: StoreHandle,
    guard: Arc<Mutex<EchoGuard>>,
    hotkey_tx: std::sync::mpsc::Sender<HotkeyJob>,
    /// 当前 UI 会话的写端。同一时刻只允许一个 UI。
    ui: Arc<Mutex<Option<TcpStream>>>,
    /// 连接线程（设置页的"退出"）和托盘都能叫停主循环。
    quit: Arc<std::sync::atomic::AtomicBool>,
    exe: PathBuf,
}

pub fn run() -> Result<(), AnyError> {
    let paths = Paths::new(Paths::resolve());
    paths.ensure()?;

    let _instance = match SingleInstance::acquire(&paths.lock()) {
        Ok(instance) => instance,
        Err(InstanceError::AlreadyRunning) => {
            // 已经有守护进程了。这里**不**去唤起 UI：开机自启的快捷方式就是
            // `--daemon`，重复拉起时弹窗等于每次登录都糊一脸。想唤起的走
            // 启动器（不带参数），它自己会发 show。
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };

    let store = RedbStore::open(
        paths.db(),
        paths.blobs(),
        Arc::new(secrets::SecretshapeDetector),
    )?;
    match store.sweep_orphan_blobs() {
        Ok(0) => {}
        Ok(n) => println!("swept {n} orphan blobs"),
        Err(e) => eprintln!("25zai: 清理孤儿 blob 失败：{e}"),
    }
    let config = store.config()?;
    // 语言要在建托盘之前定好——托盘菜单文字是这会儿取的。
    erwuzai_core::i18n::set_lang(
        config
            .language
            .resolve(erwuzai_platform::system_language_is_chinese()),
    );
    // 自启是"系统状态"，不是库里的数据：每次启动都按配置对齐一次，
    // 用户手工删了注册表值也会被补回来。
    let exe = std::env::current_exe()?;
    if let Err(e) = erwuzai_platform::autostart::set_enabled(config.autostart, &exe) {
        eprintln!("25zai: 设置开机自启失败：{e}");
    }
    let page = store.page(erwuzai_store::Scope::History, None, 1)?;
    println!("loaded {} entries from {}", page.total_visible, paths.db().display());

    let StoreService {
        handle,
        events,
        join: _join,
    } = StoreService::spawn(Box::new(store))?;

    let guard = Arc::new(Mutex::new(EchoGuard::default()));
    let _watcher = listener::start(handle.clone(), guard.clone())?;
    println!("clipboard watcher started");

    let mut hotkey = register_hotkeys(&config.hotkey, &config.note_hotkey);
    let (hotkey_tx, hotkey_rx) = std::sync::mpsc::channel::<HotkeyJob>();
    let tray = match SystemTray::new(
        tr("25仔 — 剪贴板历史"),
        config.tray_icon.as_deref().map(std::path::Path::new),
    ) {
        Ok(tray) => {
            println!("tray: created");
            Some(tray)
        }
        Err(e) => {
            eprintln!("25zai: 托盘创建失败：{e}");
            None
        }
    };

    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    std::fs::write(paths.port(), port.to_string())?;
    println!("daemon listening on 127.0.0.1:{port}");

    let ctx = Ctx {
        handle,
        guard,
        hotkey_tx,
        ui: Arc::new(Mutex::new(None)),
        quit: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        exe,
    };

    // 存储事件 → 当前 UI。没有 UI 连接时直接丢掉（下次连上会拿全量首屏）。
    // 配置变了顺手把托盘图标也换掉——设置页只能改库，托盘在守护进程手里。
    let tray_icon = Arc::new(Mutex::new(config.tray_icon.clone()));
    let tray_dirty = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let ctx = ctx.clone();
        let tray_icon = tray_icon.clone();
        let tray_dirty = tray_dirty.clone();
        std::thread::Builder::new()
            .name("25zai-events".into())
            .spawn(move || {
                while let Ok(evt) = events.recv() {
                    if let erwuzai_store::StoreEvent::ConfigChanged(cfg) = &evt {
                        if let Ok(mut slot) = tray_icon.lock() {
                            *slot = cfg.tray_icon.clone();
                        }
                        tray_dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                        // 自启开关是 UI 改的、守护进程执行的。
                        if let Err(e) =
                            erwuzai_platform::autostart::set_enabled(cfg.autostart, &ctx.exe)
                        {
                            eprintln!("25zai: 设置开机自启失败：{e}");
                        }
                    }
                    ctx.send_to_ui(&Msg::Evt(evt));
                }
            })?;
    }

    // 连接处理
    {
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("25zai-accept".into())
            .spawn(move || {
                for conn in listener.incoming() {
                    let Ok(conn) = conn else { continue };
                    let ctx = ctx.clone();
                    std::thread::spawn(move || ctx.serve(conn));
                }
            })?;
    }

    // 主线程：泵消息 + 轮询托盘和热键。
    loop {
        pump_messages();

        // 改键的活在这里落地（热键管理器不是 Send，不能挪到别的线程）。
        while let Ok(job) = hotkey_rx.try_recv() {
            let result = rebind(&mut hotkey, job.slot, &job.spec);
            let _ = job.reply.send(result);
        }

        if tray_dirty.swap(false, std::sync::atomic::Ordering::Relaxed) {
            if let Some(tray) = &tray {
                let path = tray_icon.lock().ok().and_then(|p| p.clone());
                tray.apply_icon(path.as_deref().map(std::path::Path::new));
            }
        }

        if let Some(slot) = hotkey.as_ref().and_then(|m| m.poll_pressed()) {
            // Alt+N 直接落在笔记 tab，省掉"先开窗再点一下"的那一步。
            ctx.open_ui(if slot == SLOT_NOTE {
                UiRequest::ShowNotes
            } else {
                UiRequest::Show
            });
        }

        if let Some(tray) = &tray {
            match tray.poll() {
                Some(TrayEvent::Show) => ctx.open_ui(UiRequest::Show),
                Some(TrayEvent::NewNote) => ctx.open_ui(UiRequest::ShowNotes),
                Some(TrayEvent::Settings) => ctx.open_ui(UiRequest::ShowSettings),
                Some(TrayEvent::Quit) => {
                    ctx.send_to_ui(&Msg::Ui(UiRequest::Quit));
                    return Ok(());
                }
                None => {}
            }
        }

        // 设置页点了"退出"：连接线程只是把旗子立起来，收尾留给主线程。
        if ctx.quit.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(());
        }

        std::thread::sleep(Duration::from_millis(24));
    }
}

/// 建一个热键管理器并注册两个组合。
///
/// 两个槽位互不牵连：`Alt+N` 被别的软件占用时，`Alt+V` 必须照常可用，
/// 反之亦然。所以注册失败只打日志，不整体放弃。
fn register_hotkeys(main: &str, note: &str) -> Option<SystemHotkey> {
    let manager = match SystemHotkey::new() {
        Ok(manager) => manager,
        Err(e) => {
            eprintln!("25zai: 快捷键不可用：{e}");
            return None;
        }
    };
    register_slot(&manager, SLOT_MAIN, main);
    register_slot(&manager, SLOT_NOTE, note);
    Some(manager)
}

fn register_slot(manager: &SystemHotkey, slot: usize, text: &str) {
    match HotkeySpec::parse(text) {
        Ok(spec) => {
            if let Err(e) = manager.rebind_at(slot, &spec) {
                eprintln!("25zai: 快捷键 {text} 注册失败：{e}");
            }
        }
        Err(e) => eprintln!("25zai: 快捷键配置无效（{text}）：{e}"),
    }
}

impl Ctx {
    fn send_to_ui(&self, msg: &Msg) {
        let Ok(guard) = self.ui.lock() else {
            return;
        };
        let Some(conn) = guard.as_ref() else {
            return;
        };
        let Ok(mut line) = serde_json::to_string(msg) else {
            return;
        };
        line.push('\n');
        let mut conn = conn;
        let _ = conn.write_all(line.as_bytes());
    }

    /// 改键要回主线程落地（热键管理器不是 Send），这里同步等结果。
    fn change_hotkey(&self, slot: usize, spec: String) -> WireReply {
        let (tx, rx) = std::sync::mpsc::channel();
        if self
            .hotkey_tx
            .send(HotkeyJob {
                slot,
                spec,
                reply: tx,
            })
            .is_err()
        {
            return WireReply::err_text("守护进程主循环已停止".into());
        }
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(())) => WireReply::ok_empty(),
            Ok(Err(e)) => WireReply::err_text(e),
            Err(_) => WireReply::err_text("改键超时".into()),
        }
    }
    /// UI 不在就拉起来；已经连着就只是让它显示出来。
    ///
    /// 参数就是"要打开成什么样"，三个入口（热键 / 托盘 / 第二实例）共用。
    fn open_ui(&self, req: UiRequest) {
        if self.ui.lock().map(|g| g.is_some()).unwrap_or(false) {
            self.send_to_ui(&Msg::Ui(req));
            return;
        }
        let mut cmd = std::process::Command::new(&self.exe);
        cmd.arg("--ui");
        // UI 是刚起的进程，没有连接可以推 UiRequest，只能靠启动参数。
        match req {
            UiRequest::ShowSettings => {
                cmd.arg("--settings");
            }
            UiRequest::ShowNotes => {
                cmd.arg("--notes");
            }
            _ => {}
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        if let Err(e) = cmd.spawn() {
            eprintln!("25zai: 拉起 UI 失败：{e}");
        }
    }

    fn serve(&self, stream: TcpStream) {
        let Ok(peer) = stream.try_clone() else {
            return;
        };
        let mut reader = BufReader::new(peer);
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        // 旧版第二实例的握手，保持兼容。
        if line.trim() == "show" {
            self.open_ui(UiRequest::Show);
            return;
        }

        // 用对端地址标记这一条会话：UI 退出和新 UI 连上可能交错，
        // 清理时不能无脑把槽位清空，否则会把刚连上的那个也踢掉。
        let Ok(mine) = stream.peer_addr() else {
            return;
        };
        if let Ok(mut slot) = self.ui.lock() {
            *slot = stream.try_clone().ok();
        }

        handle_line(self, &stream, line.trim_end());
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            handle_line(self, &stream, line.trim_end());
        }

        // UI 走了：只清掉自己的那一格，等下一次唤起。
        if let Ok(mut slot) = self.ui.lock() {
            if slot.as_ref().and_then(|s| s.peer_addr().ok()) == Some(mine) {
                *slot = None;
            }
        }
    }

    fn handle_request(&self, req: WireRequest) -> WireReply {
        match req {
            WireRequest::ArmEcho { hash } => {
                if let Ok(mut guard) = self.guard.lock() {
                    match hash {
                        Some(h) => guard.arm(h),
                        None => guard.disarm(),
                    }
                }
                WireReply::ok_empty()
            }
            WireRequest::SetMenuDark { dark } => {
                set_menu_dark_mode(dark);
                WireReply::ok_empty()
            }
            WireRequest::Quit => {
                self.quit.store(true, std::sync::atomic::Ordering::Relaxed);
                WireReply::ok_empty()
            }
            WireRequest::SetHotkey { spec } => self.change_hotkey(SLOT_MAIN, spec),
            WireRequest::SetNoteHotkey { spec } => self.change_hotkey(SLOT_NOTE, spec),
            other => match to_command(other) {
                Some((cmd, wait)) => {
                    self.handle.submit(cmd);
                    wait()
                }
                None => WireReply::err_text("不支持的请求".into()),
            },
        }
    }

}

/// 换绑热键：先注销旧的再注册新的，失败时把旧的装回去。
fn rebind(manager: &mut Option<SystemHotkey>, slot: usize, text: &str) -> Result<(), String> {
    let spec = HotkeySpec::parse(text).map_err(|e| e.to_string())?;
    match manager.as_mut() {
        Some(manager) => manager.rebind_at(slot, &spec).map_err(|e| e.to_string()),
        None => {
            let created = SystemHotkey::new().map_err(|e| e.to_string())?;
            created.rebind_at(slot, &spec).map_err(|e| e.to_string())?;
            *manager = Some(created);
            Ok(())
        }
    }
}

fn handle_line(ctx: &Ctx, stream: &TcpStream, line: &str) {
    if line.is_empty() {
        return;
    }
    let Ok(Msg::Req { id, body }) = serde_json::from_str::<Msg>(line) else {
        return;
    };
    let reply = ctx.handle_request(body);
    let Ok(mut out) = serde_json::to_string(&Msg::Rep { id, body: reply }) else {
        return;
    };
    out.push('\n');
    let mut stream = stream;
    let _ = stream.write_all(out.as_bytes());
}
