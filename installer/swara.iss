; Inno Setup 6 script for Swara. Installs for all users by default; the setup dialog also offers
; a per-user install. Built by installer\build.ps1 into installer\Output\Swara-Setup-<version>.exe.
;
; Uninstall deliberately leaves %APPDATA%\Swara (config and log) in place.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef ExePath
  #define ExePath "..\target\release\swara.exe"
#endif

#define Product "Swara"
#define ProductTitle "Swara - Audio Output Switcher"
#define ExeName "swara.exe"
#define HomePage "https://kurohiko.id"

[Setup]
AppId={{295F0471-25D7-4E7B-A671-AD8C0ADE8230}
AppName={#Product}
AppVersion={#AppVersion}
AppVerName={#Product} {#AppVersion}
AppPublisher=Kurohiko
AppPublisherURL={#HomePage}
DefaultDirName={autopf}\Swara
; No folder choice: in an all-users install another user could replace an exe kept outside
; Program Files.
DisableDirPage=yes
DisableProgramGroupPage=yes
PrivilegesRequired=admin
PrivilegesRequiredOverridesAllowed=dialog
; The app's silent self-update reuses whichever mode the current install has.
UsePreviousPrivileges=yes
ShowLanguageDialog=yes
SetupIconFile=..\assets\swara.ico
UninstallDisplayIcon={app}\{#ExeName}
UninstallDisplayName={#ProductTitle}
AppComments=Switch your Windows audio output with a keyboard hotkey or shortcut, and guard your default input and output so no other app changes them behind your back.
VersionInfoCompany=Kurohiko
VersionInfoProductName={#ProductTitle}
VersionInfoDescription={#ProductTitle} Setup
VersionInfoVersion={#AppVersion}
Compression=lzma2
SolidCompression=yes
OutputDir=Output
OutputBaseFilename=Swara-Setup-{#AppVersion}
WizardStyle=modern
WizardImageFile=art\side-100.png,art\side-125.png,art\side-150.png,art\side-175.png,art\side-200.png,art\side-250.png,art\side-300.png
WizardSmallImageFile=art\small-100.png,art\small-125.png,art\small-150.png,art\small-175.png,art\small-200.png,art\small-250.png,art\small-300.png
ArchitecturesAllowed=x64compatible
; 64-bit mode makes HKA resolve to the 64-bit HKLM view, which is where the app looks for Bahasa.
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
; Writing Bahasa under HKA (HKLM or HKCU depending on mode) is intended.
UsedUserAreasWarning=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "indonesian"; MessagesFile: "Indonesian.isl"

[Messages]
english.PrivilegesRequiredOverrideText1=Choose "Install for all users" to unlock "Run as admin" in Swara's Settings.%n%nPlaying League of Legends? Hotkeys already work in game. If you run League as administrator, turn this on so hotkeys keep working.%n%nChoose "Install for me only" if you don't have an administrator account.
indonesian.PrivilegesRequiredOverrideText1=Pilih "Pasang untuk semua pengguna" supaya "Jalankan sebagai admin" tersedia di Pengaturan Swara.%n%nMain League of Legends? Hotkey sudah jalan di dalam game. Kalau League kamu jalan sebagai administrator, nyalakan ini supaya hotkey tetap jalan.%n%nPilih "Pasang hanya untuk saya" kalau kamu tidak punya akun administrator.

[CustomMessages]
english.StartupTaskLabel=Start when Windows starts
indonesian.StartupTaskLabel=Jalankan saat Windows nyala
english.ShortcutTaskLabel=Create a &desktop shortcut
indonesian.ShortcutTaskLabel=Buat pintasan di &desktop
english.LinkSupport=Support the developer at kurohiko.id
indonesian.LinkSupport=Dukung developer di kurohiko.id
english.LinkKofi=Buy me a coffee on Ko-fi
indonesian.LinkKofi=Traktir kopi di Ko-fi
english.LinkTopUp=Top up your games at KurohikoTopUp.com
indonesian.LinkTopUp=Top up game kamu di KurohikoTopUp.com

[Tasks]
; Task names are stored by previous installs, so they stay as they are.
Name: "autostart"; Description: "{cm:StartupTaskLabel}"
Name: "desktopicon"; Description: "{cm:ShortcutTaskLabel}"; Flags: unchecked

[Files]
Source: "{#ExePath}"; Flags: ignoreversion; DestDir: "{app}"
; Finish-page button images (<name>-<lang>-<scale>.png), extracted on demand only.
Source: "art\*-??-???.png"; Flags: dontcopy

[Icons]
Name: "{autoprograms}\{#Product}"; Filename: "{app}\{#ExeName}"
Name: "{autodesktop}\{#Product}"; Filename: "{app}\{#ExeName}"; Tasks: desktopicon

[Registry]
; The app falls back to this value for its language while its own setting is automatic.
Root: HKA; Subkey: "Software\Swara"; ValueType: string; ValueName: "Bahasa"; ValueData: "{code:SetupLangTag}"; Flags: uninsdeletevalue uninsdeletekeyifempty

[Run]
Filename: "{app}\{#ExeName}"; Flags: nowait postinstall skipifsilent; Description: "{cm:LaunchProgram,{#Product}}"
; After a silent /UPDATE launched by the app, bring Swara back up.
Filename: "{app}\{#ExeName}"; Check: StartedByUpdater; Flags: nowait runasoriginaluser

[UninstallDelete]
; An elevated self-update keeps its downloaded setup next to the exe.
Type: files; Name: "{app}\Swara-Setup-*.exe"

[Code]
const
  StartupKey = 'Software\Microsoft\Windows\CurrentVersion\Run';
  StartupName = 'Swara';
  RunListPx = 28;

function StartedByUpdater: Boolean;
var
  N: Integer;
begin
  Result := False;
  for N := 1 to ParamCount do
    if CompareText(ParamStr(N), '/UPDATE') = 0 then Result := True;
end;

function SetupLangTag(Unused: String): String;
begin
  Result := 'en';
  if ActiveLanguage = 'indonesian' then Result := 'id';
end;

function InstalledExe: String;
begin
  Result := ExpandConstant('{app}\{#ExeName}');
end;

function TaskKill(const Switches: String): Boolean;
var
  Rc: Integer;
begin
  Result := Exec(ExpandConstant('{sys}\taskkill.exe'), Switches, '', SW_HIDE, ewWaitUntilTerminated, Rc)
    and (Rc = 0);
end;

{ Ends both the tray core and the settings window. The polite request comes first so Swara can
  save and quit; taskkill returns 0 only if something matched, so no wait when nothing runs. }
procedure CloseRunningSwara;
begin
  if TaskKill('/IM swara.exe') then
  begin
    Sleep(1500);
    TaskKill('/F /IM swara.exe');
  end;
end;

{ Going all-users removes the per-user copy of whoever launched setup, so there are never two.
  Its path is fixed (userpf\Swara with no folder page), and cmd expands that user's LOCALAPPDATA.
  Because the folder is writable by that user, its uninstaller must not run with admin rights:
  if setup was itself started elevated (update from an elevated core, Run as administrator), the
  original user is elevated as well. So it runs only when whoami shows the Medium Mandatory Level
  SID; a High token, or whoami failing to run, leaves findstr's exit 1 and nothing runs. }
procedure UninstallUserCopy;
var
  UserUninst: String;
  Rc: Integer;
begin
  UserUninst := '"%LOCALAPPDATA%\Programs\Swara\unins000.exe"';
  if not ExecAsOriginalUser(ExpandConstant('{sys}\cmd.exe'),
    '/s /c "' + ExpandConstant('"{sys}\whoami.exe" /groups | "{sys}\findstr.exe"') +
    ' S-1-16-8192 >nul && if exist ' + UserUninst + ' ' + UserUninst +
    ' /VERYSILENT /SUPPRESSMSGBOXES /NORESTART"',
    '', SW_HIDE, ewWaitUntilTerminated, Rc) then
    Log('per-user copy: uninstaller did not launch, ' + SysErrorMessage(Rc))
  else if Rc <> 0 then
    Log('per-user copy: uninstaller returned ' + IntToStr(Rc));
end;

function PrepareToInstall(var RestartNeeded: Boolean): String;
begin
  CloseRunningSwara;
  { Done once by the first all-users install; an update can come from an elevated core. }
  if IsAdminInstallMode and not StartedByUpdater then
    UninstallUserCopy;
  Result := '';
end;

{ Startup entry for the person who launched setup. In all-users mode HKCU is the elevating
  account, which may be a different admin, so reg.exe is run as the original user. }
procedure AddStartupEntry;
var
  Target: String;
  Rc: Integer;
begin
  Target := InstalledExe;
  if not IsAdminInstallMode then
    RegWriteStringValue(HKCU, StartupKey, StartupName, '"' + Target + '"')
  else if not ExecAsOriginalUser(ExpandConstant('{sys}\reg.exe'),
    'add "HKCU\' + StartupKey + '" /v ' + StartupName + ' /t REG_SZ /d "\"' + Target + '\"" /f',
    '', SW_HIDE, ewWaitUntilTerminated, Rc) or (Rc <> 0) then
    Log('startup entry: reg.exe failed, ' + IntToStr(Rc));
end;

procedure CurStepChanged(Step: TSetupStep);
begin
  { On a silent update the app's own autostart setting decides. }
  if (Step = ssPostInstall) and WizardIsTaskSelected('autostart') and not StartedByUpdater then
    AddStartupEntry;
end;

procedure Browse(const Address: String);
var
  Rc: Integer;
begin
  ShellExecAsOriginalUser('open', Address, '', '', SW_SHOWNORMAL, ewNoWait, Rc);
end;

procedure SupportClicked(Sender: TObject);
begin
  Browse('{#HomePage}');
end;

procedure KofiClicked(Sender: TObject);
begin
  Browse('https://ko-fi.com/kurohikoid');
end;

procedure TopUpClicked(Sender: TObject);
begin
  Browse('https://kurohikotopup.com');
end;

{ Picks the rendered button size closest to the current DPI. }
function ScaleTag: String;
var
  Pct: Integer;
begin
  Pct := ScaleX(100);
  if Pct <= 100 then Result := '100'
  else if Pct <= 150 then Result := '150'
  else if Pct <= 200 then Result := '200'
  else Result := '300';
end;

function MakeLinkButton(const Art, Text: String; Y, BtnW, BtnH: Integer; OnPress: TNotifyEvent): TBitmapButton;
var
  Png: String;
  Btn: TBitmapButton;
begin
  Png := Art + '-' + SetupLangTag('') + '-' + ScaleTag + '.png';
  ExtractTemporaryFile(Png);
  Btn := TBitmapButton.Create(WizardForm);
  Btn.Parent := WizardForm.FinishedPage;
  Btn.Caption := Text;
  Btn.Hint := Text;
  Btn.PngImage.LoadFromFile(ExpandConstant('{tmp}\') + Png);
  Btn.Stretch := True;
  Btn.Left := WizardForm.FinishedLabel.Left - ScaleX(2);
  Btn.Top := Y;
  Btn.Width := BtnW;
  Btn.Height := BtnH;
  Btn.Cursor := crHand;
  Btn.OnClick := OnPress;
  Result := Btn;
end;

{ Stacks the three link buttons at the foot of the Finish page; they only open on click. }
procedure InitializeWizard;
var
  BtnW, BtnH, Base, Spacing: Integer;
begin
  { The image sits inside a 2px focus border on every side of a TBitmapButton. }
  BtnW := WizardForm.FinishedPage.Width - WizardForm.FinishedLabel.Left - ScaleX(16) + ScaleX(4);
  BtnH := (BtnW - ScaleX(4)) * 58 div 440 + ScaleY(4);
  Spacing := ScaleY(4);
  Base := WizardForm.FinishedPage.Height - ScaleY(8);
  WizardForm.RunList.Height := ScaleY(RunListPx);
  MakeLinkButton('topup', CustomMessage('LinkTopUp'), Base - BtnH, BtnW, BtnH, @TopUpClicked);
  MakeLinkButton('kofi', CustomMessage('LinkKofi'), Base - 2 * BtnH - Spacing, BtnW, BtnH, @KofiClicked);
  MakeLinkButton('support', CustomMessage('LinkSupport'), Base - 3 * BtnH - 2 * Spacing, BtnW, BtnH, @SupportClicked);
end;

{ All-users uninstall: clear the startup entry in each loaded user hive that points here. HKCU
  might be some other admin. Signed-out users' hives are not loaded; their leftover entry points
  at a deleted exe, which Windows ignores. }
procedure ClearStartupEntries;
var
  Hives: TArrayOfString;
  Idx: Integer;
  HivePath, Stored, Needle: String;
begin
  Needle := Lowercase(InstalledExe);
  if not RegGetSubkeyNames(HKU, '', Hives) then Exit;
  for Idx := 0 to GetArrayLength(Hives) - 1 do
  begin
    HivePath := Hives[Idx] + '\' + StartupKey;
    if RegQueryStringValue(HKU, HivePath, StartupName, Stored) and (Pos(Needle, Lowercase(Stored)) > 0) then
      RegDeleteValue(HKU, HivePath, StartupName);
  end;
end;

{ All-users uninstall: drop every account's admin-mode task (`\Swara-*`). }
procedure DeleteElevatedTasks;
var
  Listing: TExecOutput;
  Idx, Rc: Integer;
  Row, TaskName, SchTasks: String;
begin
  SchTasks := ExpandConstant('{sys}\schtasks.exe');
  { If listing fails, carry on: once the exe is removed a leftover task just fails to start. }
  try
    if not ExecAndCaptureOutput(SchTasks, '/Query /FO CSV /NH', '',
      SW_HIDE, ewWaitUntilTerminated, Rc, Listing) then Exit;
  except
    Log('scheduled tasks: query raised ' + GetExceptionMessage);
    Exit;
  end;
  for Idx := 0 to GetArrayLength(Listing.StdOut) - 1 do
  begin
    Row := Listing.StdOut[Idx];
    { Rows look like "\Swara-rina","N/A","Ready"; quotes cannot occur in a user name. }
    if Pos('"\Swara-', Row) = 1 then
    begin
      TaskName := Copy(Row, 2, MaxInt);
      TaskName := Copy(TaskName, 1, Pos('"', TaskName) - 1);
      if TaskName <> '' then
        Exec(SchTasks, '/Delete /TN "' + TaskName + '" /F', '', SW_HIDE,
          ewWaitUntilTerminated, Rc);
    end;
  end;
end;

procedure CurUninstallStepChanged(Step: TUninstallStep);
var
  Stored: String;
begin
  if Step = usUninstall then
    CloseRunningSwara;
  if Step = usPostUninstall then
  begin
    if IsAdminInstallMode then
    begin
      ClearStartupEntries;
      DeleteElevatedTasks;
    end else if RegQueryStringValue(HKCU, StartupKey, StartupName, Stored)
      and (Pos(Lowercase(InstalledExe), Lowercase(Stored)) > 0) then
      { Leave it alone when it belongs to an all-users copy. }
      RegDeleteValue(HKCU, StartupKey, StartupName);
  end;
end;
