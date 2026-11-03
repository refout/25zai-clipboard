<#
  把 target/release/25zai.exe 打成 MSIX（全信任 Win32）。
  用法（参数放一行或分行，PowerShell 不需要续行符）:
    ./packaging/msix/pack.ps1 -IdentityName Refout.25zai -Publisher 'CN=XXXX' -PublisherDisplayName refout -Version 0.1.2

  两个坑（MakeAppx 的报错是本地化的，经常只显示问号）:
  1. 清单里的中文必须转成数字实体，否则 MakeAppx 直接判 XML 无效。
  2. 全信任要用 EntryPoint="Windows.FullTrustApplication"，不能再加
     uap10:RuntimeBehavior="win32App"（两者互斥）。
#>
param(
  [Parameter(Mandatory = $true)][string]$IdentityName,
  [Parameter(Mandatory = $true)][string]$Publisher,
  [Parameter(Mandatory = $true)][string]$PublisherDisplayName,
  [Parameter(Mandatory = $true)][string]$Version,
  [string]$Exe = 'target/release/25zai.exe',
  [string]$OutDir = 'dist'
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)

# 版本补成四段
$parts = $Version.TrimStart('v').Split('.')
while ($parts.Count -lt 4) { $parts += '0' }
$msixVersion = ($parts[0..3] -join '.')

$layout = Join-Path $env:TEMP ("msix-layout-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path (Join-Path $layout 'Assets') | Out-Null

Copy-Item (Join-Path $root $Exe) $layout
Copy-Item (Join-Path $root 'README.md') $layout
Copy-Item (Join-Path $root 'LICENSE') $layout
Copy-Item (Join-Path $root 'packaging/msix/Assets/*') (Join-Path $layout 'Assets')

# 替换占位符
$text = [IO.File]::ReadAllText((Join-Path $root 'packaging/msix/AppxManifest.xml'), [Text.Encoding]::UTF8)
$text = $text.Replace('{{IDENTITY_NAME}}', $IdentityName)
$text = $text.Replace('{{PUBLISHER}}', $Publisher)
$text = $text.Replace('{{PUBLISHER_DISPLAY_NAME}}', $PublisherDisplayName)
$text = $text.Replace('{{VERSION}}', $msixVersion)

# 非 ASCII 转数字实体（见文件头说明）
$sb = New-Object System.Text.StringBuilder
foreach ($ch in $text.ToCharArray()) {
  $code = [int]$ch
  if ($code -gt 127) {
    [void]$sb.Append('&#x' + $code.ToString('X') + ';')
  } else {
    [void]$sb.Append($ch)
  }
}
[IO.File]::WriteAllText((Join-Path $layout 'AppxManifest.xml'), $sb.ToString(), [Text.Encoding]::ASCII)

# 找 MakeAppx
$pf86 = [Environment]::GetFolderPath('ProgramFilesX86')
$sdk = Get-ChildItem (Join-Path $pf86 'Windows Kits/10/bin') -Directory |
  Where-Object { $_.Name -match '^[0-9]+\.' } | Sort-Object Name -Descending | Select-Object -First 1
if (-not $sdk) { throw '找不到 Windows SDK（需要 MakeAppx.exe）' }
$makeappx = Join-Path $sdk.FullName 'x64/makeappx.exe'

New-Item -ItemType Directory -Force -Path (Join-Path $root $OutDir) | Out-Null
$out = Join-Path $root (Join-Path $OutDir ("25zai-$Version-windows-x64.msix"))
& $makeappx pack /d $layout /p $out /o | Out-Null
if ($LASTEXITCODE -ne 0) { throw "MakeAppx 失败（$LASTEXITCODE）" }
Write-Host ("MSIX 已生成: " + $out)
Write-Output $out
