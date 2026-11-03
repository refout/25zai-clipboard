# Microsoft Store（MSIX）

Win32 程序上架 Store 只能走 MSIX（全信任打包）。`pack.ps1` 把
`target/release/25zai.exe` + 图标资产 + 清单打成一个 `.msix`。

## 先准备

仓库变量（Settings → Secrets and variables → Actions → Variables）已填好，
对应 Partner Center → 产品管理 → 产品标识：

| 变量 | 当前值 | 说明 |
|---|---|---|
| `MSIX_IDENTITY_NAME` | `2169HaleWang.25zai` | Package/Identity/Name。前缀 `2169HaleWang` 是账号级命名空间，预留时就冻结，改不了 |
| `MSIX_PUBLISHER` | `CN=0D4B9868-...` | Package/Identity/Publisher |
| `MSIX_PUBLISHER_DISPLAY_NAME` | `refout` | Package/Properties/PublisherDisplayName，商店页显示的发布者 |

注意 Identity Name **不是** `refout.25zai`：前缀跟发布者显示名称无关，填错会被拒包。
发版时 `release.yml` 会拒绝占位值，避免打出一个 Publisher 不匹配、上传必被拒的包。

产品标识全貌（用于核对，PFN / Store ID 不进清单）：

```
Package/Identity/Name                2169HaleWang.25zai
Package/Identity/Publisher           CN=0D4B9868-9F65-4FEB-980D-79CCAE83C901
Package/Properties/PublisherDisplayName  refout
Package Family Name (PFN)            2169HaleWang.25zai_ytvrv8s2wkq3e
Store ID                             9PBMHLXLWF4T
```

## 本地打包

```powershell
cargo build --release -p erwuzai-ui
./packaging/msix/pack.ps1 -IdentityName 2169HaleWang.25zai -Publisher 'CN=0D4B9868-9F65-4FEB-980D-79CCAE83C901' -PublisherDisplayName refout -Version 0.1.2
```

产物在 `dist/25zai-<版本>-windows-x64.msix`。发版时 `release.yml` 会顺手用自签证书
签名（Store 会用微软证书重签，自签只要 Subject 等于 Publisher 即可）。

## 两个必踩的坑（MakeAppx 报错是本地化的，经常只显示问号）

1. **清单里的中文必须转成数字实体**（`&#x4ED4;` 这种）。直接放 UTF-8 中文，
   MakeAppx 会报 “App manifest XML must be valid” 但原因全是 `?`。
   `pack.ps1` 已经自动转，模板里可以正常写中文。
2. **全信任不能再加 `uap10:RuntimeBehavior="win32App"`**，否则报
   “Application with RuntimeBehavior value win32App must not declare EntryPoint”。
   正确写法是只留 `EntryPoint="Windows.FullTrustApplication"`。

## 上架流程

MSIX 跟 exe 一起随 Release 发布，不用单独跑流水线：

1. 打 tag 发版（`release.yml` 自动打包 + 自签 + 上传到 Release）
2. 从 Releases 页面下载 `25zai-<版本>-windows-x64.msix`
3. Partner Center → 提交 → 上传包
4. 填商店信息：截图（≥1 张，1366×768 以上）、描述、**隐私政策 URL**
5. 年龄分级问卷（IARC）→ 提交认证

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
