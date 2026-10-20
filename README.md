# 25仔（25zai）

[![ci](https://github.com/refout/25zai-clipboard/actions/workflows/ci.yml/badge.svg)](https://github.com/refout/25zai-clipboard/actions/workflows/ci.yml)
[![release](https://github.com/refout/25zai-clipboard/actions/workflows/release.yml/badge.svg)](https://github.com/refout/25zai-clipboard/actions/workflows/release.yml)
[![license](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

> Featherweight clipboard memory for every desktop.

一个用 Rust + egui 写的轻量剪贴板历史工具：常驻托盘，`Alt+V` 唤起，点击即粘贴。
没有 Electron、没有后台云同步，数据全部留在本地的一个 redb 文件里。

---

## 功能

- **文本 / 图片 / 文件列表**采集，BLAKE3 连续去重
- **全局快捷键**唤起（默认 `Alt+V`，可在设置里改键，支持按键捕获录键）
- **点条目直接粘贴**回原来的窗口；钉住窗口后可连续粘多条
- **实时搜索**：内存索引先行，命中不足 50 条时再走存储层全量搜索
- **固定条目**：钉住的历史不会被容量上限和时间清理删掉
- **图片缩略图**：图片行左侧 40×40 缩略图，按需解码、按条目缓存
- **明暗主题**：跟随系统 / 强制明 / 强制暗，主题色可调，字号可缩放
- **托盘菜单**：显示 / 设置 / 退出，菜单跟随系统深色模式
- **敏感内容跳过**：形似密钥的内容默认不落盘（secretshape）
- **单实例**：第二次启动会把已有实例唤到前台，然后自己退出

平台：**Windows 11 已实测**；macOS / Linux 代码路径存在但未验证。

## 下载

到 [Releases](https://github.com/refout/25zai-clipboard/releases) 下 `25zai-v*-windows-x64.zip`，
解压后直接运行 `25zai.exe`，免安装、无运行时依赖。压缩包里附 `SHA256SUMS.txt` 校验。

## 从源码构建

需要 Rust 1.89+（edition 2021）。

```bash
git clone https://github.com/refout/25zai-clipboard.git
cd 25zai
cargo build --release -p erwuzai-ui
./target/release/25zai.exe
```

首次启动会创建数据目录并常驻托盘。按 `Alt+V` 唤起列表。

## 使用

| 操作 | 说明 |
|---|---|
| `Alt+V` | 唤起 / 收起列表 |
| 点击条目 | 写回剪贴板并粘贴到之前的窗口 |
| 点击条目的星标 | 固定 / 取消固定（右键条目的某一行也可以切换） |
| `↑` `↓` / `Enter` | 键盘选择与回放 |
| `Shift+Enter` | 只回放纯文本（丢掉 HTML / RTF） |
| 顶栏图钉 | 钉住窗口：失焦不隐藏，方便连续粘贴 |
| `Esc` | 收起 |

设置页分五栏：外观 / 行为 / 容量 / 唤起 / 关于。

## 配置

配置全部存在数据库的 `config` 表里（JSON），没有单独的配置文件。

| 键 | 默认值 | 说明 |
|---|---|---|
| `limit` | `500` | 非固定条目上限，固定条目不占配额 |
| `retention_days` | `null` | 可选的时间清理，和上限同时生效，先到先删 |
| `hotkey` | `Alt+V` | 全局快捷键 |
| `theme` | `system` | `system` / `light` / `dark` |
| `accent` | `[58, 110, 165]` | 主题色 RGB |
| `font_scale` | `1.0` | 字号缩放 0.8–1.6 |
| `popup_position` | `cursor` | `cursor` / `center` / `remembered` |
| `paste_on_click` | `true` | 点条目后补一次 Ctrl+V |
| `replay_reorders` | `true` | 回放后排到第一 |
| `replay_as_plain` | `false` | 回放只写纯文本 |
| `skip_sensitive` | `true` | 疑似密钥直接丢弃 |
| `mask_sensitive_preview` | `true` | 保留时预览脱敏 |
| `excluded_apps` | `[]` | 不采集这些进程的复制内容 |
| `max_text_bytes` | `1048576` | 单条文本上限，超出截断 |
| `tray_icon` | `null` | 自定义托盘图标路径 |

数据目录：Windows `%APPDATA%\25zai\`，macOS `~/Library/Application Support/25zai/`，
Linux `~/.local/share/25zai/`。设置环境变量 `25ZAI_DATA_DIR` 可以整体改到别处（便携模式）。

目录里只有两个东西：`25zai.db`（redb 数据库）和 `blobs/`（图片原始字节，文件名是 BLAKE3 哈希）。

## 架构

```
crates/
  erwuzai-core      数据目录、单实例锁、第二实例转发
  erwuzai-store     redb 七张表、去重、分页、清理、blob、服务线程、进程间协议
  erwuzai-platform  平台边界：剪贴板、全局热键、托盘、菜单主题、合成输入
  erwuzai-ui        两个角色：守护进程（daemon.rs）与 eframe 弹窗（app.rs）
```

运行时是**两个进程**，同一个 exe：

- **守护进程**（无窗口，常驻）：实例锁、数据库、剪贴板监听、全局热键、托盘。
  闲置只占 13 MB 左右。
- **UI 进程**（有窗口，按需）：按 `Alt+V` 或点托盘才拉起，弹窗一藏就退出。
  eframe 的窗口和 GL 上下文创建后收不回去，留着就是一百多 MB。

两者走回环 TCP（127.0.0.1，一行一个 JSON）。所以任务管理器里看到两个
`25zai.exe` 是正常的；退出请在托盘菜单或设置页点「退出」。

两条硬规则：

1. **存储层独占**：所有数据库操作都在一个专用线程上跑，UI 通过通道发命令，
   不碰数据库句柄。这样 redb 的写事务天然串行，也不用担心阻塞渲染。
2. **平台边界**：UI 只依赖 `erwuzai-platform` 暴露的 trait，第三方实现
   （clipboard-rs / global-hotkey / tray-icon）都挡在边界外。

完整设计见 [docs/DESIGN.md](docs/DESIGN.md)：数据模型、生命周期、回声抑制、
平台矩阵、性能预算、踩过的坑都在里面。

## 开发

```bash
cargo test --workspace     # 52 项测试
cargo clippy --workspace --all-targets
```

几个调试命令。守护进程在跑时会自动走回环连接（库锁在它手里），不在就自己开库：

| 命令 | 用途 |
|---|---|
| `25zai --dump` | 打印当前历史，无 GUI 环境下验证采集链路 |
| `25zai --peek` | 打印剪贴板当前内容 |
| `25zai --seed <文本>` | 直接写一条历史，不用监听线程 |
| `25zai --put-image <路径>` | 把图片推上剪贴板，验证图片采集 |
| `25zai --put-rich <文本>` | 以富文本写剪贴板，验证纯文本回放 |
| `25zai --search <关键词>` | 走存储层搜索，验证正文命中 |
| `25zai --set-theme <light\|dark\|system>` | 切换主题 |
| `25zai --set-tray-icon <路径>` | 设置托盘图标（`-` 恢复内置） |

## 发布

打 tag 即触发 [release.yml](.github/workflows/release.yml)：先跑全量测试，再构建
`windows-latest` 版本，打出带校验和的 zip 并挂到 Release 上。

```bash
git tag v0.1.0
git push origin v0.1.0
```

也可以在 Actions 页面手动触发：只产出 artifact，不会建 Release，用来验证打包脚本。

## 已知限制

- 每次采集要打开剪贴板 4–6 次，其他应用在这期间写入可能失败
  （实测 PowerShell `Set-Clipboard` 约 1/10 概率报错）。
- 自动粘贴依赖合成输入，只在 Windows 上实现；macOS 需要辅助功能授权，
  Wayland 基本不可行。
- `source_app` 与剪贴板序列号是 Windows 专用。
- 不加密存储——加密会破坏 redb 的范围扫描和零拷贝路径。

## 许可证

MIT
