; Inno Setup script for invisible-ptt.
;
;   cargo build --release
;   iscc /DAppVersion=0.1.0 installer\invisible-ptt.iss
;       -> dist\invisible-ptt-0.1.0-setup.exe
;
; Needs Inno Setup 6.3 or newer (for ArchitecturesAllowed=x64compatible). You do
; not need it installed locally: the Installer workflow builds this on demand,
; and a version tag publishes it. See .github/actions/build-installer.
;
; The install is deliberately per-user and unelevated. Everything this daemon
; touches is per-user already - the sign-in entry under HKCU, the config and log
; under %APPDATA%, the interactive session it must run in - so asking for
; administrator would buy nothing and cost the UAC prompt.

#define AppName       "invisible-ptt"
#define AppPublisher  "marhag87"

; CI passes the real one in with /DAppVersion=..., read out of Cargo.toml, so
; the two cannot drift. This fallback only exists for compiling the script by
; hand; it is allowed to be stale.
; 0.0.0 rather than a word like "dev" because VersionInfoVersion below needs
; something numeric.
#ifndef AppVersion
  #define AppVersion  "0.0.0"
#endif

#define AppURL        "https://github.com/marhag87/invisible-ptt"
#define ExeName       "invisible-ptt.exe"

; Must match src/tray.rs: RUN_KEY, RUN_VALUE, and the window class, or the
; tray's own "Start automatically at sign-in" tick disagrees with the installer.
#define RunKey        "Software\Microsoft\Windows\CurrentVersion\Run"
#define RunValue      "invisible-ptt"
#define TrayClass     "invisible-ptt-tray"

[Setup]
AppId={{8F3A1C6D-2B47-4E9A-9D51-6C0E7A4B2F83}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppURL}
AppSupportURL={#AppURL}
AppUpdatesURL={#AppURL}/releases
VersionInfoVersion={#AppVersion}

PrivilegesRequired=lowest
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
LicenseFile=..\LICENSE
OutputDir=..\dist
OutputBaseFilename={#AppName}-{#AppVersion}-setup
UninstallDisplayName={#AppName}
UninstallDisplayIcon={app}\{#ExeName}

ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern

; Let the Restart Manager close a running copy when installing over one. This
; covers Setup only - the uninstaller does not consult it, which is what
; InitializeUninstall in [Code] is for.
;
; Either way it is safe *only* because the tray window treats WM_CLOSE and
; WM_QUERYENDSESSION as "stop the daemon" rather than "destroy the window" (see
; wndproc in src/tray.rs). Under the default handling those messages would kill
; the tray thread alone and leave the input loop running with no icon and a
; mouse button still withheld from Windows.
;
; RestartApplications=no: bringing the daemon back up afterwards is the Run
; key's job if the user asked for it, and silently restarting a program that is
; in the middle of being uninstalled is not helpful.
CloseApplications=yes
RestartApplications=no

[Files]
Source: "..\target\release\{#ExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\config.toml.example";       DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md";                 DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE";                   DestDir: "{app}"; Flags: ignoreversion

; Note what is *not* installed: config.toml. The daemon writes an inert starter
; to %APPDATA%\invisible-ptt on first run and a config beside the exe would
; override it (see config_path in src/main.rs), so shipping one here would
; silently take that decision away and survive uninstall as a stray file.

[Tasks]
Name: "autostart"; Description: "Start {#AppName} automatically when I sign in"

[Registry]
; Bare exe path, no config argument: the daemon resolves its own config the same
; way it does on a manual launch. The tray toggle writes exe + config; both are
; the same value name, so the tick stays truthful either way.
Root: HKCU; Subkey: "{#RunKey}"; ValueType: string; ValueName: "{#RunValue}"; \
    ValueData: """{app}\{#ExeName}"""; Flags: uninsdeletevalue; Tasks: autostart

[Icons]
Name: "{group}\{#AppName}";           Filename: "{app}\{#ExeName}"
Name: "{group}\Uninstall {#AppName}"; Filename: "{uninstallexe}"

[Run]
Filename: "{app}\{#ExeName}"; Description: "Start {#AppName} now"; \
    Flags: nowait postinstall skipifsilent

[Code]

const
  WM_CLOSE = $0010;

function PostMessage(hWnd: Integer; Msg: Cardinal; wParam: Integer;
  lParam: Integer): Boolean;
  external 'PostMessageW@user32.dll stdcall';

function TrayWindow: Integer;
begin
  Result := FindWindowByClassName('{#TrayClass}');
end;

{ Ask a running daemon to stop, and wait until it has.

  Killing it is not an option: an uninstall that leaves the mouse mid-mapping
  takes the only program that can undo it with it, and the button stays dead
  until the mouse power-cycles. But it does not need killing - the tray window
  reads WM_CLOSE as "stop the daemon" (see wndproc in src/tray.rs), which is
  the same path as the Exit menu item, so it restores the mouse on the way out.

  The wait is for the window to disappear, which the tray does only after the
  input loop has finished with the mouse, plus a moment for the process to go
  and release the exe. False if it is still there after ten seconds. }
function StopDaemon: Boolean;
var
  Wnd, I: Integer;
  Sent: Boolean;
begin
  Wnd := TrayWindow;
  if Wnd = 0 then
  begin
    Result := True;
    Exit;
  end;

  Sent := PostMessage(Wnd, WM_CLOSE, 0, 0);
  if Sent then
    for I := 1 to 100 do
    begin
      if TrayWindow = 0 then
      begin
        Sleep(250);
        Result := True;
        Exit;
      end;
      Sleep(100);
    end;

  Result := False;
end;

{ The fallback, for a daemon that did not answer - one whose tray window never
  came up, say, so there is nothing to post to. Ask, and keep asking. }
function EnsureStopped(const Reason: String): Boolean;
begin
  while not StopDaemon do
  begin
    if MsgBox(Reason + #13#10#13#10
              + 'Right-click the {#AppName} icon in the notification area and '
              + 'choose Exit, then click Retry. Exit is also what hands your '
              + 'mouse button back to Windows.',
              mbConfirmation, MB_RETRYCANCEL) = IDCANCEL then
    begin
      Result := False;
      Exit;
    end;
  end;
  Result := True;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  if EnsureStopped('{#AppName} is still running, and Setup cannot replace a '
                   + 'program while it is in use.') then
    Result := ''
  else
    Result := '{#AppName} is still running.';
end;

{ CloseApplications is a Setup directive; the uninstaller never consults it, so
  without this an uninstall would happily run with the daemon still up. }
function InitializeUninstall: Boolean;
begin
  Result := EnsureStopped('{#AppName} is running and did not respond to a '
                          + 'request to close.');
end;

procedure CurUninstallStepChanged(CurStep: TUninstallStep);
var
  Settings: String;
begin
  if CurStep <> usPostUninstall then
    Exit;

  { uninsdeletevalue only covers the entry Setup itself wrote. If the user
    ticked autostart from the tray menu instead, the value is still there and
    would point at a deleted exe at every sign-in. }
  RegDeleteValue(HKEY_CURRENT_USER, '{#RunKey}', '{#RunValue}');

  { The config holds Discord OAuth tokens, so neither leaving it nor deleting
    it quietly is the obvious right answer. Ask.

    Note the leading "+" on the continuation lines. The preprocessor runs over
    this file first and reads any line whose first non-blank character is "#"
    as a directive, so a wrapped string that starts with #13#10 fails the
    compile with "Unknown preprocessor directive" - which is what it did. }
  Settings := ExpandConstant('{userappdata}\{#AppName}');
  if DirExists(Settings) then
    if MsgBox('Delete your settings and log file too?' + #13#10#13#10 + Settings
              + #13#10#13#10 + 'Keep them if you plan to reinstall. They include '
              + 'your Discord credentials.',
              mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
      DelTree(Settings, True, True, True);
end;
