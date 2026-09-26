; Windows installer built from a release binary with Inno Setup 6.3 or newer:
;
;   iscc /DVersion=0.1.0 /DArch=x86_64 /DBinary=...\vespera.exe ^
;        /DOutputDir=dist packaging\windows\vespera.iss
;
; Arch matches the Rust target: x86_64 or aarch64. Installation uses the
; current user's Programs folder and does not need administrator rights.
; Updates close a running copy before replacing it.

#ifndef Version
  #error Version must be defined on the ISCC command line
#endif
#ifndef Arch
  #error Arch must be defined on the ISCC command line (x86_64 or aarch64)
#endif
#ifndef Binary
  #error Binary must be defined on the ISCC command line
#endif
#ifndef OutputDir
  #error OutputDir must be defined on the ISCC command line
#endif
#if Arch == "aarch64"
  #define InnoArch "arm64"
#else
  #define InnoArch "x64compatible"
#endif

#define AppName "Vespera"
#ifndef NumericVersion
  #define NumericVersion Version
#endif
#define AppExeName "vespera.exe"

[Setup]
; New program id. An update of this installer keeps this value.
AppId={{7E4C1A90-2B58-4D6F-8A13-5C9E0B7D2F46}
AppName={#AppName}
AppVersion={#Version}
AppVerName={#AppName} {#Version}
AppPublisher=vitorhubdev
AppPublisherURL=https://github.com/vitorhubdev/zapfast-extra
AppSupportURL=https://github.com/vitorhubdev/zapfast-extra/issues
AppUpdatesURL=https://github.com/vitorhubdev/zapfast-extra/releases
DefaultDirName={localappdata}\Programs\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed={#InnoArch}
ArchitecturesInstallIn64BitMode={#InnoArch}
MinVersion=10.0
LicenseFile=..\..\LICENSE
OutputDir={#OutputDir}
OutputBaseFilename=vespera-v{#Version}-{#Arch}-pc-windows-msvc-setup
SetupIconFile=vespera.ico
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes
RestartApplications=no
UninstallDisplayIcon={app}\{#AppExeName}
VersionInfoVersion={#NumericVersion}.0

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Additional shortcuts:"; Flags: unchecked

[Files]
Source: "{#Binary}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

Source: "vespera-installer.txt"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExeName}"; AppUserModelID: "io.github.vitorhubdev.Vespera"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Tasks: desktopicon; AppUserModelID: "io.github.vitorhubdev.Vespera"

[Run]
Filename: "{app}\{#AppExeName}"; Description: "Launch {#AppName}"; Flags: nowait postinstall skipifsilent
