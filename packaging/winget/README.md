# winget 发布

清单在 `packaging/winget/`，当前是 0.1.2。用 Inno 安装包（`setup.exe`）作为安装源，
`InstallerType: inno`，用户级安装。

## 首次提交（还没进 winget-pkgs 时）

1. 装 wingetcreate：

   ```powershell
   iwr https://aka.ms/wingetcreate/latest -OutFile wingetcreate.exe
   ```

2. 确认清单校验通过：

   ```powershell
   .\wingetcreate.exe validate .\packaging\winget
   ```

3. 提交 PR（需要 GitHub PAT，带 public_repo）：

   ```powershell
   .\wingetcreate.exe submit .\packaging\winget --token <你的PAT>
   ```

   等 microsoft/winget-pkgs 的自动校验和人工审核合并。

## 以后发版

发版后跑：

```powershell
./scripts/winget-update.ps1 -Version 0.1.3 -Submit -Token <你的PAT>
```

脚本会从 release 的 `SHA256SUMS.txt` 取 SHA256，重写四份清单，然后调
`wingetcreate submit`。不加 `-Submit` 就只更新本地文件。

CI 里也接了（`.github/workflows/winget.yml`）：release 发布后，如果仓库配了
`WINGET_TOKEN` secret，会自动开 PR；没配就跳过。

## 本地验证

```powershell
winget install Refout.25zai          # 装（合并之后才生效）
winget show Refout.25zai             # 看元数据
winget uninstall Refout.25zai
```

> 注意：winget 的 `PackageIdentifier` 一旦合并就不能改。`Refout.25zai` 是最终值。
