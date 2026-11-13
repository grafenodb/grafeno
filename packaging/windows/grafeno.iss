; Grafeno for Windows: the installer of the engine (Inno Setup 6).
;
;   iscc /DAppVersion=0.2.1 packaging\windows\grafeno.iss
;
; It needs target\release\grafeno.exe (cargo build --release -p
; grafeno-server). The engine alone: Grafeno Studio is installed apart.
; The installer writes dist\grafeno-<version>-windows-x64-setup.exe.
;
; A guided setup: the program folder, the data folder,
; the port, whether other computers may connect (with a firewall rule), the
; account the service runs as, the first administrator, the service and the
; PATH. Unattended, the same answers are parameters:
;
;   grafeno-<version>-windows-x64-setup.exe /VERYSILENT /SUPPRESSMSGBOXES
;     /DIR="C:\Grafeno" /DATADIR="D:\GrafenoData" /PORT=7810 /NETWORK=0
;     /ACCOUNT=virtual /ADMIN=admin /ADMINPASSWORD=... /TASKS="service,path"
;
; /ACCOUNT=virtual (the default) runs the service as NT SERVICE\Grafeno, a
; virtual account that may use only Grafeno's folders; /ACCOUNT=system runs
; it as LocalSystem. Upgrades (running a newer setup over an older one) keep
; the settings, the data and the service's account; uninstalling keeps the
; settings and the data (C:\ProgramData\Grafeno and the data folder) unless
; asked to remove them (unattended: unins000.exe /VERYSILENT /PURGE=1).
;
; Signed builds: iscc /DSign "/Ssigntool=signtool sign /f cert.pfx /p ...
; /fd sha256 /tr http://timestamp.digicert.com /td sha256 $f" signs the setup
; and its uninstaller.

#ifndef AppVersion
  #define AppVersion "0.2.1"
#endif
#define SettingsDir "{commonappdata}\Grafeno"
#define ConfigFile "{commonappdata}\Grafeno\grafeno.toml"

[Setup]
AppId={{5B0D1C8E-6C2A-4E7B-9F3A-0E8B6A1C4D27}
AppName=Grafeno
AppVersion={#AppVersion}
AppVerName=Grafeno {#AppVersion}
AppPublisher=GrafenoDB
AppPublisherURL=https://github.com/grafenodb/grafeno
DefaultDirName={autopf}\Grafeno
DefaultGroupName=Grafeno
DisableProgramGroupPage=yes
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\..\dist
OutputBaseFilename=grafeno-{#AppVersion}-windows-x64-setup
Compression=lzma2/max
SolidCompression=yes
ChangesEnvironment=yes
WizardStyle=modern
SetupIconFile=grafeno.ico
LicenseFile=license.txt
UninstallDisplayIcon={app}\grafeno.exe
UninstallDisplayName=Grafeno {#AppVersion}
SetupLogging=yes
CloseApplications=no
#ifdef Sign
SignTool=signtool
SignedUninstaller=yes
#endif

[Tasks]
Name: service; Description: "Run Grafeno as a Windows service (it starts with Windows)"
Name: path; Description: "Add grafeno to the system PATH (to use it from any terminal)"

[Files]
Source: "..\..\target\release\grafeno.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\README.md"; DestDir: "{app}"; DestName: "README.md"; Flags: ignoreversion
Source: "..\..\LICENSE-MIT"; DestDir: "{app}"; DestName: "LICENSE-MIT.txt"; Flags: ignoreversion
Source: "..\..\LICENSE-APACHE"; DestDir: "{app}"; DestName: "LICENSE-APACHE.txt"; Flags: ignoreversion
Source: "..\..\docs\install.md"; DestDir: "{app}\docs"; Flags: ignoreversion
Source: "..\..\docs\query-language.md"; DestDir: "{app}\docs"; Flags: ignoreversion

[Dirs]
Name: "{#SettingsDir}"
Name: "{code:DataDir}"; Flags: uninsneveruninstall

[Icons]
Name: "{group}\Grafeno Shell"; Filename: "{app}\grafeno.exe"; Parameters: "shell --config ""{#ConfigFile}"""; Comment: "GQ console connected to the local service"; Tasks: service
Name: "{group}\Grafeno settings"; Filename: "{sys}\notepad.exe"; Parameters: """{#ConfigFile}"""; Comment: "grafeno.toml: address, token, limits"
Name: "{group}\Uninstall Grafeno"; Filename: "{uninstallexe}"

[Registry]
Root: HKLM; Subkey: "SYSTEM\CurrentControlSet\Control\Session Manager\Environment"; ValueType: expandsz; ValueName: "Path"; ValueData: "{olddata};{app}"; Tasks: path; Check: NeedsAddPath(ExpandConstant('{app}'))
; The Event Log's source for the service's events (grafeno.exe holds the
; message table).
Root: HKLM; Subkey: "SYSTEM\CurrentControlSet\Services\EventLog\Application\Grafeno"; ValueType: expandsz; ValueName: "EventMessageFile"; ValueData: "{app}\grafeno.exe"; Flags: uninsdeletekey
Root: HKLM; Subkey: "SYSTEM\CurrentControlSet\Services\EventLog\Application\Grafeno"; ValueType: dword; ValueName: "TypesSupported"; ValueData: "7"

[Run]
; Settings with a fresh token, only on the first install (upgrades keep them).
Filename: "{app}\grafeno.exe"; Parameters: "config init --path ""{#ConfigFile}"" --db ""{code:DataDir}\grafeno.redb"" --addr {code:Address}"; Flags: runhidden waituntilterminated; StatusMsg: "Creating the settings and token..."; Check: not FileExists(ExpandConstant('{#ConfigFile}'))
; The first administrator, before the service holds the database. The
; password reaches it on its standard input, from a file deleted right after.
Filename: "{cmd}"; Parameters: "/c """"{app}\grafeno.exe"" users create ""{code:AdminName}"" --superuser --db ""{code:DataDir}\grafeno.redb"" --password-stdin < ""{tmp}\admin.txt"""""; Flags: runhidden waituntilterminated; StatusMsg: "Creating the first administrator..."; Check: CreatesAdmin; BeforeInstall: WriteAdminPassword; AfterInstall: ForgetAdminPassword
Filename: "{app}\grafeno.exe"; Parameters: "service install --no-start --config ""{#ConfigFile}"" {code:AccountArgument}"; Flags: runhidden waituntilterminated; StatusMsg: "Installing the Grafeno service..."; Tasks: service
; Grafeno's folders are for SYSTEM, the administrators and the service
; only: what they inherited from C:\ProgramData (every user may read it)
; goes, on every install and upgrade. By security identifiers, so that it
; works in every language of Windows.
Filename: "{sys}\icacls.exe"; Parameters: """{#SettingsDir}"" /grant:r *S-1-5-18:(OI)(CI)F *S-1-5-32-544:(OI)(CI)F"; Flags: runhidden waituntilterminated
Filename: "{sys}\icacls.exe"; Parameters: """{#SettingsDir}"" /inheritance:r"; Flags: runhidden waituntilterminated
Filename: "{sys}\icacls.exe"; Parameters: """{code:InstalledDataDir}"" /grant:r *S-1-5-18:(OI)(CI)F *S-1-5-32-544:(OI)(CI)F"; Flags: runhidden waituntilterminated
Filename: "{sys}\icacls.exe"; Parameters: """{code:InstalledDataDir}"" /inheritance:r"; Flags: runhidden waituntilterminated
; The virtual account may use Grafeno's folders, and nothing else.
Filename: "{sys}\icacls.exe"; Parameters: """{#SettingsDir}"" /grant ""NT SERVICE\Grafeno:(OI)(CI)M"""; Flags: runhidden waituntilterminated; Tasks: service; Check: VirtualAccount
; The settings file inherits nothing (grafeno config init makes it private
; to SYSTEM, the administrators and its maker): the service's account is
; named on it too, or the service could not read its settings.
Filename: "{sys}\icacls.exe"; Parameters: """{#ConfigFile}"" /grant ""NT SERVICE\Grafeno:M"""; Flags: runhidden waituntilterminated; Tasks: service; Check: VirtualAccount
Filename: "{sys}\icacls.exe"; Parameters: """{code:InstalledDataDir}"" /grant ""NT SERVICE\Grafeno:(OI)(CI)M"""; Flags: runhidden waituntilterminated; Tasks: service; Check: VirtualAccount
; Other computers may connect: a firewall rule for the port.
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall add rule name=""Grafeno {code:Port}"" dir=in action=allow protocol=TCP localport={code:Port}"; Flags: runhidden waituntilterminated; StatusMsg: "Opening the port in the firewall..."; Check: OpensFirewall
Filename: "{sys}\sc.exe"; Parameters: "start Grafeno"; Flags: runhidden waituntilterminated; StatusMsg: "Starting the Grafeno service..."; Tasks: service

[UninstallRun]
Filename: "{app}\grafeno.exe"; Parameters: "service uninstall"; Flags: runhidden waituntilterminated; RunOnceId: "RemoveService"
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""Grafeno {code:InstalledPort}"""; Flags: runhidden waituntilterminated; RunOnceId: "RemoveFirewallRule"

[Messages]
FinishedLabel=Grafeno is installed.%n%nThe engine is a service: connect to it with its console (Grafeno Shell), its HTTP API, or Grafeno Studio (installed apart). Its token: grafeno config token. Check it with: grafeno doctor.%n%nThe settings (C:\ProgramData\Grafeno) and the data stay if you uninstall, unless you choose to remove them.

[Code]
var
  ServerPage: TInputQueryWizardPage;
  NetworkPage: TInputOptionWizardPage;
  DataPage: TInputDirWizardPage;
  AccountPage: TInputOptionWizardPage;
  AdminPage: TInputQueryWizardPage;
  // The account of the service an earlier install registered ('' if none).
  OldAccount: String;
  // An earlier install's settings were there when the setup started.
  WasInstalled: Boolean;
  // Uninstall: remove the settings and the data too.
  Purge: Boolean;
  PurgeData: String;

function InitializeSetup(): Boolean;
begin
  if not RegQueryStringValue(HKEY_LOCAL_MACHINE,
    'SYSTEM\CurrentControlSet\Services\Grafeno', 'ObjectName', OldAccount) then
    OldAccount := '';
  WasInstalled := FileExists(ExpandConstant('{#ConfigFile}'));
  Result := True;
end;

// Whether an earlier install's settings were there (the first [Run] step
// writes them on a first install).
function Upgrading(): Boolean;
begin
  Result := WasInstalled;
end;

function DefaultDataDir(): String;
begin
  Result := ExpandConstant('{#SettingsDir}\data');
end;

procedure InitializeWizard();
begin
  DataPage := CreateInputDirPage(wpSelectDir, 'Data folder',
    'Where should Grafeno keep its databases?',
    'The service keeps its database here. It stays when Grafeno is uninstalled or upgraded.',
    False, '');
  DataPage.Add('');
  DataPage.Values[0] := ExpandConstant('{param:DATADIR|' + DefaultDataDir() + '}');

  ServerPage := CreateInputQueryPage(DataPage.ID, 'Server',
    'Which port should the engine listen on?',
    'Clients (the console, applications, Grafeno Studio) connect to this port.');
  ServerPage.Add('Port:', False);
  ServerPage.Values[0] := ExpandConstant('{param:PORT|7810}');

  NetworkPage := CreateInputOptionPage(ServerPage.ID, 'Network',
    'Who may connect to the engine?',
    'Listening on the network also adds a firewall rule for the port. Every request needs the token (or an account) either way.',
    True, False);
  NetworkPage.Add('Only this computer (127.0.0.1)');
  NetworkPage.Add('Other computers too (every network interface, firewall rule)');
  if ExpandConstant('{param:NETWORK|0}') = '1' then
    NetworkPage.SelectedValueIndex := 1
  else
    NetworkPage.SelectedValueIndex := 0;

  AccountPage := CreateInputOptionPage(NetworkPage.ID, 'Service account',
    'Which account should the service run as?',
    'A virtual account may use only Grafeno''s folders (the settings and the data); LocalSystem may use the whole computer.',
    True, False);
  AccountPage.Add('NT SERVICE\Grafeno, a virtual account (recommended)');
  AccountPage.Add('LocalSystem');
  if Lowercase(ExpandConstant('{param:ACCOUNT|virtual}')) = 'system' then
    AccountPage.SelectedValueIndex := 1
  else
    AccountPage.SelectedValueIndex := 0;

  AdminPage := CreateInputQueryPage(AccountPage.ID, 'First administrator',
    'Create the first administrator?',
    'A superuser who signs in with a password (in the console, the API or Grafeno Studio). Leave the password empty to skip it: the token in the settings works meanwhile.');
  AdminPage.Add('Name:', False);
  AdminPage.Add('Password (12 characters or more):', True);
  AdminPage.Add('Password again:', True);
  AdminPage.Values[0] := ExpandConstant('{param:ADMIN|admin}');
  AdminPage.Values[1] := ExpandConstant('{param:ADMINPASSWORD|}');
  AdminPage.Values[2] := AdminPage.Values[1];
end;

// An upgrade keeps its settings: the questions about them are skipped.
function ShouldSkipPage(PageID: Integer): Boolean;
begin
  Result := Upgrading() and
    ((PageID = DataPage.ID) or (PageID = ServerPage.ID) or (PageID = NetworkPage.ID)
     or (PageID = AccountPage.ID) or (PageID = AdminPage.ID));
end;

function NextButtonClick(CurPageID: Integer): Boolean;
var
  Value: Integer;
begin
  Result := True;
  if CurPageID = ServerPage.ID then
  begin
    Value := StrToIntDef(Trim(ServerPage.Values[0]), 0);
    if (Value < 1) or (Value > 65535) then
    begin
      MsgBox('The port is a number from 1 to 65535.', mbError, MB_OK);
      Result := False;
    end;
  end;
  if (CurPageID = AdminPage.ID) and (AdminPage.Values[1] <> '') then
  begin
    if AdminPage.Values[1] <> AdminPage.Values[2] then
    begin
      MsgBox('The passwords differ.', mbError, MB_OK);
      Result := False;
    end
    else if Length(AdminPage.Values[1]) < 12 then
    begin
      MsgBox('The password needs 12 characters or more.', mbError, MB_OK);
      Result := False;
    end
    else if Trim(AdminPage.Values[0]) = '' then
    begin
      MsgBox('Give the administrator a name.', mbError, MB_OK);
      Result := False;
    end;
  end;
end;

function DataDir(Param: String): String;
begin
  Result := Trim(DataPage.Values[0]);
  if Result = '' then
    Result := DefaultDataDir();
end;

function AdminName(Param: String): String;
begin
  Result := Trim(AdminPage.Values[0]);
end;

// Only on a first install (no database yet), and only with a password.
function CreatesAdmin(): Boolean;
begin
  Result := (not Upgrading()) and (AdminPage.Values[1] <> '') and (AdminName('') <> '');
end;

procedure WriteAdminPassword();
begin
  SaveStringToFile(ExpandConstant('{tmp}\admin.txt'), AdminPage.Values[1] + #13#10, False);
end;

procedure ForgetAdminPassword();
begin
  DeleteFile(ExpandConstant('{tmp}\admin.txt'));
end;

// An upgrade keeps the account the service had; a first install takes
// the page's choice.
function VirtualAccount(): Boolean;
begin
  if OldAccount <> '' then
    Result := CompareText(OldAccount, 'NT SERVICE\Grafeno') = 0
  else
    Result := AccountPage.SelectedValueIndex = 0;
end;

function AccountArgument(Param: String): String;
begin
  if VirtualAccount() then
    Result := '--account "NT SERVICE\Grafeno"'
  else
    Result := '';
end;

// The data folder of the installed settings (upgrades, the uninstaller),
// else the page's. The uninstaller has no wizard pages: DataPage is nil
// there, so it falls back to the default folder.
function InstalledDataDir(Param: String): String;
var
  Lines: TArrayOfString;
  I, Slash: Integer;
  Line: String;
  Quoted: Boolean;
begin
  if IsUninstaller() then
    Result := DefaultDataDir()
  else
    Result := DataDir('');
  if not LoadStringsFromFile(ExpandConstant('{#ConfigFile}'), Lines) then
    exit;
  for I := 0 to GetArrayLength(Lines) - 1 do
  begin
    Line := Trim(Lines[I]);
    // db = 'C:\ProgramData\Grafeno\data\grafeno.redb' (TOML writes
    // Windows paths as literal strings) or db = "C:\\...\\grafeno.redb":
    // its folder.
    if (Pos('db ', Line) = 1) and ((Pos('''', Line) > 0) or (Pos('"', Line) > 0)) then
    begin
      Quoted := Pos('''', Line) > 0;
      if Quoted then
        Line := Copy(Line, Pos('''', Line) + 1, Length(Line))
      else
        Line := Copy(Line, Pos('"', Line) + 1, Length(Line));
      Delete(Line, Length(Line), 1);
      StringChangeEx(Line, '/', '\', True);
      if not Quoted then
        StringChangeEx(Line, '\\', '\', True);
      Slash := Length(Line);
      while (Slash > 0) and (Line[Slash] <> '\') do
        Slash := Slash - 1;
      if Slash > 0 then
        Result := Copy(Line, 1, Slash - 1);
      break;
    end;
  end;
end;

function Port(Param: String): String;
begin
  Result := IntToStr(StrToIntDef(Trim(ServerPage.Values[0]), 7810));
end;

function Address(Param: String): String;
begin
  if NetworkPage.SelectedValueIndex = 1 then
    Result := '0.0.0.0:' + Port('')
  else
    Result := '127.0.0.1:' + Port('');
end;

function OpensFirewall(): Boolean;
begin
  Result := (not Upgrading()) and (NetworkPage.SelectedValueIndex = 1);
end;

// The port in the installed settings (for the firewall rule's name).
function InstalledPort(Param: String): String;
var
  Lines: TArrayOfString;
  I, J: Integer;
  Line: String;
begin
  Result := '7810';
  if not LoadStringsFromFile(ExpandConstant('{#ConfigFile}'), Lines) then
    exit;
  for I := 0 to GetArrayLength(Lines) - 1 do
  begin
    Line := Trim(Lines[I]);
    if Pos('addr', Line) = 1 then
      for J := Length(Line) downto 1 do
        if Line[J] = ':' then
        begin
          // addr = "127.0.0.1:7810": what follows the last colon, without
          // the closing quote.
          Result := Copy(Line, J + 1, Length(Line) - J - 1);
          break;
        end;
  end;
end;

function NeedsAddPath(Dir: string): Boolean;
var
  Path: string;
begin
  if not RegQueryStringValue(HKEY_LOCAL_MACHINE,
    'SYSTEM\CurrentControlSet\Control\Session Manager\Environment', 'Path', Path) then
  begin
    Result := True;
    exit;
  end;
  Result := Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Path) + ';') = 0;
end;

// Upgrades: stop and remove the old service so its binary can be replaced.
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code: Integer;
  OldExe: string;
begin
  Result := '';
  OldExe := ExpandConstant('{app}\grafeno.exe');
  if FileExists(OldExe) then
    Exec(OldExe, 'service uninstall', '', SW_HIDE, ewWaitUntilTerminated, Code);
end;

// Uninstall: ask whether to remove the settings and the data too (no by
// default; /PURGE=1 unattended), and take the install folder out of the
// system PATH.
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Path, Dir: string;
  At: Integer;
begin
  if CurUninstallStep = usUninstall then
  begin
    PurgeData := InstalledDataDir('');
    if UninstallSilent() then
      Purge := ExpandConstant('{param:PURGE|0}') = '1'
    else
      Purge := MsgBox('Also remove the settings and the data (' + PurgeData +
        ')? This deletes the databases and cannot be undone.', mbConfirmation,
        MB_YESNO or MB_DEFBUTTON2) = IDYES;
    exit;
  end;
  if CurUninstallStep <> usPostUninstall then
    exit;
  if Purge then
  begin
    // The stopped service's process may hold the database a moment longer.
    Sleep(2000);
    DelTree(PurgeData, True, True, True);
    DelTree(ExpandConstant('{#SettingsDir}'), True, True, True);
  end;
  if not RegQueryStringValue(HKEY_LOCAL_MACHINE,
    'SYSTEM\CurrentControlSet\Control\Session Manager\Environment', 'Path', Path) then
    exit;
  Dir := ExpandConstant('{app}');
  At := Pos(';' + Uppercase(Dir), Uppercase(Path));
  if At > 0 then
  begin
    Delete(Path, At, Length(Dir) + 1);
    RegWriteExpandStringValue(HKEY_LOCAL_MACHINE,
      'SYSTEM\CurrentControlSet\Control\Session Manager\Environment', 'Path', Path);
  end;
end;
