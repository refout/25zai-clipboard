<#
  重新生成 packaging/winget 下的清单，可选直接开 PR。
  用法:
    ./scripts/winget-update.ps1 -Version 0.1.3
    ./scripts/winget-update.ps1 -Version 0.1.3 -Submit -Token <PAT>

  winget 的 SHA256 从 release 的 SHA256SUMS.txt 取，不手抄。
#>
param(
  [Parameter(Mandatory = $true)][string]$Version,
  [switch]$Submit,
  [string]$Token = $env:WINGET_TOKEN
)

$ErrorActionPreference = 'Stop'
$tag = "v$Version"
$root = Split-Path -Parent $PSScriptRoot
$dir = Join-Path $root 'packaging/winget'
$base = 'https://github.com/refout/25zai-clipboard/releases/download'
$setupName = "25zai-$tag-windows-x64-setup.exe"
$setupUrl = "$base/$tag/$setupName"

Write-Host "拉取 $tag 的 SHA256..."
$sums = (Invoke-WebRequest "$base/$tag/SHA256SUMS.txt" -UseBasicParsing).Content
$line = [regex]::Split($sums, '\r?\n') | Where-Object { $_ -match 'setup\.exe' } | Select-Object -First 1
if (-not $line) { throw "SHA256SUMS 里找不到 setup.exe" }
$sha = ([regex]::Split($line.Trim(), '\s+'))[0].ToUpper()

function Write-Manifest([string]$name, [string]$body) {
  $path = Join-Path $dir $name
  [IO.File]::WriteAllText($path, $body.Trim() + [char]10, [Text.UTF8Encoding]::new($false))
  Write-Host "写入 $name"
}

$versionBody = @"
PackageIdentifier: Refout.25zai
PackageVersion: $Version
DefaultLocale: zh-CN
ManifestType: version
ManifestVersion: 1.6.0
"@

$installerBody = @"
PackageIdentifier: Refout.25zai
PackageVersion: $Version
InstallerLocale: zh-CN
Platform:
  - Windows.Desktop
MinimumOSVersion: 10.0.17763.0
InstallerType: inno
Scope: user
InstallModes:
  - interactive
  - silent
  - silentWithProgress
UpgradeBehavior: install
Installers:
  - Architecture: x64
    InstallerUrl: $setupUrl
    InstallerSha256: $sha
    InstallerSwitches:
      Silent: /VERYSILENT /SUPPRESSMSGBOXES /NORESTART
      SilentWithProgress: /SILENT /SUPPRESSMSGBOXES /NORESTART
    AppsAndFeaturesEntries:
      - DisplayName: 25仔
        Publisher: refout
        DisplayVersion: $Version
ManifestType: installer
ManifestVersion: 1.6.0
"@

$zhBody = @"
PackageIdentifier: Refout.25zai
PackageVersion: $Version
PackageLocale: zh-CN
Publisher: refout
PublisherUrl: https://github.com/refout/25zai-clipboard
PublisherSupportUrl: https://github.com/refout/25zai-clipboard/issues
PackageName: 25仔
PackageUrl: https://github.com/refout/25zai-clipboard
License: MIT
LicenseUrl: https://github.com/refout/25zai-clipboard/blob/main/LICENSE
Copyright: MIT
ShortDescription: 本地剪贴板历史管理器
Description: 25仔是一款本地优先的剪贴板历史管理器，支持文本、图片与文件列表，全局快捷键唤起，点条目直接粘贴，数据只存本机不上传。
Tags:
  - clipboard
  - clipboard-manager
  - productivity
  - privacy
ReleaseNotesUrl: https://github.com/refout/25zai-clipboard/releases/tag/$tag
ManifestType: locale
ManifestVersion: 1.6.0
"@

$enBody = @"
PackageIdentifier: Refout.25zai
PackageVersion: $Version
PackageLocale: en-US
Publisher: refout
PublisherUrl: https://github.com/refout/25zai-clipboard
PublisherSupportUrl: https://github.com/refout/25zai-clipboard/issues
PackageName: 25zai
PackageUrl: https://github.com/refout/25zai-clipboard
License: MIT
LicenseUrl: https://github.com/refout/25zai-clipboard/blob/main/LICENSE
Copyright: MIT
ShortDescription: Local-first clipboard history manager
Description: 25zai is a local-first clipboard history manager for Windows. Text, images and file lists, global hotkey, paste on click. Everything stays on your machine.
Tags:
  - clipboard
  - clipboard-manager
  - productivity
  - privacy
ReleaseNotesUrl: https://github.com/refout/25zai-clipboard/releases/tag/$tag
ManifestType: locale
ManifestVersion: 1.6.0
"@

Write-Manifest 'Refout.25zai.yaml' $versionBody
Write-Manifest 'Refout.25zai.installer.yaml' $installerBody
Write-Manifest 'Refout.25zai.locale.zh-CN.yaml' $zhBody
Write-Manifest 'Refout.25zai.locale.en-US.yaml' $enBody

Write-Host "SHA256 = $sha"

if ($Submit) {
  if (-not $Token) { throw "需要 -Token 或环境变量 WINGET_TOKEN" }
  if (-not (Get-Command wingetcreate -ErrorAction SilentlyContinue)) {
    Write-Host '安装 wingetcreate...'
    Invoke-WebRequest 'https://aka.ms/wingetcreate/latest' -OutFile "$env:TEMP/wingetcreate.exe" -UseBasicParsing
    & "$env:TEMP/wingetcreate.exe" submit $dir --token $Token
  } else {
    wingetcreate submit $dir --token $Token
  }
} else {
  Write-Host '本地清单已更新。提交: wingetcreate submit packaging/winget --token <PAT>'
}
