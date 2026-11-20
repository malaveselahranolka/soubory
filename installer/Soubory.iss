; Instalátor Souborů (Inno Setup 6.7 nebo novější). Sestavuje ho .github\workflows\release.yml:
;   scripts\build-release.ps1
;   iscc /DAppVersion=0.1.0-beta.1 /DNumVersion=0.1.0.0 installer\Soubory.iss
; Výsledek: dist\Soubory-<verze>-setup.exe. Obrázky a ikona: fm --render assets --brand.
;
; Instaluje jen pro přihlášeného uživatele (bez práv správce) do %LOCALAPPDATA%\Programs\Soubory:
; fm.exe, zástupce v nabídce Start, App Paths ("fm" ve Win+R), spouštění po přihlášení
; (fm --background) a odinstalaci v Nastavení > Aplikace. Úlohy:
;   defaultfm   fm.exe --set-default / --restore-explorer (složky, disky, Win+E … v Souborech),
;               bez PowerShellu, takže ho antivirus ani zásady blokující skripty nezastaví
;   dialogkit   PowerShell skripty ze scripts\, nainstalované do {app}\setup:
;               install-dialogkit.ps1 / uninstall-dialogkit.ps1 (do {app}\dialogkit)
;   shim        okno Souborů místo dialogu Otevřít (Claude, Chrome), stejně přes skripty:
;               install-shim.ps1 / uninstall-shim.ps1 (do {app}\shim)
; Každá instalace i aktualizace nastaví přesně to, co je zaškrtnuté. Odinstalace vrátí Průzkumník
; a pak ověří, že na tento fm.exe už nevede žádné sloveso (když fm.exe nejde spustit, smaže je sama).

#ifndef AppVersion
  #define AppVersion "0.0.0-dev"
#endif
#ifndef NumVersion
  #define NumVersion "0.0.0.0"
#endif
#ifndef Style
  #define Style "modern dynamic windows11"
#endif
#define Repo "https://github.com/malaveselahranolka/soubory"
#define Big(Name) "..\assets\wizard-" + Name + "-100.png,..\assets\wizard-" + Name + "-125.png,..\assets\wizard-" + Name + "-150.png,..\assets\wizard-" + Name + "-175.png,..\assets\wizard-" + Name + "-200.png,..\assets\wizard-" + Name + "-225.png,..\assets\wizard-" + Name + "-250.png"

[Setup]
AppId={{CB6D685E-6A4F-4914-9B9A-5E9376AE930D}
AppName=Soubory
AppVersion={#AppVersion}
AppVerName=Soubory {#AppVersion}
AppPublisher=malaveselahranolka
AppPublisherURL={#Repo}
AppSupportURL={#Repo}/issues
AppUpdatesURL={#Repo}/releases
AppCopyright=© 2026 malaveselahranolka, GNU GPL v3 nebo novější
VersionInfoVersion={#NumVersion}
VersionInfoProductName=Soubory
VersionInfoDescription=Instalátor Souborů
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.17763
DefaultDirName={localappdata}\Programs\Soubory
DisableDirPage=yes
DisableProgramGroupPage=yes
DisableWelcomePage=no
DisableReadyPage=yes
WizardStyle={#Style}
WizardImageFile={#Big("light")}
WizardImageFileDynamicDark={#Big("dark")}
WizardSmallImageFile={#Big("small")}
WizardSmallImageFileDynamicDark={#Big("small")}
WizardSmallImageBackColor=none
WizardSmallImageBackColorDynamicDark=none
SetupIconFile=..\assets\fm.ico
UninstallDisplayIcon={app}\fm.exe
UninstallDisplayName=Soubory
OutputDir=..\dist
OutputBaseFilename=Soubory-{#AppVersion}-setup
Compression=lzma2/max
SolidCompression=yes
; fm se ukončí sám (fm --quit) a nejdřív dokončí rozdělané kopírování; Restart Manager by ho zabil.
CloseApplications=no
RestartApplications=no
SetupMutex=SouborySetup
SetupLogging=yes
ShowLanguageDialog=no

[Languages]
Name: "cs"; MessagesFile: "compiler:Languages\Czech.isl"

[Messages]
WelcomeLabel1=Vítejte v instalaci Souborů
WelcomeLabel2=Soubory jsou správce souborů ve stylu Finderu: vyhledávání v názvech souborů na všech discích, rychlý náhled mezerníkem, karty a barevné štítky.%n%nInstalují se jen pro váš účet a nepotřebují práva správce. Jde o testovací verzi {#AppVersion}.
SelectTasksLabel2=Vyberte, co má instalátor nastavit, a klikněte na Instalovat. Později to změníte tak, že instalátor spustíte znovu.
FinishedHeadingLabel=Soubory jsou nainstalované

[CustomMessages]
GroupDefault=Výchozí správce souborů:
TaskDefault=Otevírat složky, disky a Win+E v Souborech místo Průzkumníka souborů
GroupAddon=Doplňky (zkušební):
TaskShim=Okno Otevřít v Claudovi a Chrome: vybírat soubory v Souborech
TaskDialogkit=Tlačítko „Nový“ v oknech Otevřít a Uložit ostatních aplikací; složky, které jiné aplikace otevírají v Průzkumníkovi (třeba „Zobrazit ve složce“), otevírat v Souborech
OpenNow=Otevřít Soubory
Elevated=Instalátor běží s právy správce. Soubory se instalují jen pro váš účet a jako správce běžet nesmějí.%n%nZavřete ho a spusťte znovu obyčejně: dvojklikem, bez „Spustit jako správce“.
Busy=Soubory ještě dokončují kopírování nebo přesun, nebo mají otevřený dialog. Dokončete nebo zrušte operaci, případně Soubory zavřete, a spusťte instalátor znovu.
ScriptFailed=Nepodařilo se %1:%n%n%2%n%nSoubory jsou nainstalované, jen tahle volba se nenastavila. Podrobnosti jsou v protokolu instalace:%n%3
DoDefault=nastavit Soubory jako výchozí pro složky
DoRestore=vrátit složky Průzkumníku souborů
DoKit=nainstalovat tlačítko „Nový“
DoUnkit=odebrat tlačítko „Nový“
DoShim=zapnout okno Souborů v dialogu Otevřít
DoUnshim=vypnout okno Souborů v dialogu Otevřít
FinishedDefault=Soubory najdete v nabídce Start. Složky, disky a Win+E se teď otevírají v Souborech.%n%nPrůzkumník souborů vrátíte tak, že Soubory odinstalujete (Nastavení › Aplikace), nebo instalátor spustíte znovu a volbu zrušíte.
FinishedPlain=Soubory najdete v nabídce Start. Složky se dál otevírají v Průzkumníkovi souborů; změníte to tak, že instalátor spustíte znovu.
UninstBusy=Soubory ještě dokončují kopírování nebo přesun, nebo mají otevřený dialog. Dokončete nebo zrušte operaci, případně Soubory zavřete, a odinstalaci spusťte znovu.
UninstData=Smazat také nastavení, štítky a vyhledávací index Souborů?%n%nKdyž je ponecháte, budou po nové instalaci zase k dispozici.

[Tasks]
Name: "defaultfm"; Description: "{cm:TaskDefault}"; GroupDescription: "{cm:GroupDefault}"
Name: "shim"; Description: "{cm:TaskShim}"; GroupDescription: "{cm:GroupAddon}"
Name: "dialogkit"; Description: "{cm:TaskDialogkit}"; GroupDescription: "{cm:GroupAddon}"; Flags: unchecked

[Files]
Source: "..\dist\app\fm.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\dist\app\THIRD-PARTY-NOTICES.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion
Source: "..\scripts\install-dialogkit.ps1"; DestDir: "{app}\setup"; Flags: ignoreversion
Source: "..\scripts\uninstall-dialogkit.ps1"; DestDir: "{app}\setup"; Flags: ignoreversion
Source: "..\scripts\lib\install-common.ps1"; DestDir: "{app}\setup\lib"; Flags: ignoreversion
; install-dialogkit.ps1 copies these on to {app}\dialogkit; a hook DLL still loaded in some open
; dialog is set aside there instead of failing the install.
Source: "..\dist\dialogkit\*"; DestDir: "{app}\setup\dialogkit"; Excludes: "*.pdb"; Flags: ignoreversion recursesubdirs
; The same for the Open-dialog shim: install-shim.ps1 copies it on to {app}\shim.
Source: "..\scripts\install-shim.ps1"; DestDir: "{app}\setup"; Flags: ignoreversion
Source: "..\scripts\uninstall-shim.ps1"; DestDir: "{app}\setup"; Flags: ignoreversion
Source: "..\dist\shim\fm_dialog_shim.dll"; DestDir: "{app}\setup\shim"; Flags: ignoreversion

[InstallDelete]
; 0.1.0-beta.1 installed these; fm.exe --set-default / --restore-explorer replaced them.
Type: files; Name: "{app}\setup\set-default-fm.ps1"
Type: files; Name: "{app}\setup\restore-explorer.ps1"

[Icons]
Name: "{autoprograms}\Soubory"; Filename: "{app}\fm.exe"; Comment: "Správce souborů"

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "Soubory"; ValueData: """{app}\fm.exe"" --background"; Flags: uninsdeletevalue
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\App Paths\fm.exe"; ValueType: string; ValueName: ""; ValueData: "{app}\fm.exe"; Flags: uninsdeletekey

[Run]
Filename: "{app}\fm.exe"; Description: "{cm:OpenNow}"; Flags: postinstall nowait skipifsilent

[Code]
const
  FmMutex = 'Local\fm.main';
  TOKEN_QUERY = 8;
  TokenElevationType = 18;

var
  FailedDefault: Boolean;
  KitPreset: Boolean;

function OpenProcessToken(Process: THandle; Access: Cardinal; var Token: THandle): Boolean;
  external 'OpenProcessToken@advapi32.dll stdcall';
function GetTokenInformation(Token: THandle; InfoClass: Integer; var Info: Integer; Len: Cardinal; var RetLen: Cardinal): Boolean;
  external 'GetTokenInformation@advapi32.dll stdcall';
function GetCurrentProcess: THandle;
  external 'GetCurrentProcess@kernel32.dll stdcall';
function CloseHandle(H: THandle): Boolean;
  external 'CloseHandle@kernel32.dll stdcall';

{ The elevated half of a split token (TokenElevationTypeFull), which the scripts and fm.exe refuse.
  With UAC off or as the built-in Administrator there is no normal token to run with: allowed. }
function ElevatedSplitToken: Boolean;
var
  T: THandle;
  V: Integer;
  L: Cardinal;
begin
  Result := False;
  if OpenProcessToken(GetCurrentProcess, TOKEN_QUERY, T) then begin
    if GetTokenInformation(T, TokenElevationType, V, 4, L) then
      Result := V = 2;
    CloseHandle(T);
  end;
end;

function PsQuote(S: String): String;
begin
  StringChangeEx(S, '''', '''''', True);
  Result := '''' + S + '''';
end;

// Runs Exe hidden and logs its output (read as UTF-8). Err: its first stderr line, else why it failed.
function RunCaptured(const Exe, Params, What: String; var Err: String): Boolean;
var
  Code, I: Integer;
  Output: TExecOutput;
begin
  Err := '';
  Log(What);
  Result := ExecAndCaptureOutput(Exe, Params, ExpandConstant('{sys}'), SW_HIDE, ewWaitUntilTerminated, Code, Output);
  for I := 0 to GetArrayLength(Output.StdOut) - 1 do
    Log('  ' + Output.StdOut[I]);
  for I := 0 to GetArrayLength(Output.StdErr) - 1 do begin
    Log('  ! ' + Output.StdErr[I]);
    if (Err = '') and (Trim(Output.StdErr[I]) <> '') then
      Err := Trim(Output.StdErr[I]);
  end;
  if not Result then
    Err := SysErrorMessage(Code)
  else if Code <> 0 then begin
    Result := False;
    if Err = '' then
      Err := Format('%s skončil s kódem %d.', [ExtractFileName(Exe), Code]);
  end;
  Log(Format('  exit code %d', [Code]));
end;

// Runs <Name> from the setup folder in a hidden 64-bit Windows PowerShell.
function RunScript(const Name, Args: String; var Err: String): Boolean;
begin
  Result := RunCaptured(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
    '-NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "[Console]::OutputEncoding = [Text.Encoding]::UTF8; try { & '
    + PsQuote(ExpandConstant('{app}\setup\') + Name) + ' ' + Args
    + ' } catch { [Console]::Error.WriteLine($_.Exception.Message); exit 1 }"',
    'PowerShell: ' + Name + ' ' + Args, Err);
end;

// fm.exe --set-default or --restore-explorer.
function RunFm(const Arg: String; var Err: String): Boolean;
begin
  Result := RunCaptured(ExpandConstant('{app}\fm.exe'), Arg, 'fm.exe ' + Arg, Err);
end;

procedure Report(const What, Err: String);
begin
  SuppressibleMsgBox(FmtMessage(CustomMessage('ScriptFailed'), [CustomMessage(What), Err, ExpandConstant('{log}')]),
    mbError, MB_OK, IDOK);
end;

{ Asks the running fm to finish its copies and quit, and waits for it up to two minutes. }
function QuitFm(const Exe: String): Boolean;
var
  Code, I: Integer;
begin
  Result := not CheckForMutexes(FmMutex);
  if Result then
    Exit;
  if FileExists(Exe) then
    Exec(Exe, '--quit', ExpandConstant('{sys}'), SW_HIDE, ewWaitUntilTerminated, Code);
  for I := 1 to 600 do begin
    if not CheckForMutexes(FmMutex) then begin
      Result := True;
      Exit;
    end;
    Sleep(200);
  end;
end;

function HasMarker(const Dir, Product: String): Boolean;
var
  S: AnsiString;
begin
  Result := LoadStringFromFile(Dir + '\.fm-install', S) and (Trim(String(S)) = Product);
end;

{ Where the install scripts used before this installer put Soubory. }
function OldDir(const Part: String): String;
begin
  Result := ExpandConstant('{localappdata}\Programs\fm\') + Part;
end;

function InitializeSetup: Boolean;
begin
  Result := not ElevatedSplitToken;
  if not Result then
    SuppressibleMsgBox(CustomMessage('Elevated'), mbCriticalError, MB_OK, IDOK);
end;

procedure CurPageChanged(CurPageID: Integer);
begin
  { Coming from the old scripts with dialogkit installed: keep it. }
  if (CurPageID = wpSelectTasks) and not KitPreset then begin
    KitPreset := True;
    if HasMarker(OldDir('dialogkit'), 'fm-dialogkit') then
      WizardSelectTasks('dialogkit');
  end;
  { The Ready page is off, so this page starts the install; Inno still labels it Next. }
  if CurPageID = wpSelectTasks then
    WizardForm.NextButton.Caption := SetupMessage(msgButtonInstall);
  if CurPageID = wpFinished then begin
    if WizardIsTaskSelected('defaultfm') and not FailedDefault then
      WizardForm.FinishedLabel.Caption := CustomMessage('FinishedDefault')
    else
      WizardForm.FinishedLabel.Caption := CustomMessage('FinishedPlain');
    { The "Otevřít Soubory" checkbox goes a blank line below the text. }
    WizardForm.AdjustLabelHeight(WizardForm.FinishedLabel);
    WizardForm.RunList.Top := WizardForm.FinishedLabel.Top + WizardForm.FinishedLabel.Height + ScaleY(14);
  end;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  ExtractTemporaryFile('fm.exe');
  if not QuitFm(ExpandConstant('{tmp}\fm.exe')) then
    Result := CustomMessage('Busy');
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  Err, Exe, Kit, Shim: String;
  Code: Integer;
begin
  if CurStep <> ssPostInstall then
    Exit;
  Exe := ExpandConstant('{app}\fm.exe');
  Kit := ExpandConstant('{app}\dialogkit');
  Shim := ExpandConstant('{app}\shim');
  { An install by the old scripts (%LOCALAPPDATA%\Programs\fm): its dialogkit and shim are
    uninstalled and its folder deleted; Run, App Paths and the Start menu shortcut were just
    rewritten for this one, and fm --set-default below moves the verbs over. }
  if HasMarker(OldDir('dialogkit'), 'fm-dialogkit') and not RunScript('uninstall-dialogkit.ps1', '-InstallDir ' + PsQuote(OldDir('dialogkit')), Err) then
    Report('DoUnkit', Err);
  if HasMarker(OldDir('shim'), 'fm-dialog-shim') and not RunScript('uninstall-shim.ps1', '-InstallDir ' + PsQuote(OldDir('shim')), Err) then
    Report('DoUnshim', Err);
  if HasMarker(OldDir('app'), 'fm') then
    DelTree(OldDir('app'), True, True, True);
  RemoveDir(ExpandConstant('{localappdata}\Programs\fm'));

  if WizardIsTaskSelected('defaultfm') then begin
    FailedDefault := not RunFm('--set-default', Err);
    if FailedDefault then
      Report('DoDefault', Err);
  end else if not RunFm('--restore-explorer', Err) then
    Report('DoRestore', Err);

  if WizardIsTaskSelected('dialogkit') then begin
    if not RunScript('install-dialogkit.ps1', '-SourceDir ' + PsQuote(ExpandConstant('{app}\setup\dialogkit')) + ' -InstallDir ' + PsQuote(Kit), Err) then
      Report('DoKit', Err);
  end else if DirExists(Kit) and not RunScript('uninstall-dialogkit.ps1', '-InstallDir ' + PsQuote(Kit), Err) then
    Report('DoUnkit', Err);

  if WizardIsTaskSelected('shim') then begin
    if not RunScript('install-shim.ps1', '-SourceDir ' + PsQuote(ExpandConstant('{app}\setup\shim')) + ' -InstallDir ' + PsQuote(Shim), Err) then
      Report('DoShim', Err);
  end else if DirExists(Shim) and not RunScript('uninstall-shim.ps1', '-InstallDir ' + PsQuote(Shim), Err) then
    Report('DoUnshim', Err);

  { Resident from now on, as after each sign-in (the Run value). Not SW_HIDE: Windows would apply
    it to the first window this process shows later. }
  Exec(Exe, '--background', ExpandConstant('{sys}'), SW_SHOWNORMAL, ewNoWait, Code);
end;

{ --- Uninstall --- }

function InitializeUninstall: Boolean;
begin
  Result := not ElevatedSplitToken;
  if not Result then begin
    SuppressibleMsgBox(CustomMessage('Elevated'), mbCriticalError, MB_OK, IDOK);
    Exit;
  end;
  Result := QuitFm(ExpandConstant('{app}\fm.exe'));
  if not Result then
    SuppressibleMsgBox(CustomMessage('UninstBusy'), mbError, MB_OK, IDOK);
end;

{ After fm --restore-explorer, or instead of it when fm.exe cannot run: each verb that still
  opens this fm.exe is deleted, with the "open" default it needed, and so is the Redirect value. }
procedure RemoveLeftoverVerbs(const Exe: String);
var
  Verbs, Shells: array[0..7] of String;
  I: Integer;
  Cmd, Def: String;
begin
  Verbs[0] := 'Directory\shell\open';  Shells[0] := 'Directory\shell';
  Verbs[1] := 'Drive\shell\open';      Shells[1] := 'Drive\shell';
  Verbs[2] := 'Folder\shell\explore';  Shells[2] := '';
  Verbs[3] := 'CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}\shell\opennewwindow';  Shells[3] := '';
  Verbs[4] := 'CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}\shell\open';  Shells[4] := '';
  Verbs[5] := 'CLSID\{20D04FE0-3AEA-1069-A2D8-08002B30309D}\shell\open';  Shells[5] := 'CLSID\{20D04FE0-3AEA-1069-A2D8-08002B30309D}\shell';
  Verbs[6] := 'CLSID\{645FF040-5081-101B-9F08-00AA002F954E}\shell\open';  Shells[6] := 'CLSID\{645FF040-5081-101B-9F08-00AA002F954E}\shell';
  Verbs[7] := 'CLSID\{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}\shell\open';  Shells[7] := 'CLSID\{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}\shell';
  for I := 0 to 7 do begin
    if RegQueryStringValue(HKCU, 'Software\Classes\' + Verbs[I] + '\command', '', Cmd)
      and (Pos(Lowercase(Exe), Lowercase(Cmd)) > 0) then begin
      Log('Leftover verb removed: ' + Verbs[I]);
      RegDeleteKeyIncludingSubkeys(HKCU, 'Software\Classes\' + Verbs[I]);
      if (Shells[I] <> '') and RegQueryStringValue(HKCU, 'Software\Classes\' + Shells[I], '', Def)
        and (Def = 'open') and not RegKeyExists(HKCU, 'Software\Classes\' + Shells[I] + '\open') then
        RegDeleteValue(HKCU, 'Software\Classes\' + Shells[I], '');
    end;
  end;
  if RegQueryStringValue(HKCU, 'Software\fm', 'Redirect', Cmd) and SameText(Cmd, Exe) then
    RegDeleteValue(HKCU, 'Software\fm', 'Redirect');
end;

procedure DeleteData;
var
  D: String;
begin
  D := ExpandConstant('{localappdata}\fm');
  if not DirExists(D) then
    Exit;
  if SuppressibleMsgBox(CustomMessage('UninstData'), mbConfirmation, MB_YESNO or MB_DEFBUTTON2, IDNO) <> IDYES then
    Exit;
  { Only what fm.exe writes; dialogkit keeps its own files there. }
  DeleteFile(D + '\prefs.txt');
  DeleteFile(D + '\prefs.tmp');
  DeleteFile(D + '\tags.tsv');
  DeleteFile(D + '\tags.tmp');
  DeleteFile(D + '\fm.log');
  DelTree(D + '\index', True, True, True);
  DelTree(D + '\index-dev', True, True, True);
  RemoveDir(D);
end;

const
  { CLSID_FileOpenDialog's per-user override (install-shim.ps1). }
  OpenDialogKey = 'Software\Classes\CLSID\{DC1C5A9C-E88A-4dde-A5A1-60F82A20AEF7}';

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Err, Kit, Cmd: String;
begin
  if CurUninstallStep = usUninstall then begin
    if not RunFm('--restore-explorer', Err) then
      Log('fm --restore-explorer failed: ' + Err);
    RemoveLeftoverVerbs(ExpandConstant('{app}\fm.exe'));
    Kit := ExpandConstant('{app}\dialogkit');
    if DirExists(Kit) and not RunScript('uninstall-dialogkit.ps1', '-InstallDir ' + PsQuote(Kit), Err) then
      Log('uninstall-dialogkit.ps1 failed: ' + Err);
    { The Open dialog goes back to Windows before fm.exe, its picker, is deleted. }
    Kit := ExpandConstant('{app}\shim');
    if DirExists(Kit) and not RunScript('uninstall-shim.ps1', '-InstallDir ' + PsQuote(Kit), Err) then
      Log('uninstall-shim.ps1 failed: ' + Err);
    { Even when the script could not run (blocked PowerShell) or the folder is gone: an override
      left pointing at a DLL nobody owns any more would break every app's Open dialog. }
    if RegQueryStringValue(HKCU, OpenDialogKey + '\InprocServer32', '', Cmd)
      and (Pos(Lowercase(Kit + '\'), Lowercase(Cmd)) = 1) then begin
      Log('Open dialog override removed by setup: ' + Cmd);
      RegDeleteKeyIncludingSubkeys(HKCU, OpenDialogKey);
    end;
  end;
  if CurUninstallStep = usPostUninstall then
    DeleteData;
end;
