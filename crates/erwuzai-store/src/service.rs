//! 服务线程：独占 `Box<dyn ClipboardStore>`，把命令翻译成事件。
//!
//! 这一层是 UI 与存储之间唯一的桥。它做三件事：
//! 1. 串行执行命令（redb 写事务因此天然串行化）；
//! 2. 把返回值翻译成 [`StoreEvent`]；
//! 3. 把错误同时回给调用方和事件流（UI 需要可见告警）。
//!
//! 不做的事：不缓存、不合并、不重试。合并与节流属于 UI 侧。

use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread::JoinHandle;

use crate::traits::{ClipboardStore, StoreCommand, StoreHandle, StoreResult};
use crate::types::*;

pub struct StoreService {
    pub handle: StoreHandle,
    pub events: Receiver<StoreEvent>,
    pub join: JoinHandle<()>,
}

impl StoreService {
    pub fn spawn(store: Box<dyn ClipboardStore>) -> std::io::Result<Self> {
        let (cmd_tx, cmd_rx) = channel::<StoreCommand>();
        let (evt_tx, evt_rx) = channel::<StoreEvent>();
        let join = std::thread::Builder::new()
            .name("25zai-store".into())
            .spawn(move || run(&*store, cmd_rx, evt_tx))?;
        Ok(Self {
            handle: StoreHandle::new(cmd_tx),
            events: evt_rx,
            join,
        })
    }
}

fn run(store: &dyn ClipboardStore, rx: Receiver<StoreCommand>, evt: Sender<StoreEvent>) {
    // 通道关闭（UI 退出）时自然结束。
    while let Ok(cmd) = rx.recv() {
        dispatch(store, cmd, &evt);
    }
}

fn dispatch(store: &dyn ClipboardStore, cmd: StoreCommand, evt: &Sender<StoreEvent>) {
    match cmd {
        StoreCommand::Capture { item, reply } => {
            let r = store.capture(item);
            match &r {
                Ok(CaptureOutcome::Inserted { entry, evicted }) => {
                    // 先发删除再发新增，UI 列表不会瞬时超过上限。
                    if !evicted.is_empty() {
                        let _ = evt.send(StoreEvent::Removed(evicted.clone()));
                    }
                    let _ = evt.send(StoreEvent::Added(vec![entry.clone()]));
                }
                Ok(CaptureOutcome::Moved { entry }) => {
                    let _ = evt.send(StoreEvent::Moved {
                        id: entry.entry.id,
                        seq: entry.seq,
                        last_used_at: entry.last_used_at,
                    });
                }
                Ok(CaptureOutcome::Skipped(_)) => {}
                Err(_) => {}
            }
            warn(&r, evt);
            let _ = reply.send(r);
        }
        StoreCommand::Page { cursor, limit, reply } => {
            let r = store.page(cursor, limit);
            warn(&r, evt);
            let _ = reply.send(r);
        }
        StoreCommand::Search { query, limit, reply } => {
            let r = store.search(&query, limit);
            warn(&r, evt);
            let _ = reply.send(r);
        }
        StoreCommand::Content { id, reply } => {
            let r = store.get_content(id);
            warn(&r, evt);
            let _ = reply.send(r);
        }
        StoreCommand::Touch { id, reply } => {
            let r = store.touch(id);
            if let Ok(TouchOutcome::Moved { seq, last_used_at }) = &r {
                let _ = evt.send(StoreEvent::Moved {
                    id,
                    seq: *seq,
                    last_used_at: *last_used_at,
                });
            }
            warn(&r, evt);
            let _ = reply.send(r);
        }
        StoreCommand::SetPinned { id, pinned, reply } => {
            let r = store.set_pinned(id, pinned);
            if let Ok(evicted) = &r {
                if !evicted.is_empty() {
                    let _ = evt.send(StoreEvent::Removed(evicted.clone()));
                }
            }
            warn(&r, evt);
            let _ = reply.send(r);
        }
        StoreCommand::Delete { id, reply } => {
            let r = store.delete(id);
            if let Ok(true) = &r {
                let _ = evt.send(StoreEvent::Removed(vec![id]));
            }
            warn(&r, evt);
            let _ = reply.send(r);
        }
        StoreCommand::ClearUnpinned { reply } => {
            let r = store.clear_unpinned();
            if let Ok(ids) = &r {
                if !ids.is_empty() {
                    let _ = evt.send(StoreEvent::Removed(ids.clone()));
                }
            }
            warn(&r, evt);
            let _ = reply.send(r);
        }
        StoreCommand::GetConfig { reply } => {
            let r = store.config();
            warn(&r, evt);
            let _ = reply.send(r);
        }
        StoreCommand::UpdateConfig { config, reply } => {
            let r = store.update_config(config.clone());
            if r.is_ok() {
                let _ = evt.send(StoreEvent::ConfigChanged(Box::new(config)));
            }
            if let Ok(evicted) = &r {
                if !evicted.is_empty() {
                    let _ = evt.send(StoreEvent::Removed(evicted.clone()));
                }
            }
            warn(&r, evt);
            let _ = reply.send(r);
        }
        StoreCommand::Stats { reply } => {
            let r = store.stats();
            warn(&r, evt);
            let _ = reply.send(r);
        }
    }
}

fn warn<T>(r: &StoreResult<T>, evt: &Sender<StoreEvent>) {
    if let Err(e) = r {
        let _ = evt.send(StoreEvent::Warning(e.to_string()));
    }
}
