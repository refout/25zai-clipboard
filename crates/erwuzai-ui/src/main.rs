//! 25zai 桌面入口。
//!
//! 三条路径，都在同一个 exe 里：
//!
//! - 不带参数：确保守护进程在跑，然后让它把 UI 拉起来（启动器）
//! - --daemon：无窗口常驻，握着存储层、监听、热键、托盘
//! - --ui：eframe 窗口，只通过回环 TCP 跟守护进程说话，隐藏即退出
//!
//! 拆成两个进程的原因是内存：eframe 的窗口和 GL 上下文创建之后收不回去，
//! 隐藏时那一百多 MB 一直挂着。UI 退出之后只剩守护进程的二十来 MB。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod daemon;
mod fonts;
mod listener;
mod secrets;

use std::error::Error;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use erwuzai_core::{request_show, Paths, SingleInstance};
use erwuzai_store::{
    CapturedItem, ClipboardStore, Payload, RedbStore, Remote, StoreCommand, StoreHandle,
};

type AnyError = Box<dyn Error + Send + Sync>;

fn main() -> eframe::Result<()> {
    if let Err(e) = run() {
        eprintln!("25zai: {e}");
        std::process::exit(1);
    }
    Ok(())
}

fn run() -> Result<(), AnyError> {
    // 开发用：25zai --put-image <路径> 把一张图推上剪贴板，
    // 用来验证监听侧的图片采集。必须在抢实例锁之前处理，
    // 因为它要和正在运行的实例并存。
    if let Some(path) = arg_value("--put-image") {
        use erwuzai_platform::{ClipboardBackend, RawClipboard, SystemClipboard};
        let bytes = std::fs::read(&path)?;
        if path.to_ascii_lowercase().ends_with(".png") {
            // 模拟浏览器/截图工具：剪贴板上直接就是 PNG 格式，而不是 DIB。
            // 走这条路才能验证"零解码"快路径。
            let ctx = clipboard_rs::ClipboardContext::new().map_err(|e| e.to_string())?;
            use clipboard_rs::Clipboard as _;
            ctx.set_buffer("PNG", bytes).map_err(|e| e.to_string())?;
        } else {
            SystemClipboard::new().write(&RawClipboard::Image {
                png: bytes,
                width: 0,
                height: 0,
            })?;
        }
        println!("image pushed to clipboard: {path}");
        return Ok(());
    }

    // 开发用：25zai --put-rich <文本> 以富文本形式写剪贴板，
    // 用来验证"纯文本回放"是否真的剥掉了格式。
    if let Some(text) = arg_value("--put-rich") {
        use erwuzai_platform::{ClipboardBackend, RawClipboard, SystemClipboard};
        SystemClipboard::new().write(&RawClipboard::Rich {
            plain: text.clone(),
            html: Some(format!("<p><b>{text}</b></p>")),
        })?;
        println!("rich text pushed to clipboard");
        return Ok(());
    }

    // 开发用：25zai --pack-icon <源图> <目标 png> 把任意图片压成内嵌托盘图标。
    // 不碰数据库，所以放在抢实例锁之前。
    if let Some((src, dst)) = arg_pair("--pack-icon") {
        let img = image::open(&src)?;
        let icon = img.thumbnail(64, 64);
        if let Some(parent) = std::path::Path::new(&dst).parent() {
            std::fs::create_dir_all(parent)?;
        }
        icon.save(&dst)?;
        println!("packed {}x{} -> {dst}", icon.width(), icon.height());
        return Ok(());
    }

    // 开发用：25zai --pack-image <源图> <目标 png> <边长> 生成界面用的小图。
    if let Some((src, dst, size)) = arg_triple("--pack-image") {
        let img = image::open(&src)?;
        let size: u32 = size.parse().map_err(|_| "边长必须是数字")?;
        let small = img.thumbnail(size, size);
        if let Some(parent) = std::path::Path::new(&dst).parent() {
            std::fs::create_dir_all(parent)?;
        }
        small.save(&dst)?;
        println!("packed {}x{} -> {dst}", small.width(), small.height());
        return Ok(());
    }

    // 开发用：25zai --peek 打印剪贴板当前内容，排查采集侧问题。
    if std::env::args().any(|a| a == "--peek") {
        use erwuzai_platform::{ClipboardBackend, RawClipboard, SystemClipboard};
        let described = match SystemClipboard::new().read()? {
            Some(RawClipboard::Text(t)) => format!("text, {} bytes", t.len()),
            Some(RawClipboard::Rich { plain, html }) => format!(
                "rich, plain {} bytes, html {} bytes",
                plain.len(),
                html.map(|h| h.len()).unwrap_or(0)
            ),
            Some(RawClipboard::Files(f)) => format!("files, {} entries", f.len()),
            Some(RawClipboard::Image { png, width, height }) => {
                format!("image, {width}x{height}, png {} bytes", png.len())
            }
            None => "nothing".to_string(),
        };
        println!("clipboard: {described}");
        return Ok(());
    }

    let paths = Paths::new(Paths::resolve());
    paths.ensure()?;

    if std::env::args().any(|a| a == "--daemon") {
        return daemon::run().map_err(|e| e.to_string().into());
    }
    if std::env::args().any(|a| a == "--ui") {
        return run_ui();
    }

    // 开发命令要独占数据库，不能因为"已有实例"就被转发掉。
    let dev_command = [
        "--dump",
        "--set-tray-icon",
        "--seed",
        "--peek",
        "--put-image",
        "--put-rich",
        "--pack-image",
    ]
    .iter()
    .any(|flag| std::env::args().any(|a| a == *flag));

    if !dev_command {
        return launch(&paths);
    }

    // —— 以下只服务开发命令：自己开库，不经过守护进程 ——
    let _instance = SingleInstance::acquire(&paths.lock()).ok();
    let store = RedbStore::open(
        paths.db(),
        paths.blobs(),
        std::sync::Arc::new(secrets::SecretshapeDetector),
    )?;

    // 开发用：25zai --set-tray-icon <路径> 设置托盘图标（- 表示恢复内置）。
    if let Some(path) = arg_value("--set-tray-icon") {
        let mut config = store.config()?;
        config.tray_icon = if path == "-" { None } else { Some(path) };
        store.update_config(config.clone())?;
        println!("tray_icon = {:?}", config.tray_icon);
        return Ok(());
    }

    // 开发用：25zai --set-theme <light|dark|system>
    if let Some(mode) = arg_value("--set-theme") {
        let mut config = store.config()?;
        config.theme = match mode.as_str() {
            "light" => erwuzai_store::ThemeMode::Light,
            "dark" => erwuzai_store::ThemeMode::Dark,
            _ => erwuzai_store::ThemeMode::System,
        };
        store.update_config(config)?;
        println!("theme = {mode}");
        return Ok(());
    }

    // 开发用：25zai --search <关键词> 走和 UI 相同的存储层搜索，验证正文命中。
    if let Some(query) = arg_value("--search") {
        let hits = store.search(&query, 20)?;
        println!("命中 {} 条", hits.len());
        for entry in hits {
            println!(
                "#{:<3} {}",
                entry.entry.id,
                entry.entry.preview.replace(['\r', '\n'], " ")
            );
        }
        return Ok(());
    }

    // 开发用：25zai --dump 打印当前历史，用于无 GUI 环境下验证采集链路。
    if std::env::args().any(|a| a == "--dump") {
        let page = store.page(None, 50)?;
        println!("共 {} 条", page.total_visible);
        for entry in page.items {
            println!(
                "#{:<3} {:<5} [{}] {}",
                entry.entry.id,
                kind_of(entry.entry.kind),
                entry.entry.source_app.as_deref().unwrap_or("-"),
                entry.entry.preview.replace(['\r', '\n'], " ")
            );
        }
        return Ok(());
    }

    // 开发用：25zai --seed 文本 直接写一条历史，方便在没有监听线程时验证 UI。
    if let Some(text) = seed_text() {
        let outcome = store.capture(CapturedItem {
            payload: Payload::Text(text),
            source_app: Some("seed".into()),
            captured_at: now_ms(),
        })?;
        println!("{outcome:?}");
        return Ok(());
    }

    Ok(())
}

/// 启动器：守护进程在就直接叫它开 UI，不在就拉起来再叫。
fn launch(paths: &Paths) -> Result<(), AnyError> {
    if request_show(&paths.port()) {
        return Ok(());
    }

    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--daemon");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd.spawn()?;

    // 冷启动时守护进程要开库、扫孤儿 blob、建托盘，几百毫秒起步；
    // 给到 5 s，太紧会在慢盘上误判成"启动失败"。
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(50));
        if request_show(&paths.port()) {
            return Ok(());
        }
    }
    Err("守护进程启动超时".into())
}

/// UI 进程：连上守护进程，拿首屏和配置，然后开窗口。
fn run_ui() -> Result<(), AnyError> {
    let paths = Paths::new(Paths::resolve());
    let port = std::fs::read_to_string(paths.port())?;
    let (remote, events, ui_rx) = Remote::connect(&format!("127.0.0.1:{}", port.trim()))?;
    let handle = StoreHandle::remote(remote);

    let (cmd, rx) = StoreCommand::get_config();
    handle.submit(cmd);
    let config = rx
        .recv_timeout(Duration::from_secs(2))
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or_default();

    let (cmd, rx) = StoreCommand::page(None, 500);
    handle.submit(cmd);
    let page = rx
        .recv_timeout(Duration::from_secs(2))
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or_default();
    println!("loaded {} entries via daemon", page.items.len());

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([480.0, 420.0])
            .with_decorations(false)
            // 先不显示：首帧要先把窗口摆到光标处再露出来，
            // 否则会先闪一下默认位置，再跳到目标位置。
            .with_visible(false)
            .with_always_on_top()
            .with_taskbar(false),
        ..Default::default()
    };

    eframe::run_native(
        "25zai",
        options,
        Box::new(move |cc| {
            fonts::install_cjk(&cc.egui_ctx);
            Ok(Box::new(app::App::new(handle, events, ui_rx, config, page)))
        }),
    )
    .map_err(|e| e.to_string())?;

    Ok(())
}

fn kind_of(kind: erwuzai_store::Kind) -> &'static str {
    use erwuzai_store::Kind;
    match kind {
        Kind::Text => "文本",
        Kind::RichText => "富文本",
        Kind::Image => "图片",
        Kind::FileList => "文件",
    }
}

fn seed_text() -> Option<String> {
    arg_value("--seed")
}

/// 取 flag 之后的三个参数。
fn arg_triple(flag: &str) -> Option<(String, String, String)> {
    let args: Vec<String> = std::env::args().collect();
    let index = args.iter().position(|a| a == flag)?;
    Some((
        args.get(index + 1)?.clone(),
        args.get(index + 2)?.clone(),
        args.get(index + 3)?.clone(),
    ))
}

/// 取 flag 之后的两个参数。
fn arg_pair(flag: &str) -> Option<(String, String)> {
    let args: Vec<String> = std::env::args().collect();
    let index = args.iter().position(|a| a == flag)?;
    Some((args.get(index + 1)?.clone(), args.get(index + 2)?.clone()))
}

fn arg_value(flag: &str) -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == flag {
            return args.next();
        }
    }
    None
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
