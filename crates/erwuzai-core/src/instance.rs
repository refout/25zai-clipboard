//! 单实例。
//!
//! 顺序很重要：**先抢实例锁，再开数据库**。反过来的话，第二个实例会先
//! 撞上 redb 的文件锁，报出来的错和"数据库损坏"难以区分。
//!
//! 用 `std::fs::File::try_lock`（Rust 1.89 稳定）：Windows 走 `LockFileEx`，
//! Unix 走 `flock`。锁由操作系统随句柄释放，进程被强杀也不会留下死锁文件——
//! 这正是"用 create_new 建锁文件"那套做法解决不了的问题。

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

#[derive(Debug)]
pub enum InstanceError {
    /// 已经有实例在跑，调用方应转发 "show" 后退出（退出码 0）。
    AlreadyRunning,
    Io(io::Error),
}

impl std::fmt::Display for InstanceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstanceError::AlreadyRunning => f.write_str("已有一个 25仔 实例在运行"),
            InstanceError::Io(e) => write!(f, "实例锁失败：{e}"),
        }
    }
}

impl std::error::Error for InstanceError {}

impl From<io::Error> for InstanceError {
    fn from(e: io::Error) -> Self {
        InstanceError::Io(e)
    }
}

/// 持有锁文件句柄；drop 即释放。
pub struct SingleInstance {
    _file: File,
}

impl SingleInstance {
    pub fn acquire(lock_path: &Path) -> Result<Self, InstanceError> {
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path)?;

        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => Err(InstanceError::AlreadyRunning),
            Err(TryLockError::Error(e)) => Err(InstanceError::Io(e)),
        }
    }
}

// ---------------------------------------------------------------------------
// 第二实例 → 首实例 的"唤出"通道
// ---------------------------------------------------------------------------

/// 首实例启动的回环监听。
///
/// 用 TCP 而不是命名管道 / Unix socket：两种平台接口不同，而这里只传一个
/// "show" 字符串，回环 TCP 是最小的公共实现。端口取 0 让系统分配，
/// 避免固定端口冲突，实际端口写进 `instance.port` 供第二实例读取。
pub struct ShowServer {
    _thread: std::thread::JoinHandle<()>,
}

impl ShowServer {
    pub fn start(port_file: &Path) -> io::Result<(Self, Receiver<()>)> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        if let Some(parent) = port_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(port_file, port.to_string())?;

        let (tx, rx) = mpsc::channel();
        let handle = std::thread::Builder::new()
            .name("25zai-ipc".into())
            .spawn(move || {
                for conn in listener.incoming() {
                    let Ok(mut conn) = conn else { continue };
                    let mut buf = [0u8; 16];
                    let read = conn.read(&mut buf).unwrap_or(0);
                    if buf[..read].starts_with(b"show") {
                        let _ = tx.send(());
                    }
                }
            })?;

        Ok((
            Self {
                _thread: handle,
            },
            rx,
        ))
    }
}

/// 第二实例调用：通知首实例把窗口叫出来。返回是否送达。
pub fn request_show(port_file: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(port_file) else {
        return false;
    };
    let Ok(port) = text.trim().parse::<u16>() else {
        return false;
    };
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    match TcpStream::connect_timeout(&addr, Duration::from_millis(500)) {
        Ok(mut stream) => stream.write_all(b"show").is_ok(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("25zai-lock-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn second_acquire_is_rejected() {
        let dir = tmp("dup");
        let path = dir.join("instance.lock");

        let first = SingleInstance::acquire(&path).unwrap();
        assert!(matches!(
            SingleInstance::acquire(&path),
            Err(InstanceError::AlreadyRunning)
        ));

        drop(first);
        // 释放后应能重新拿到
        assert!(SingleInstance::acquire(&path).is_ok());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn second_instance_can_wake_the_first() {
        let dir = tmp("ipc");
        let port_file = dir.join("instance.port");
        let (_server, rx) = ShowServer::start(&port_file).unwrap();

        assert!(request_show(&port_file), "请求应送达");
        assert!(rx.recv_timeout(Duration::from_secs(2)).is_ok(), "首实例应收到唤出");

        // 端口文件不存在时不应 panic，只返回 false
        assert!(!request_show(&dir.join("nope.port")));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn creates_missing_parent_directory() {
        let dir = tmp("nested");
        let path = dir.join("a/b/instance.lock");
        assert!(SingleInstance::acquire(&path).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }
}
