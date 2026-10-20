; 25仔 安装包。由 .github/workflows/release.yml 调用：
;   iscc /DAppVersion=0.1.0 /DOutputBase=25zai-v0.1.0-windows-x64-setup .github/installer/25zai.iss
;
; 便携版 zip 仍然保留，这个包只是多一层开始菜单 / 开机自启 / 卸载。
; 装的是同一个 exe——它自己按参数决定当守护进程还是弹窗。

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef OutputBase
  #define OutputBase "25zai-windows-x64-setup"
#endif

#define AppName "25仔"
#define AppPublisher "refout"
#define AppUrl "https://github.com/refout/25zai-clipboard"
#define AppExeName "25zai.exe"

[Setup]
; AppId 一旦发布就不能改，否则升级会被当成另一个产品并存安装。
AppId={{8F2C1D4E-7A6B-4C3E-9D5F-2A1B6C8E0F31}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppUrl}
AppSupportURL={#AppUrl}/issues
AppUpdatesURL={#AppUrl}/releases
; 纯用户级安装：不弹 UAC，落到 %LOCALAPPDATA%\Programs\25zai
PrivilegesRequired=lowest
DefaultDirName={autopf}\25zai
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
AllowNoIcons=yes
OutputDir=..\..\dist
OutputBaseFilename={#OutputBase}
SourceDir=..\..\dist\stage
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\{#AppExeName}
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; 装完不重启：这个程序没有需要重启才能生效的东西。
CloseApplications=no

[Languages]
; 官方翻译文件随仓库带一份：choco 装的 Inno 版本不一定自带中文，
; 而 Default.isl 本身就是英文，不能拿它冒充中文。
Name: "chinese"; MessagesFile: "{#SourcePath}ChineseSimplified.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "autostart"; Description: "开机自动启动（后台常驻，不弹窗）"; GroupDescription: "附加任务："
Name: "desktopicon"; Description: "创建桌面快捷方式"; GroupDescription: "附加任务："; Flags: unchecked

[Files]
Source: "{#AppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "README.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExeName}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Tasks: desktopicon
; 自启走 --daemon：只把守护进程拉起来，不弹窗。
Name: "{userstartup}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Parameters: "--daemon"; Tasks: autostart

[Run]
Filename: "{app}\{#AppExeName}"; Description: "立即运行 {#AppName}"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; 卸载前先杀掉守护进程，否则 exe 被占用删不掉。
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM {#AppExeName}"; Flags: runhidden; RunOnceId: "KillApp"

[Code]
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ResultCode: Integer;
begin
  // 覆盖安装时旧守护进程还占着 exe，Windows 不允许直接覆盖正在运行的文件。
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM {#AppExeName}', '',
       SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Sleep(800);
  Result := '';
end;
