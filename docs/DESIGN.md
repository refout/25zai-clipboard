# 25仔（25zai）工程设计 v2

> 25zai — Featherweight clipboard memory for every desktop.

本文档在 v1 的基础上做了语义修正与缺口补齐。**标注 `[待验证]` 的条目是承重假设，
落地前必须实测**，不要在它们上面直接排期。

## 0. v2 修订摘要

| # | 问题 | 处理 | 章节 |
|---|---|---|---|
| 1 | 回放写剪贴板会触发自己的监听，纯文本粘贴会污染历史 | 新增回声抑制层，按平台用序列号 / changeCount / 哈希窗口 | §5.2 |
| 2 | 没有单实例约束，redb 独占锁会直接失败，双实例互相回声 | 启动先抢实例锁，第二实例转发 show 后退出 | §11 |
| 3 | 清理声称 O(k)，但没有非固定计数与游标 | `meta` 表存 `non_pinned_count` + `cleanup_cursor` | §5.4 |
| 4 | `entries` 不可变导致 timestamp 永远是首次复制时间 | `usage_order` 槽位改成 `{entry_id, last_used_at}` | §4.1 |
| 5 | "自动粘贴"没定义，而它决定依赖与权限模型 | MVP 只写剪贴板，自动粘贴推迟到 P2 并标注平台差异 | §3 / §7 |
| 6 | 不做托盘就没有退出入口 | MVP 弹窗内提供设置与退出菜单项 | §7 |
| 7 | 存储线程挂在 `cx.background_spawn()` 上会长期占用线程池 | 改为独占 `std::thread` + 通道 | §2 |
| 8 | 分页与虚拟列表是两套重复机制 | 列表走内存索引 + 虚拟列表，分页只服务全量搜索 | §6 / §7 |
| 9 | 内存 500 与上限 500 是两个不同的数 | 索引容量独立配置，UI 明示覆盖范围 | §5.5 |
| 10 | 搜索结果与删除事件存在竞态 | 搜索结果只回 id，UI 用自有索引解析，未知 id 丢弃 | §5.5 |
| 11 | 全量搜索只匹配 `preview`，正文命中搜不到 | 明确定义为预览搜索并在 UI 标注 | §6 |
| 12 | 敏感检测放在监听线程会拖慢 watcher 回调 | 监听线程只搬字节，解码 / 哈希 / 检测全在存储线程 | §5.1 |
| 13 | 排除应用在 macOS / Wayland 上做不到 | 平台矩阵降级为 可靠 / 启发式 / 不可用 | §8 |
| 14 | 首屏两次事务之间可能读到已删除条目 | 扫 seq 与读 entries 合并进同一个读事务 | §4.2 |
| 15 | 没有 schema 版本，后续加表无法迁移 | `meta[schema_version]` 从第一天写入 | §4.1 |
| 16 | redb 是 COW，删除不缩文件 | 单条上限 + 设置页显示库大小 | §4.4 |
| 17 | 敏感条目默认落盘，只防窥屏不防磁盘 | 默认"跳过采集"，脱敏保留改为显式选项 | §9 |
| 18 | 没有测试计划 | 模型化属性测试 + 崩溃一致性测试列为 P0 交付物 | §12 |

---

## 1. 命名与工程标识

| 项目 | 值 |
|---|---|
| 中文名 | 25仔 |
| 英文名 / 可执行文件 | `25zai` |
| 仓库名 | `25zai-clipboard` |
| Cargo 包名 | `erwuzai`（`[[bin]] name = "25zai"`） |
| 工作区 | `erwuzai-core` / `erwuzai-store` / `erwuzai-ui` / `erwuzai-platform` |
| 数据目录 | Win `%APPDATA%\25zai\`；macOS `~/Library/Application Support/25zai/`；Linux `~/.local/share/25zai/` |
| 数据库 / Blob | `25zai.db` / `blobs/` |
| 设置 | **只存 redb 的 `config` 表**，不再维护 `25zai.toml` |
| 实例锁 | 数据目录下的 `instance.lock` |
| 目录覆盖 | 环境变量 `25ZAI_DATA_DIR`（便携模式 / 测试） |
| 托盘 tooltip | `25仔 — 剪贴板历史` |
| 设置页标题 | `25仔 设置` |

包名 `erwuzai`、二进制名 `25zai`，避免 crate 标识符变成 `_25zai`。

**为什么删掉 `25zai.toml`**：v1 同时有 TOML 配置文件和 redb 的 `config` 表，
是两份真相。设置项改了要写两处，读的时候还要决定谁优先；更糟的是 TOML 和数据库
无法原子更新——改上限触发清理时，TOML 已写、清理失败，两边就永久不一致。
设置全部进 `config` 表后，改配置和改数据在同一个写事务里。
唯一保留的外部输入是 `25ZAI_DATA_DIR`，它必须在打开数据库之前生效，
本来就放不进数据库。

---

## 2. 架构

两个进程，同一个 exe（`--daemon` / `--ui`，不带参数是启动器）：

```
┌───────────────────────────── 守护进程（常驻，无窗口） ─────────────────────────────┐
│ 主线程：消息泵（托盘 / 热键）· 连接处理 · 按热键拉起 UI                            │
│ 存储线程（std::thread，独占 redb::Database）                                        │
│   解码 · 敏感检测 · BLAKE3 · 去重 · 清理 · 查询 → StoreEvent                       │
│ 监听线程（clipboard-rs ClipboardWatcherContext）                                   │
│   只搬字节 · 回声比对 · bounded 通道投递                                            │
│ redb：entries / texts / content_index / usage_order / seq_index / meta / config    │
│ 图片 blob → 文件系统（DB 只存哈希引用）                                             │
└───────────────────────────────┬───────────────────────────────────────────────────┘
                    回环 TCP（127.0.0.1，端口由系统分配）
                    一行一个 JSON：Req/Rep 按 id 配对，Evt/Ui 服务端主动推
┌───────────────────────────────┴───────────────────────────────────────────────────┐
│ UI 进程（eframe/egui，隐藏即退出）                                                 │
│ 弹窗 · 设置页 · 内存索引（最近 N 条轻量元数据）                                     │
│ StoreHandle::remote：StoreCommand → wire 请求，回执走各自的 channel                │
└───────────────────────────────────────────────────────────────────────────────────┘
```

为什么拆：eframe 的窗口和 GL 上下文一旦创建就收不回去（根 viewport 一关整个进程就退出），
隐藏时那一百多 MB 会一直挂着。实测隐藏只释放 2 MB。拆开之后闲置只剩守护进程。
代价是存储层只能有一个主人（redb 的文件锁是独占的），UI 必须走 IPC。

四条硬规则：

1. **redb 事务绝不在 UI 主线程执行。**
2. **存储线程是独占的 `std::thread`，不是 UI 框架的任务池。**
   长期阻塞会挤占 UI 的调度线程，而且线程生命周期会和 App 生命周期耦合，
   出问题难以隔离。
3. **监听线程不碰数据库、不做哈希、不解码。** 它只负责把原始字节搬进通道，
   唯一例外是回声比对（见 §5.2），且只在抑制窗口内触发。
4. **UI 进程不碰数据库，也不持有热键 / 托盘 / 回声守卫。** 这三样都必须活过
   UI 的生命周期，所以归守护进程；UI 要改键、要布防回声，都发请求。

### 2.1 进程边界上的几处取舍

- **协议用回环 TCP 而不是命名管道**：和 `instance.port` 那套一致，各平台都有，
  且天然支持"一个连接掉了不影响别的"。只监听 127.0.0.1。
- **`StoreCommand` 里的 `Sender` 过不了线**：`wire.rs` 里另立一套 `WireRequest`，
  客户端把 `StoreCommand` 翻译成请求、把回复塞回原来的 `Sender`，
  所以 `app.rs` 一行都不用改——它看到的还是 `StoreHandle`。
- **热键管理器不是 `Send`**（里面是 Win32 裸指针），只能留在守护进程主线程。
  服务线程改键时往主线程投一个 `HotkeyJob`，等回执，超时 2 s。
- **托盘菜单的明暗是进程级的**（uxtheme 的 `SetPreferredAppMode`），
  所以 UI 换主题时要把明暗推给守护进程。
- **存储事件在 UI 不在时直接丢**：下次连上会拿全量首屏，不需要补发。

### 2.2 实测（release，Windows 11）

| 状态 | 进程数 | RSS |
|---|---|---|
| 闲置（只有守护进程） | 1 | **13–17 MB** |
| 弹窗可见 | 2 | 守护进程 17 MB + UI 约 110 MB |
| 拆分前的隐藏态 | 1 | 121.6 MB |

冷启动（进程不在，直接双击）：启动器拉起守护进程 → 轮询端口 → 唤起 UI，
整条链路 3 s 内完成；之后每次唤起只起 UI 进程，约 0.7 s。

代码分层对应 `erwuzai-store`：`ClipboardStore` 是纯同步契约，
`StoreCommand` / `StoreHandle` 是通道，服务线程负责事件翻译。
trait 不产生事件，所以存储层的每条不变量都能在单测里直接对拍。

---

## 3. 功能范围矩阵

| 功能项 | 阶段 | 备注 |
|---|---|---|
| 文本 / 图片 / 文件列表采集 | MVP | |
| 连续去重（BLAKE3） | MVP | |
| 重新复制 / 回放后排到第一 | MVP | |
| 自动清理（条数上限 + 固定豁免） | MVP | |
| 首屏列表 + 虚拟滚动 | MVP | 取消"查看更多"按钮 |
| 实时搜索（内存索引 + 存储兜底） | MVP | 仅匹配预览 |
| 全局快捷键唤起 | MVP | |
| 弹窗失焦自动隐藏 | MVP | |
| 回放 = 写剪贴板，用户自己按 Ctrl+V | MVP | **不模拟按键** |
| 设置 / 退出入口 | MVP | 弹窗内菜单，见 §7 |
| 纯文本粘贴 | P1 | 依赖 §5.2 回声抑制 |
| 类型 / 来源过滤 | P1 | 来源过滤在 macOS/Wayland 降级 |
| 键盘导航 / 数字快速选择 | P1 | |
| 敏感检测 + 排除应用 | P1 | 排除应用仅 Windows 可靠 |
| 系统托盘 | P1 | MVP 用菜单项替代 |
| 固定 / 收藏 UI | P1 | 清理豁免逻辑已在 MVP |
| 自动粘贴（模拟按键） | P2 | 平台差异大，见 §7 |
| 备注 / 分组 / Snippets / 智能分类 / 导入导出 | P2 | |
| 加密存储 | 不纳入 | 会破坏 redb 范围扫描与零拷贝 |
| 多设备同步 | 不纳入 | |

---

## 4. 存储层

以 **redb 4.x** 为基线。redb 3.0 起不兼容 v2 磁盘格式，打开旧文件返回
`UpgradeRequired`：必须捕获该错误、备份旧文件、建新库或引导迁移，
**不允许静默失败或自动删库**。

### 4.1 表结构

| 表 | 键 | 值 | 作用 |
|---|---|---|---|
| `entries` | `u64` | `EntryRecord`（postcard） | 内容元数据，永不因顺序变化重写 |
| `texts` | `u64` | 全文 / 完整路径列表 | 按需加载，列表查询不触达 |
| `content_index` | `[u8;32]` | `u64` | 去重，永远指向同一内容 |
| **`usage_order`** | `u64`（seq） | **16B 槽位 `{entry_id, last_used_at}`** | 列表顺序的唯一依据 |
| `seq_index` | `u64`（entry_id） | `u64`（seq） | 反向索引，移动 / 清理时定位旧 seq |
| `meta` | `&str` | 字节 | `schema_version` / `next_entry_id` / `next_seq` / `non_pinned_count` / `cleanup_cursor` |
| `config` | `&str` | postcard(`Config`) | 配置整块读写 |

`EntryRecord`：`id`、`hash`、`kind`、`preview`（截断到 500 字符；文件列表为
"前 3 个文件名 + 总数"）、`created_at`、`source_app`、`pinned`、`blob_ref`、`truncated`。

**为什么 `last_used_at` 在槽位里而不是 `entries` 里**：`entries` 不可变是这套设计的
前提，但用户看到的必须是"最近一次复制时间"。把时间戳塞进 usage 槽位，既保住了
不可变，又让时间清理不必回表——因为 seq 与时间同向单调（不变量 I5），
过期条目必然是 `usage_order` 的一个前缀。

**写入成本**：一次采集命中 5 棵 B-tree（entries / texts / content_index /
usage_order / seq_index），一次回放只碰 2 棵（usage_order / seq_index）。
写放大与内容大小无关。

### 4.2 分页

```
首屏：page(None, 20)     → usage_order.range(..)?.rev().take(20)
更多：page(Some(cursor)) → usage_order.range(..cursor)?.rev().take(20)
```

- 用 **seq 游标**而不是 offset：头部插入或条目移动都不会造成翻页漂移。
- 扫 seq 与读 `entries` **必须在同一个读事务内**。跨事务会读到指向已删除条目的
  seq；即便如此，实现仍要容忍 `entries.get(id) == None` 并跳过。
- 复杂度 `O(log n + limit)`，与历史总量无关。
- 返回 `Page::total_visible`，供虚拟列表估算高度。

### 4.3 不变量

见 `crates/erwuzai-store/src/schema.rs` 顶部 I1–I6。摘要：

- I1 `seq_index` 与 `usage_order` 互为逆映射
- I2 `content_index` 与 `entries.hash` 一一对应
- I3 `texts` 的键集 ⊆ `entries` 的键集
- I4 `non_pinned_count` 与实际非固定条目数一致
- I5 seq 与 last_used_at 同向单调（时间戳做单调钳制抵御时钟回拨）
- I6 `cleanup_cursor` 只前进

任何跨表写入必须在同一个写事务内完成，否则这六条会破。

### 4.4 空间

redb 是 COW B-tree：删除只回收页到空闲列表，**文件不会自动缩小**。
所以必须有：

- `max_text_bytes`（默认 1 MiB）：超出的文本截断并置 `truncated`，
  避免一条 10 MB 日志把库撑大。
- 设置页显示 `db_bytes` / `blob_bytes`，并在超过阈值时提示"清空历史"。
- 定期（如每 30 天）或用户主动触发一次 compact。

### 4.5 启动迁移

```
open db
 ├─ Ok
 ├─ Err(UpgradeRequired) → 备份 25zai.db → 建新库 → 提示用户"旧历史已备份至 …"
 └─ Err(AlreadyOpen)     → 说明实例锁没拦住，走 §11 的转发路径
```

---

## 5. 生命周期

### 5.1 采集

```
[监听线程]
  on_clipboard_change
    → 读取原始数据（只搬字节）
    → 若回声抑制窗口处于 armed 状态：算哈希比对（见 §5.2）
    → 投递 RawCapture 到 bounded(64) 通道；满则丢最旧 + 记 Warning

[存储线程]
  解码 payload
    → 空内容 → Skipped(Empty)
    → 尺寸检查 → Skipped(TooLarge) 或截断
    → 文本：secretshape::is_probably_secret()
         → skip_sensitive ? Skipped(Sensitive) : 打脱敏标记
    → 图片：跳过敏感检测
    → BLAKE3
    → content_index 查重
        ├─ 未命中 → 新 entry_id + 新 seq → 写 5 张表 → 清理 → 提交
        └─ 命中   → 只换 seq（§5.3）
```

哈希与检测都在存储线程，监听线程的 `on_clipboard_change` 保持微秒级返回。
`secretshape` 的 57–60 ns 是纯函数基准，不含读取和解码，不要当成端到端延迟。

### 5.2 回声抑制（新增，MVP 必须）

回放要写剪贴板，写剪贴板会触发自己的监听。不处理的话：普通回放会空转一次去重，
**纯文本粘贴会生成一条新历史条目**，历史被自己污染。

```
写入前：echo_guard.arm(hash_of_written_bytes)
写入剪贴板
收到变更：
  ├─ 平台能拿到序列号 → 序列号相等则吞掉，disarm
  ├─ 否则算哈希 → 与 armed 哈希相等则吞掉，disarm
  └─ 300ms 超时 → 自动 disarm
写入失败 → 立即 disarm
```

| 平台 | 判定依据 | 可靠性 |
|---|---|---|
| Windows | `GetClipboardSequenceNumber()` | 精确 |
| macOS | `NSPasteboard.changeCount` | 精确 |
| Linux X11 / Wayland | 哈希 + 300ms 窗口 | 近似 |

关键细节：**guard 存的是"我们写进去的字节"的哈希，不是条目的哈希。**
纯文本粘贴写进去的内容与存储内容不同，只有前者能匹配上。

### 5.3 回放与重新复制：移动到新 seq

两者共用一条路径：

1. 由 `content_index`（重新复制）或 `entry_id`（回放）定位 `entry_id`。
2. `seq_index[entry_id]` 取旧 seq。
3. 同一写事务内：删 `usage_order[old_seq]`、删 `seq_index[entry_id]`、
   取 `next_seq`、写 `usage_order[new_seq] = {id, now}`、写 `seq_index[id] = new_seq`。
4. `entries` / `texts` / `content_index` 完全不动。

`replay_reorders == false` 时直接返回 `Unchanged`，不消耗 seq。

### 5.4 自动清理

| 配置项 | 默认 | 说明 |
|---|---|---|
| 非固定条目上限 | 500 | 可配 100–5000 |
| 时间清理 | 关闭 / 7 天 / 30 天 | 与上限同时生效，先到先删 |
| 触发时机 | 每次写入新条目、取消固定、收紧配置后，**同一写事务内** | 无额外定时线程 |
| 清理对象 | `seq` 最小且未固定的条目 | 即最久未使用 |
| 固定条目 | 豁免，且**不占配额** | 保证循环终止 |

```
while non_pinned_count > limit
   || (retention 且 oldest_unpinned.last_used_at < now - retention):
    seq = 从 cleanup_cursor 起扫描，跳过 pinned
    if 找不到: break
    删 usage_order[seq] / seq_index[id] / entries[id] / texts[id] / content_index[hash]
    blob 待删队列.push(hash)
    cleanup_cursor = seq + 1
    non_pinned_count -= 1
提交
提交成功后：删除待删 blob 文件 → 发 EntriesRemoved
```

- `non_pinned_count` 让"是否超限"是 O(1)，`cleanup_cursor` 让扫描摊还 O(1)。
  两者缺一，清理就会退化成每次插入 O(n)。
- **时间清理不需要全表扫描**：因为 I5（seq 与时间同向单调），过期条目必然在
  前缀上，从游标开始扫即可。
- 取消固定会让计数上升，可能立刻把刚取消固定的那条清理掉。UI 需要提示，
  或者给"刚取消固定"的条目一个短豁免。
- **blob 删除放在事务提交之后**。提交后、删文件前崩溃会留下孤儿 blob，
  不影响正确性，启动时扫一遍兜底。

### 5.5 内存索引与事件

UI 主线程维护最近 N 条轻量元数据（`entry_id` + `preview` + `preview_fold` +
`kind` + `source_app` + `pinned`）。N 独立配置（默认 500），**与清理上限无关**：
上限设成 5000 时，列表只覆盖最近 500 条，UI 必须明确显示"仅显示最近 N 条"，
否则用户会以为历史丢了。

事件：

- `Added(Vec<EntryView>)`
- `Moved { id, seq, last_used_at }`
- `Removed(Vec<EntryId>)`
- `ConfigChanged(Config)`
- `Warning(String)`

规则：

1. **UI 不自行查库确认**——不发点查，只发分页和搜索。
2. **搜索结果只回 `entry_id`**，UI 用自己的索引解析，未知 id 直接丢弃。
   否则一次晚到的搜索会把已删除条目重新渲染出来。
3. 事件按 50 ms 合批再触发重绘，避免脚本连续复制时刷屏。

---

## 6. 搜索

1. **即时层**：内存索引上做大小写不敏感包含匹配，零延迟。
   索引里预存 `preview_fold`（已小写化），避免每次按键重复分配。
2. **兜底层**：输入停顿 200 ms 后发 `Search(query, limit)` 给存储线程，
   按 `usage_order` 从新到旧扫 `entries.preview`，凑够 `limit` 即停。

明确取舍：**不做全量倒排索引**，换取零索引维护成本。

### 6.1 两遍扫描（已实现）

预览只有前 500 字符，正文里才有的内容原本搜不到。现在 `search` 分两遍：

1. **预览遍**：扫 `entries.preview`，命中即可返回。绝大多数查询在这里就结束。
2. **正文遍**：仅在结果不足 `limit` 时执行，解码 `texts` 里的 `StoredBody` 再匹配。
   设了 `MAX_BODY_SCAN = 2000` 条上限——解码整段文本比匹配预览贵一个数量级，
   没有上限的话一次冷搜索会把存储线程占住。

排序规则：**预览命中一律排在正文命中之前**，即使正文命中那条更新。理由是预览命中
的相关性更高（用户看到的就是预览）。

配套的 UI 坑：列表合并搜索结果时会再按"预览包含关键词"过滤一遍，正文命中会被
全部丢掉。所以 UI 侧对 `search_extra` 只做类型/来源过滤，不再过滤关键词。

---

## 7. 窗口行为（eframe / egui）

### 7.1 框架选型已完成

阶段 0 的双框架实测见 §14。结论：GPUI 0.2.2 与 eframe/egui 0.37 的静态内存
都在 80–92 MB，但 GPUI 系（含 `gpui-ce`、`gpui-base` 两个社区分支）的公开 API
都没有隐藏窗口，而剪贴板弹窗必须失焦即消失。因此选 **eframe 0.37（glow 后端）**。

"静默内存 < 15 MB"这条预算作废，改为可验证的四条（拆进程后静默实测 13–17 MB，见 §2.2）：

- 无 WebView
- 冷启动 < 1 s
- 安装包 < 15 MB
- 空窗口 RSS < 100 MB

### 7.2 窗口行为清单

- **失焦即退出**：`ctx.input(|i| i.focused)` 为 false 且已过启动稳定期时，
  发 `ViewportCommand::Close` 关掉整个 UI 进程。拆分之后"隐藏"只能是退出——
  留着窗口就还是那一百多 MB。下一次唤起由守护进程重新拉起 UI（约 0.7 s），
  首屏从守护进程取，用户看到的是同样的列表。
- **不进任务栏**：`ViewportBuilder::with_taskbar(false)`。
- **多显示器**：出现在光标所在屏幕；位置支持居中 / 跟随鼠标 / 记住。
- **拖动**：无边框窗口，整窗可拖（见 §15「整窗拖动」）。
- **虚拟列表**：只渲染可视区，配合 `Page::total_visible` 估算高度。
- **纯文本粘贴（P1）**：回放时丢弃富文本格式，只写纯文本。依赖 §5.2。
- **点条目直接粘贴（已实现，Windows）**：回放写完剪贴板、隐藏窗口之后，
  把焦点还给唤起前的窗口，再补一次 Ctrl+V。目标 HWND 在 `show()` 里用
  `GetForegroundWindow()` 记下——只在窗口原本是隐藏状态时记，否则记到的是自己。
  顺序不能反：先 `SetForegroundWindow` 再敲键。窗口刚 `Visible(false)` 时进程
  还握着前台权，这时候切换是允许的；晚一步系统把前台判给别人，就会被前台锁挡掉。
  等待放在后台线程（80ms 等窗口藏下去 + 40ms 等焦点回落），不卡渲染。
  **钉住窗口时**焦点还在自己身上，直接敲键只会粘回自己：先把焦点借给目标窗口，
  粘完再 `SetForegroundWindow` 还回来（窗口是 always-on-top，视觉上不会闪）。
  敲键前要确认焦点真的搬过去了（`GetForegroundWindow() == 目标`），否则整段跳过——
  宁可什么都不粘，也不能把内容粘进自己的搜索框。设置里也能整体关掉
  （`paste_on_click`），关掉就退回"只写剪贴板"。
  **坑**：`Config` 整体标了 `#[serde(default)]`，缺字段时用的是**字段类型**的默认值
  （bool 就是 false），所以新加的布尔开关必须写 `#[serde(default = "...")]`，
  否则老配置读进来这个功能是关的。
  其它平台仍需合成输入：macOS 要辅助功能授权、Wayland 基本不可行。

### 7.3 Windows 前台锁

全局热键唤起的进程通常不是前台进程，`SetForegroundWindow` 会被系统拒绝，
窗口只在任务栏闪烁。需要 `AttachThreadInput` 或 `AllowSetForegroundWindow`
绕过，并在隐藏前记录目标 HWND、回放后 `SetForegroundWindow` 还回去。
winit（eframe 的窗口层）提供 `with_always_on_top`。实测结论：**唤起这一侧确实
抢不到前台**（热键唤起的窗口只在任务栏闪），但粘贴那一侧不需要额外技巧——
弹窗当时就是前台进程，隐藏后立刻 `SetForegroundWindow(目标)` 是允许的，
所以没上 `AttachThreadInput`。

### 7.4 外观

- **主题**：`ctx.set_theme(ThemePreference)`，跟随系统 / 明亮 / 黑暗。
- **主题色**：写进`Visuals` 的 `selection.bg_fill` / `widgets.active.bg_fill` /
  `hyperlink_color`，通过 `style_mut_of(theme, ..)` 对明亮与黑暗两套样式**同时**生效，
  避免切主题后颜色不一致。
- **字号**：0.9 / 1.0 / 1.15 三档，以 `Style::default().text_styles` 为基准乘系数，
  而不是在当前样式上叠乘——否则连续点两次"大"会变成 1.15²。
- **必须重写 `clear_color`**：eframe 的默认实现返回写死的深灰 `(12,12,12,180)`，
  与主题无关。不改的话切到明亮主题只有文字变深、窗口底色仍是深灰，看起来就是"没生效"。
  正确做法是返回 `visuals.panel_fill`。

设置面板的布局约定：**顶栏固定**（标题 + 返回）、**分页签**（外观 / 行为 / 容量 / 唤起）、
**中间可滚动**、**底栏固定**（清空历史 / 退出）。小窗口下如果整页一起滚，"退出"会永远在屏幕外。
分页签之后每个分页内容都短，不需要再靠滚动找设置；标签列用 `Grid` 对齐。

### 7.5 必须有的退出入口

MVP 不做托盘，但弹窗内必须有：设置（打开设置窗口）、清空历史、退出。
否则用户只能去任务管理器。快捷键注册失败时也要在弹窗里显示可见告警，
而不是静默失效。

---

## 8. 平台适配矩阵

| 能力 | Windows | macOS | Linux X11 | Linux Wayland |
|---|---|---|---|---|
| 监听 + 读写 | clipboard-rs | clipboard-rs（轮询 changeCount） | clipboard-rs | clipboard-rs（wayland feature，wl-clipboard-rs） |
| 文件列表 | 支持 | 支持 | `text/uri-list` + GNOME fallback | `text/uri-list` |
| 回声抑制 | 序列号（精确） | changeCount（精确） | 哈希窗口 | 哈希窗口 |
| 全局快捷键 | global-hotkey | global-hotkey（主线程） | global-hotkey（X11） | XDG GlobalShortcuts portal，覆盖不全 |
| 快捷键降级 | — | — | — | 旧合成器 `GDK_BACKEND=x11` 或用户自行绑定 |
| 来源应用 | `GetClipboardOwner` → PID → exe（可靠） | **无写入者信息，只能用变更时的前台应用做启发式** | `_NET_WM_PID`（不可靠） | **不可用** |
| 托盘 | 社区分支 / 暂缓 | 社区分支 / 暂缓 | DBus/SNI | DBus/SNI |

**修正**：v1 写 macOS 用 `NSWorkspace` 拿来源应用是不成立的——`NSWorkspace`
给的是前台应用，不是剪贴板写入者。排除应用因此降级为：Windows 可靠、
macOS 启发式、Wayland 不可用。设置页必须如实标注，不能让用户以为配了就一定生效。

**依赖风险**：v1 里的 `clipboard-rs 0.3.4` Wayland 后端、Wayclip 的
`global-hotkey` 分支都是 GitHub 上的非标准依赖 `[待验证]`。依赖 fork 意味着
供应链与维护风险，建议：

- 平台层全部收敛到 `erwuzai-platform` 的 trait 后面（`ClipboardBackend` /
  `HotkeyBackend` / `TrayBackend`），主 crate 不直接 `use` 第三方实现。
- Linux 首版允许"没有全局快捷键"：托盘菜单或命令行 `25zai show` 作为入口，
  比依赖一个未验证的 portal 实现更诚实。

---

## 9. 安全与隐私

| 机制 | 实现 |
|---|---|
| 敏感检测 | 存储线程内、入库之前调用 `secretshape::is_probably_secret()`，仅文本。**已接入生产**（`SecretshapeDetector`，`default-features = false` 只取布尔层，不带 regex） |
| 默认行为 | **默认跳过采集**（`skip_sensitive = true`）；"脱敏后保留"是显式选项 |
| 排除应用 | 见 §8，平台能力不同，需在 UI 标注 |
| 脱敏预览 | 敏感条目在写入时即脱敏，库里不留原文预览 |
| 本地存储 | 数据目录权限 0700（Unix 需显式 chmod），Windows 用 %APPDATA% 默认 ACL |
| 无网络 | 默认不上传任何数据 |

**诚实说明**：不做全库加密时，`texts` 里的明文就是明文。脱敏预览只防窥屏，
不防磁盘取证。这一点应该写进设置页，而不是让用户误以为"标了敏感就安全"。

`secretshape` 是形状检测：高熵字符串、常见 token 前缀会命中，长口令短语不会。
它是启发式，不是保证。

---

## 10. 性能预算

| 指标 | 目标 | 手段 |
|---|---|---|
| 静默内存 | **< 20 MB** | 只有守护进程；UI 隐藏即退出（原 100 MB 预算见下方修正） |
| 唤起 UI | < 1 s | 起一个新进程 + 从守护进程取首屏 |
| 首屏打开 | < 50 ms | 单读事务 + `range(..).rev().take(20)` |
| 回放 / 重新复制 | < 2 ms | 只动两行小记录 |
| 单次写入 | < 5 ms | 5 棵 B-tree 写事务，`Durability::Eventual` |
| 清理 | 摊还 O(1) | 计数 + 游标 |
| 搜索即时反馈 | < 10 ms | 内存索引 |
| 冷启动 | < 200 ms | 无 WebView |

**修正一**：v1 的"静默 < 15 MB"已被实测证伪。任何 GPU 加速的 UI 框架在 Windows 上
空窗口就要 80–92 MB（§14）。现在的预算是 **< 100 MB**，其中 redb 的
`cache_size` 仍设 8 MB——数据库不是内存大头，UI 框架才是。

**修正二**："单次写入 < 5 ms"与 `Durability::Immediate` 冲突。Windows 上有
Defender 时一次 fsync 常常 1–5 ms。默认用 `Durability::Eventual`，
`Immediate` 作为"宁可慢也不丢"的选项暴露给用户。

**修正三**：v1 引用的 redb 4.1 "写入 1.5 倍 / 提交 2 倍"是上游基准 `[待验证]`，
不构成对剪贴板负载的承诺。

---

## 11. 单实例与启动顺序

```
1. 抢 instance.lock（`std::fs::File::try_lock`：Windows `LockFileEx` / Unix `flock`）
   └─ 失败 → 读 `instance.port`，连回环 TCP 发 "show" 给首实例 → 退出（码 0）
2. 打开 redb（处理 UpgradeRequired / AlreadyOpen）
3. 读 config，校验 schema_version
4. 后台线程扫孤儿 blob
5. 启动监听线程
6. 注册全局快捷键
   └─ 失败 → 记 Warning，弹窗内可见告警 + 引导改键
7. 常驻（守护进程无窗口；UI 进程按需拉起、隐藏即退出）
```

顺序不能换：实例锁必须在打开数据库之前，否则第二个实例会先撞上 redb 的
文件锁，报出来的错和"数据库损坏"难以区分。

**第二实例不运行任何业务逻辑**：它不开数据库、不起监听线程、不注册热键，
只把 5 字节的 "show" 发给首实例然后退出（生命周期约 20 ms）。首实例在
`127.0.0.1:0`（系统分配端口）上监听，端口写进 `instance.port`。
选回环 TCP 而不是命名管道 / Unix socket：那两者平台接口不同，而这里只传一个字符串。

锁用操作系统文件锁而不是"建锁文件 + 退出时删除"：后者在进程被强杀时会留下
死锁文件，下次启动永远进不去，只能让用户手工删文件。文件锁随句柄释放，
不存在这个问题。

---

## 12. 实施路线图

**阶段 0：框架 spike（P0，先做）**
已完成。GPUI 与 egui 双框架实测，GPUI 因缺少隐藏窗口 API 且内存无优势被排除，
详见 §14。
同时验证 `clipboard-rs` 的文本 / 图片 / 文件列表读写与 `global-hotkey` 注册。

**阶段 1：Windows 垂直切片（P0）**
`ClipboardStore` 的 redb 实现 + 六张表 + 去重 + 分页 + 回放移动 + 清理
（计数 + 游标）+ 回声抑制 + 单实例 + eframe 弹窗（搜索 + 虚拟列表）+ 设置/退出入口。
闭环验收：复制 → 呼出 → 回放后排第一 → 超限自动清理最旧非固定条目 →
回放不产生重复历史。

**阶段 2：macOS（P0）**
复用存储与 UI，验证监听、图片、文件列表、changeCount 回声抑制、快捷键主线程初始化、
失焦隐藏后的焦点恢复。

**阶段 3：图片与安全（P1）**
blob 外置 + 缩略图后台解码与磁盘缓存（不要在 UI 线程解码 4K 图）+ secretshape +
脱敏 + 纯文本粘贴 + 类型过滤 + 键盘导航。

**阶段 4：Linux（P1）**
X11 先行，Wayland 作为独立验证项。快捷键允许缺失，用托盘或 CLI 兜底。

**阶段 5：体验完善（P2）**
托盘、备注分组、粘贴栈、Snippets、导入导出、自动粘贴。

### 测试（P0 交付物，不是后补）

1. **模型化属性测试**：用 `Vec<EntryRecord>` 做参考模型，随机生成
   capture / touch / pin / unpin / delete / update_config 序列，逐步对拍
   全部查询结果与 I1–I6。
2. **崩溃一致性**：在"事务提交后、blob 删除前"注入 panic，重启后验证
   库可用且孤儿 blob 被回收。
3. **迁移测试**：用旧 schema 版本的文件启动，验证拒绝 / 备份 / 迁移路径。
4. **回声测试**：模拟"写入 → 立即读回"，断言不产生新条目。

---

## 13. 风险与对策

| 风险 | 对策 |
|---|---|
| UI 框架内存不达标（已发生） | 阶段 0 实测后改用 eframe/egui，预算从 15 MB 调整为 100 MB |
| redb 磁盘格式升级 | 4.x 基线；捕获 `UpgradeRequired`，备份后建新库，不静默失败 |
| 回放回声污染历史 | §5.2 抑制层，MVP 必做 |
| 双实例互相回声 | §11 实例锁 + IPC 转发 |
| 清理循环不终止 | 固定条目不占配额；计数严格递减；无受害者即 break |
| 清理退化 O(n) | `non_pinned_count` + `cleanup_cursor` |
| blob 孤儿 | 删除在提交后；启动扫描兜底 |
| macOS/Wayland 拿不到来源应用 | 矩阵降级，UI 如实标注 |
| 依赖 fork 的供应链风险 | 平台层收敛到自有 trait，主 crate 不直接依赖 fork |
| COW 库文件不缩 | 单条上限 + 显示库大小 + 定期 compact |
| 数据库文件锁与实例锁混淆 | 启动顺序固定：先实例锁后开库 |

---

## 14. 阶段 0 实测结果（2026-10-18，Windows 11）

GPUI 0.2.2（crates.io，非 git 依赖），release 构建，`crates/erwuzai-gpui/src/bin/spike.rs`
与 `spike_idle.rs` 可复现。

| 指标 | 目标 | 实测 | 结论 |
|---|---|---|---|
| 空窗口 RSS | < 10 MB | **81.5 MB** | 差 8 倍 |
| 无窗口事件循环 RSS | — | **51.3 MB** | 框架底噪本身已超预算 |
| 私有字节（有窗口） | — | 69.6 MB | — |
| 窗口创建耗时 | < 300 ms | **588 ms** | 差 2 倍 |
| 可执行文件 | — | 10.3 MB | — |
| 失焦隐藏 | 隐藏而非销毁 | **公开 API 无 `hide_window`** | 不达标 |

补充数据：

- debug 构建：RSS 77.5 MB、窗口创建 954 ms。release 只改善了时间，内存几乎没动。
- 内存读数在 1 s / 3 s / 6 s 三次采样稳定在 75→81.5 MB，不是启动抖动。
- 不建窗口的对照组仍有 51.3 MB，说明开销来自 GPUI 运行时（font-kit 枚举系统字体、
  blade、image/resvg 等），不是 GPU 交换链。

**失焦隐藏**：`gpui::Window` 只暴露 `minimize_window()` / `activate_window()`，
`platform_window` 字段是 `pub(crate)`，外部 crate 拿不到 `PlatformWindow::hide()`。
所以"隐藏而非销毁"在 0.2.2 上无法用公开 API 实现，只能降级成最小化，
或者每次呼出重建窗口。

### 14.1 对照：eframe / egui 0.37（glow 后端）

同样的 480×320 空窗口，`crates/egui-probe`，release 构建，20 秒采样已稳定。

| 指标 | GPUI 0.2.2 | eframe 0.37 (glow) |
|---|---|---|
| 空窗口 RSS | 81.5 MB | 91.6 MB |
| 私有字节 | 69.6 MB | 89.1 MB |
| 无窗口底噪 | 51.3 MB | 不适用 |
| 窗口创建 | 588 ms | 714 ms |
| 可执行文件 | 10.3 MB | 5.6 MB |
| 隐藏窗口 | **无公开 API** | 有（隐藏时不跑 egui pass） |
| API 稳定性 | 0.2.x，频繁破坏 | 0.37，成熟稳定 |

两点说明：

- eframe 默认 feature 里的 `wgpu` 在 Rust 1.99 下编译失败（wgpu-hal 报 10 个错误），
  必须 `default-features = false, features = ["glow", "default_fonts"]`。
- egui 首次采样到 110 MB 是启动期峰值，稳定值 91.6 MB。

### 14.2 结论

**两个 GPU 加速框架都在 80–92 MB 量级，没有任何一个能接近 15–25 MB 的预算。**
方案里"静默约 5 MB"的社区数据与本机实测差一个数量级，不应再作为设计依据。

这意味着"Featherweight"只有两条路：

1. **放弃绝对内存指标**：把定位改成"无 WebView、冷启动 < 1 s、安装包 < 15 MB"。
   在这条路上 egui 优于 GPUI——隐藏窗口有原生支持，API 稳定，生态成熟。
2. **守住 < 15 MB**：只能用原生窗口（Windows 上 Win32 + Direct2D），
   代价是每个平台各写一套 UI。

**决策（2026-10-18）**：选路线 1，用 **eframe 0.37（glow）**。理由：

- 隐藏窗口是剪贴板弹窗的硬需求，GPUI 系全部缺失；
- 两者内存只差 10 MB（81.5 vs 91.6），不值得为此放弃功能；
- egui 0.37 的 API 稳定，eframe 隐藏时完全不跑 UI pass。

已落地：`crates/erwuzai-ui` 用 eframe 起窗口，接 redb 读出历史，
搜索框 + `ScrollArea::show_rows` 虚拟列表 + 上下键选择 + Esc 隐藏。
失焦隐藏的代码已就位，默认由 `25ZAI_HIDE_ON_BLUR` 控制——全局热键接上之前
不能默认开启，否则窗口藏起来就再也叫不出来。

---

## 15. 当前实现状态（2026-10-18）

四个 crate 全部编译通过，`cargo test` 30 项全绿，clippy 无警告。

| crate | 内容 |
|---|---|
| `erwuzai-core` | `Paths` 数据目录解析、`SingleInstance` 文件锁 |
| `erwuzai-store` | `RedbStore`（七表、去重、分页、清理、blob）、服务线程、13 个集成测试 |
| `erwuzai-platform` | `ClipboardBackend` / `HotkeyBackend` / `EchoGuard` / `SystemClipboard` / `SystemHotkey` |
| `erwuzai-ui` | 入口与两个角色：`daemon`（常驻，存储 / 监听 / 热键 / 托盘）与 UI（eframe 弹窗，隐藏即退出） |

已实测跑通的闭环：

```
复制 3 条文本 → 监听线程捕获 → 存储线程去重入库
→ 列表按 C/B/A 倒序显示 → 全局热键 Alt+V 唤起
→ Enter 回放：读内容 → 写剪贴板 → arm EchoGuard → touch 置顶 → 隐藏
```

平台层已补齐（Windows）：

- **`sequence()`** 走 `GetClipboardSequenceNumber`，回声判定从"哈希兜底"升级为
  "序列号精确匹配"。返回 0 视为失败并退回哈希——不能让 `Some(0)` 参与比较，
  否则两边都是 0 会被误判成同一次写入。
- **`source_app`** 走 `GetClipboardOwner` → `GetWindowThreadProcessId` →
  `OpenProcess` + `QueryFullProcessImageNameW`。实测能正确识别 `powershell.exe`。

**踩到的坑**：不要用 `Clipboard::get(&[...])` 一次性取全部格式。clipboard-rs 0.3.5
的 Windows 实现里，`get` 自己先 `OpenClipboard`，再对每个格式调 `get_files` /
`get_image` / `get_text`，那些函数会**再次**打开剪贴板；嵌套打开全部失败，
函数返回空数组而不是报错。表现是"监听有事件但一条都采集不到"，很难定位。
必须逐格式读取。

**图片路径已实测**（2026-10-18）：

| 用例 | 结果 |
|---|---|
| 1×1 PNG | 入库 `IMG`，blob 73 B |
| 3840×2400 JPEG（1.6 MB） | 入库 `IMG`，blob 6.7 MB，release 下从写剪贴板到落盘 **0.70 s** |
| 相同字节重复采集 | 去重为 `Moved`，不新增条目 |
| 删除条目 | blob 文件同时删除 |
| 孤儿 blob / `.tmp` 残留 | 启动扫描回收 |

两个要注意的点：

1. **图片在监听线程上重编码**。`read_from` 调 `to_png()`，4K 图 release 0.70 s、
   debug 约 12 s。这期间监听线程阻塞、剪贴板被反复打开，后续复制会漏采。
   优化方向：优先取剪贴板里的原始 `PNG` 格式（`get_buffer("PNG")`），
   没有才退回 DIB → PNG 转换；或者把编码挪到存储线程。
2. **clipboard-rs 的 `set_image` 对 1×1 图报 `Invalid bitmap data`**
   （DIBV5 转换的边界情况），尽管 DIB 实际已写入。极小图片回放可能失败。

**MVP 收口（2026-10-18）**：

- **热键失败保护**：`hide_on_blur` 只在热键注册成功时开启，另加 500 ms 启动宽限。
  热键注册失败时窗口保持可见并显示错误，不会变成叫不出来的隐身进程。
- **设置面板**：内嵌在弹窗内（`设置` 按钮），可调条数上限、时间清理、
  回放置顶、跳过密钥、排除应用，以及清空历史与退出。改动即时提交 `update_config`。
- **全量搜索兜底**：输入停顿 200 ms 且内存命中不足 50 条时，发 `StoreCommand::search`；
  回应带 generation，过期结果直接丢弃。
- **启动孤儿扫描**：`sweep_orphan_blobs` 在打开库之后调用。
- **文件列表采集已实测**：`Set-Clipboard -Path` 单文件与多文件都正确入库为 `FILE`，
  预览是"前 3 个文件名 + 总数"。
- **钉住窗口**：顶栏「搜索框 / 图钉 / 设置」三个元素，中间的 28×28 图钉是
  **窗口**开关（不是条目固定）：钉住后**任何情况都不关闭**——失焦、点条目回放、
  按 Esc 全部无效，`hide()` 直接返回。要关先点图钉解除。方便对着别的窗口一条条往回粘。
  钉住态用主题色底 + 白色针，和默认样式的「设置」一眼能分开。
  状态只在本次唤出内有效——`show()` 里重置，避免下次唤起莫名常驻。
  图钉自绘：egui 没有图标字体，中文界面里写「钉」字又占宽。
- **钉住条目**：每行右端 18×18 的五角星按钮（★ 实心 = 已钉住，☆ 只在悬停 / 选中行里淡显）。
  用星而不是图钉，是为了和顶栏那个「钉窗口」区分开。点它只切换 pinned，不会回放内容
  （行的点击区缩到星标左边）；右键切换保留为快捷方式。切换后本地立刻改
  `entry.pinned` 并按「固定在前 + seq 倒序」重排，选中项跟着同一条走——
  存储层只写库不回推事件，不本地改的话会看起来"根本没生效"。
- **隐藏后仍能唤醒**：窗口一藏，eframe 就退到 `ControlFlow::Wait`——没有重绘请求
  就不再跑帧，而热键 / 托盘 / 第二实例全是**通过通道**送进来的，不跑帧等于永远收不到。
  症状是「点完条目窗口关掉，再按快捷键叫不回来」（实测隐藏时 5 秒只烧 15 ms CPU，
  说明确实停在 Wait 上）。修法：隐藏期间每 120 ms 挂一次 `request_repaint_after`，
  并且那一帧只轮询、直接 `return`，不再排版 500 行列表。代价是隐藏时约 4% 单核占用。
- **整窗拖动**：窗口无边框，在**所有控件之前**注册一个铺满窗口的 `Sense::drag()` 区域。
  egui 的命中测试只认最上层能响应同类交互的控件，所以输入框、按钮、列表行照常点击，
  拖动落在它们之外的空白处（含内边距、分隔线、底部统计）时才移动窗口；
  列表行只 `Sense::click()`，因此从行上拖也能移窗。`drag_started` 时发
  `ViewportCommand::StartDrag`，交给 winit 走系统的 `WM_NCLBUTTONDOWN`。
- **固定条目置顶**：新增 `pinned` 表（存 entry_id 而非 seq——`touch` 会换 seq），
  `page()` 先输出固定条目再拼时间流。否则"固定"只是不被清理，照样淹没在历史里。
- **敏感检测换成 secretshape**：替掉存储层里那个占位启发式。
- **外观设置**：主题（跟随系统/明亮/黑暗）、主题色、字号三档。修掉了 eframe
  `clear_color` 默认硬编码深灰导致"切明亮无效"的问题。
- **中文字形**：egui 的 `default_fonts` 不含 CJK，启动时挂系统字体
  （Windows 依次试 `simhei.ttf` / `msyh.ttc` / `simsun.ttc`，macOS/Linux 另有候选），
  作为 fallback 追加在默认字体之后。代价是常驻内存 +10 MB（字体文件本身）。
- **失焦判定加焦点闩**：`Visible(true)` / `Focus` 是帧末生效的，唤出后一两帧
  `focused` 仍是 `false`，直接判定会闪一下。改为"本次唤出后必须先见过焦点，
  之后失去焦点才隐藏"。副作用是窗口若从未拿到焦点则不会自动隐藏——这是更安全的一侧。

**P1 进展（2026-10-18）**：

- **图片快路径**：先试 `get_buffer("PNG")`，源应用直接提供 PNG 时零解码、零重编码；
  只有 DIB/DIBV5 才走 `get_image`。
- **"把 to_png 挪到存储线程"这个原计划被实测否决**：3840×2400 下
  `get_image` 253 ms、`to_png` 84 ms，编码只占四分之一。真问题是解码，
  所以优化点是绕开它而不是搬家。实测 PNG 路径 334 ms vs DIB 路径 577 ms
  （含进程启动开销）。
- **纯文本粘贴**：设置里有"回放只保留纯文本"开关，`Shift+Enter` 可临时覆盖。
  注意哈希必须在降级**之后**计算，否则回声守卫匹配不上。
- **类型 / 来源过滤**：搜索框下方一排类型芯片（全部 / 文本 / 富文本 / 图片 / 文件），
  右侧是来源应用下拉（从当前列表里实际出现过的 `source_app` 生成，不是全量配置）。
  过滤在 `passes()` 里统一实现，内存命中计数、全量搜索的外挂结果都走同一条判定，
  避免"列表过滤了但搜索仍返回"的不一致。
- **系统托盘**：`tray-icon` 0.26 + `muda` 0.21（Tauri 生态）。默认图标是程序生成的
  32×32 RGBA（蓝底圆角 + 白色内页），不依赖外部资源文件。菜单：显示历史 / 设置 / 退出 25仔；
  左键单击图标直接唤出。托盘与热键任一可用即可开启失焦隐藏——两者都失败时窗口保持常驻，
  避免变成叫不回来的隐身进程。
  右键菜单是 `TrackPopupMenu` 画的 Win32 原生菜单，只跟随**系统**主题，不会跟着
  应用自己的深色走。适配靠 uxtheme 的两个未公开序号导出：135 `SetPreferredAppMode`
  （2 强制深色 / 3 强制浅色）、136 `FlushMenuThemes`（已建好的菜单不会自己重画）。
  注意 uxtheme **不一定已经加载**——用 `GetModuleHandleW` 取不到时必须
  `LoadLibraryW` 补上，否则整个调用是空转。主题每帧比对一次，变了才去调。
- **快捷键可配置（按键捕获）**：设置里点按钮进入录键模式，直接按组合键即可，
  不需要手写字符串。要点：Ctrl / Shift / Alt **本身也会作为 `Event::Key` 出现**
  （`ControlLeft`、`ShiftRight`…），录键时必须跳过，否则按下 Ctrl 的瞬间就被
  当成"用户选了 Ctrl"。`Esc` 取消；没有修饰键的组合会被拒绝。
  改键走 `SystemHotkey::rebind`——先注销旧组合再注册新的，**失败时把旧的装回去**，
  不会出现"改失败以后一个快捷键都用不了"的死局。
- **弱化文字按主题定值**：egui 0.37 的 `weak_text_alpha = 0.6` 是在正文色上乘透明度，
  深色主题算出来接近 `#3F3F3F`，压在 `#1B1B1B` 的面板上几乎看不见。改为直接给
  `weak_text_color` 定值（深色 `gray(185)` / 浅色 `gray(95)`），正文提到
  `gray(238)`（浅色压到 `gray(24)`）。
  **文字一律中性，不掺主题色**——彩色正文看久了发脏，层级靠字号和亮度区分就够了。
  只有选中行切到 `strong_text_color()`，否则压在主题色底上会完全看不见。
- **列表行不再显示时间和类型标签**：左侧那列（时间 + 「文本 / 富文本 / 图片 / 文件」
  色块）整块去掉了，正文从行首开始排。时间对剪贴板历史几乎没有信息量——用户找的是
  "刚才复制的那条"，顺序本身就是时间；类型标签同理，图片和文件在预览里已经一眼能认出来。
  去掉之后正文多出 54px 宽，行高也回到单行 ~22px。
- **图片行左侧带缩略图**：40×40 方框，等比放大后**居中裁切**（cover）。按比例缩进去
  （contain）的话长截图会变成一条几像素高的细线，看不出是什么。纹理先缩到 96px 再上传，
  4K 截图也只占几十 KB 显存；按 entry id 缓存，读不出来记 `None` 不重试。
  解码放在 UI 线程，**一帧最多一张**，没轮到的 `request_repaint()` 要下一帧——
  egui 不会自己连续重绘，不主动要帧的话整屏只会加载出一张。
  坑：`image::open` 在 0.25 里是**按扩展名**猜格式的，而 blob 文件名是哈希、没有扩展名，
  会直接报 `The image format could not be determined`。必须
  `ImageReader::open(path)?.with_guessed_format()?.decode()` 按内容嗅探。
- **每行一张淡底色卡片 + 6px 行距**：紧挨着时整列文字糊成一片，分不清一条到哪结束；
  底色只比面板亮一档（深色 38 / 浅色 238，面板是 27 / 248），悬停再提亮、选中用主题色。
  不画描边——几百行每条都框一圈，看着比内容还吵（参考 Win+V）。
- **设置分页签**：外观 / 行为 / 容量 / 唤起 / 关于五页，顶部一排 64×26 的按钮，
  选中态用 `Button::selected`。分页只是视图状态，不落盘。「关于」页显示内嵌的 256px 图标
  （`assets/about.png`，由 `--pack-image` 从原图生成）、版本号、快捷键、条目数和数据说明。
- **窗口背景是纯色，没有毛玻璃**：egui / eframe 本身不提供这个能力——
  `Frame` 的 `fill` 最多做到半透明，取背后内容再模糊要自己做。
  Windows 那条路也走不通：`DWMWA_SYSTEMBACKDROP_TYPE` 采样的是壁纸，
  不会把别的窗口内容透出来；`SetWindowCompositionAttribute` 在 22H2+ 上
  退化成了纯色，实测两者都是"返回成功但没有效果"。
  曾经自己截屏 + 盒式模糊做过一版，能出效果，但底色一暗（深色壁纸、
  深色编辑器）就整块糊成面板色，观感和"没开"一样，还要维护 DPI、
  拖动取样、GDI alpha 一堆边界情况。收益不值这个复杂度，已经整块删掉：
  `Config` 里的 `frosted_glass` / `glass_opacity`、设置页开关、
  `erwuzai-platform::backdrop` 模块都不在了，窗口用 `panel_fill` 实色绘制。
  旧配置文件里的这两个字段会被 `serde` 忽略，不影响升级。
- **列表右内边距与滚动条**：外层 `Frame` 只留左/上/下边距，右边留给滚动区——
  悬浮滚动条要贴在窗口右边框上，内容再各自收窄 `EDGE_PAD`（14px）。
  另外预览 `LayoutJob` 必须显式 `wrap_mode(Wrap)`：横向布局里 `ui.wrap_mode()` 是
  `Extend`，`Label` 会把 `max_width` 改成 `INFINITY`，长 URL 会把整行顶穿右边界。
- **自定义托盘图标**：设置里可选本地图片（png/jpg/ico/bmp），用 `image` 解码后
  `thumbnail(32,32)` 再转 RGBA，**不用** `Icon::from_path`——那个只在 Windows 下可用
  且只认 .ico。实测 3024×3706 / 18.9 MB 的 PNG 解码耗时 122 ms。
- **配置改用 JSON 存储**：postcard 不自描述，给 `Config` 加字段会让旧 blob 整块解不出来
  （已经发生过两次，每次都重置用户设置）。改为 `serde_json` + `#[serde(default)]`，
  新增字段向后兼容；解码失败时顺手把默认值写回，避免每次启动重复报警。
- **顺带修掉一个真 bug**：`write_to` 处理富文本时原来只写 `CF_HTML`，没写纯文本。
  结果是从历史里回放一条富文本，粘到记事本这类只认 `CF_UNICODETEXT` 的应用里是空的。
  现在用 `ctx.set(&[Text, Html])` 一次写两种格式（实测 plain 15 字节、html 101 字节）。

仍未完成：

1. **剪贴板争用**。每次采集要打开剪贴板 4–6 次，期间其他应用写入可能失败
   （实测 PowerShell `Set-Clipboard` 约 1/10 概率报 ExternalException）。
2. 类型 / 来源过滤、托盘、图片缩略图 **已完成**；剩数字快速选择（1–9 直接回放）。
3. macOS / Linux 未验证；`source_app` 与 `sequence` 是 Windows 专用。

**2026-10-19 追加**：列表行改成淡底色卡片 + 6px 行距（对齐 Win+V 的信息密度）；
图片行加 40×40 缩略图；点条目直接粘贴，钉住窗口时借焦点再还回去；
托盘右键菜单跟随深色模式；毛玻璃整块删除。测试 43 项全绿。
同日开源到 GitHub（`refout/25zai-clipboard`）并加了 Actions：`ci` 跑
`clippy -D warnings` + 全量测试，`release` 由 tag 触发、先测后打包，
产出带 SHA256 的 zip。

---

## 附录 A：`ClipboardStore` 契约

```rust
pub trait ClipboardStore: Send {
    fn schema_version(&self) -> u32;
    fn capture(&self, item: CapturedItem) -> StoreResult<CaptureOutcome>;
    fn page(&self, cursor: Option<Seq>, limit: usize) -> StoreResult<Page>;
    fn search(&self, query: &str, limit: usize) -> StoreResult<Vec<EntryView>>;
    fn get_content(&self, id: EntryId) -> StoreResult<Option<Content>>;
    fn blob_path(&self, hash: ContentHash) -> Option<PathBuf>;
    fn touch(&self, id: EntryId) -> StoreResult<TouchOutcome>;
    fn set_pinned(&self, id: EntryId, pinned: bool) -> StoreResult<Vec<EntryId>>;
    fn delete(&self, id: EntryId) -> StoreResult<bool>;
    fn clear_unpinned(&self) -> StoreResult<Vec<EntryId>>;
    fn config(&self) -> StoreResult<Config>;
    fn update_config(&self, config: Config) -> StoreResult<Vec<EntryId>>;
    fn stats(&self) -> StoreResult<Stats>;
    fn sweep_orphan_blobs(&self) -> StoreResult<usize>;
}
```

设计要点：

- **全部 `&self`**：redb 事务入口本身就是 `&`，实现只在单一服务线程上被调用，
  不需要 `Sync`。
- **不返回事件**：trait 只回答"做了什么"（`CaptureOutcome.evicted`、
  `set_pinned` 返回被清理的 id）。事件翻译是服务线程的职责，这样存储层
  可以整体替换成内存实现做对拍测试。
- **`touch` 同时服务回放与重新复制**，语义是"移动到最新 seq"，
  由 `Config::replay_reorders` 决定是否短路。
- **`get_content` 与 `blob_path` 分离**：图片只回路径，解码和缩略图
  在 UI 侧后台线程做，存储层不碰像素。
