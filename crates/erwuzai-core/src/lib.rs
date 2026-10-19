//! 进程级基础设施：数据目录、单实例锁、启动顺序。
//!
//! 这个 crate 不依赖 redb、不依赖 GPUI、不依赖任何第三方库。
//! 它是 `main` 最先碰到的东西，出错必须在任何窗口出现之前报出来。

pub mod instance;
pub mod paths;

pub use instance::{request_show, InstanceError, ShowServer, SingleInstance};
pub use paths::Paths;
