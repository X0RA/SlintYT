#define AppName "SlintYT"
#define AppExeName "slintyt.exe"
#define AppPublisher "SlintYT"
#ifndef AppVersion
#define AppVersion "0.1.0"
#endif
#ifndef AppSourceDir
#define AppSourceDir "..\..\target\release"
#endif
#ifndef AppOutputDir
#define AppOutputDir "..\..\target\release"
#endif
#ifndef AppOutputBaseFilename
#define AppOutputBaseFilename "SlintYT-" + AppVersion + "-Setup"
#endif

[Setup]
AppId={{D1B4F9AB-936F-4496-889C-8DF8E1DBBB81}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
AppVerName={#AppName} {#AppVersion}
DefaultDirName={commonpf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
AllowNoIcons=yes
OutputDir={#AppOutputDir}
OutputBaseFilename={#AppOutputBaseFilename}
SetupIconFile=..\..\ui\assets\appicon.ico
UninstallDisplayIcon={app}\appicon.ico
Compression=lzma2
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=admin
UsedUserAreasWarning=no
WizardStyle=modern
InfoAfterFile=windows-readme.txt

[Tasks]
Name: "startmenuicon"; Description: "Create a Start Menu shortcut"; GroupDescription: "Shortcuts:"; Flags: checkedonce
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "Shortcuts:"; Flags: unchecked

[Files]
Source: "{#AppSourceDir}\{#AppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#AppSourceDir}\libmpv-2.dll"; DestDir: "{app}"; Flags: ignoreversion

Source: "windows-readme.txt"; DestDir: "{app}"; DestName: "README.txt"; Flags: ignoreversion
Source: "..\..\ui\assets\appicon.ico"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExeName}"; WorkingDir: "{app}"; IconFilename: "{app}\appicon.ico"; Tasks: startmenuicon
Name: "{group}\Uninstall {#AppName}"; Filename: "{uninstallexe}"; Tasks: startmenuicon
Name: "{commondesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; WorkingDir: "{app}"; IconFilename: "{app}\appicon.ico"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExeName}"; Description: "{cm:LaunchProgram,{#StringChange(AppName, '&', '&&')}}"; Flags: nowait postinstall skipifsilent

; Preserve browser settings, account history and cache on uninstall.
