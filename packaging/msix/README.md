# Microsoft Store（MSIX）

Win32 程序上架 Store 只能走 MSIX（全信任打包）。`pack.ps1` 把
`target/release/25zai.exe` + 图标资产 + 清单打成一个 `.msix`。

## 先准备

Partner Center 保留应用名后，在「产品管理 → 产品标识」里拿三个值，填到仓库
变量（Settings → Secrets and variables → Actions → Variables）：

| 变量 | 例子 |
|---|---|
| `MSIX_IDENTITY_NAME` | `Refout.25zai` |
| `MSIX_PUBLISHER` | `CN=1A2B3C4D-....` |
| `MSIX_PUBLISHER_DISPLAY_NAME` | `refout` |

## 本地打包

```powershell
cargo build --release -p erwuzai-ui
./packaging/msix/pack.ps1 -IdentityName Refout.25zai -Publisher 'CN=1A2B...' -PublisherDisplayName refout -Version 0.1.2
```

产物在 `dist/25zai-<版本>-windows-x64.msix`。CI 里 `store.yml` 会顺手用自签证书
签名（Store 会用微软证书重签，自签只要 Subject 等于 Publisher 即可）。

## 两个必踩的坑（MakeAppx 报错是本地化的，经常只显示问号）

1. **清单里的中文必须转成数字实体**（`&#x4ED4;` 这种）。直接放 UTF-8 中文，
   MakeAppx 会报 “App manifest XML must be valid” 但原因全是 `?`。
   `pack.ps1` 已经自动转，模板里可以正常写中文。
2. **全信任不能再加 `uap10:RuntimeBehavior="win32App"`**，否则报
   “Application with RuntimeBehavior value win32App must not declare EntryPoint”。
   正确写法是只留 `EntryPoint="Windows.FullTrustApplication"`。

## 上架流程

1. 跑 `store` 工作流（或本地打包），下载 `.msix`
2. Partner Center → 提交 → 上传包
3. 填商店信息：截图（≥1 张，1366×768 以上）、描述、**隐私政策 URL**
4. 年龄分级问卷（IARC）→ 提交认证

## 审核注意

- **剪贴板监听属于敏感能力**，必须有隐私政策，写明「数据只存本机、不上传」。
- `runFullTrust` 会要求填用途，照实写「剪贴板历史管理器」。
- **开机自启**走清单里的 `desktop:StartupTask`（TaskId=`25zaiDaemon`），
  运行时由 `erwuzai-platform` 调 WinRT `StartupTask` 开关；它会出现在
  「任务管理器 → 启动」里，用户可关，符合商店对后台自启的要求。
- **数据目录**：全信任 MSIX 会把 `%APPDATA%` 写入重定向到
  `%LOCALAPPDATA%Packages\<包族名>\LocalCache\Roaming\25zai`，所以 Store 版
  读不到便携版的旧历史（这是隔离，不是 bug）。

## 已知取舍

- 清单里 `DisplayName` 用了中文「25仔」，靠数字实体过 MakeAppx。
- 若以后要做多语言显示名，建议改用 `ms-resource:` + PRI 资源，而不是往清单塞中文。
