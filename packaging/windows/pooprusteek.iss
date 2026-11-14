; Установщик Windows: per-user, без прав администратора, один экран выбора папки.
; Сборка: ISCC /DAppVersion=0.2.0 /DBinDir=<папка с бинарниками> /O<out> pooprusteek.iss

#ifndef AppVersion
  #error Pass /DAppVersion=X.Y.Z
#endif
#ifndef BinDir
  #error Pass /DBinDir=<dir with pooprusteek-windows-*.exe>
#endif

#define AppName "Pooprusteek"
#define ExeName "pooprusteek.exe"
#define X64Exe BinDir + "\pooprusteek-windows-x86_64.exe"
#define Arm64Exe BinDir + "\pooprusteek-windows-arm64.exe"

[Setup]
; AppId зашит и в src/update/install_record.rs — менять только вместе.
AppId={{743230F1-F710-4EF9-9F8A-CEE2AAB13D61}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=Aver005
AppPublisherURL=https://github.com/Aver005/pooprusteek
AppSupportURL=https://github.com/Aver005/pooprusteek/issues
AppUpdatesURL=https://github.com/Aver005/pooprusteek/releases
; %LOCALAPPDATA%\Programs — папка пользователя, туда же пишет самообновление.
PrivilegesRequired=lowest
DefaultDirName={autopf}\{#AppName}
UsePreviousAppDir=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Первая установка: папка → установка → готово. Переустановка берёт прежнюю папку
; (иначе старая копия осталась бы первой в PATH) и показывает один экран «Установить».
DisableWelcomePage=yes
DisableDirPage=auto
DisableProgramGroupPage=yes
DisableReadyPage=yes
WizardStyle=modern dynamic windows11
; Иконка и картинка мастера — из assets/branding (исходник icon-c-terminal-cursor.svg).
SetupIconFile=..\..\assets\branding\pooprusteek.ico
WizardSmallImageFile=..\..\assets\branding\wizard-small.png
ShowLanguageDialog=no
ChangesEnvironment=yes
; Мьютекс создаёт приложение (src/update/install_record.rs): установщик просит его закрыть.
AppMutex=PooprusteekRunning
CloseApplications=no
RestartApplications=no
UninstallDisplayName={#AppName}
UninstallDisplayIcon={app}\{#ExeName}
OutputBaseFilename=pooprusteek-setup
Compression=lzma2/ultra64
SolidCompression=yes

[Languages]
Name: "en"; MessagesFile: "compiler:Default.isl"
Name: "ru"; MessagesFile: "compiler:Languages\Russian.isl"

[CustomMessages]
en.DeleteUserData=Also delete your settings, sessions and downloaded model?%n%n%1
ru.DeleteUserData=Удалить также настройки, сессии и скачанную модель?%n%n%1
en.DeleteConfigFiles=Also delete your settings file (config.toml and its backups) in %1?%n%nYour own rules (AGENTS.md), skills and account logins in that folder are kept.
ru.DeleteConfigFiles=Удалить также файл настроек (config.toml и его копии) в %1?%n%nВаши правила (AGENTS.md), скиллы и входы аккаунтов в этой папке останутся.
en.DirNotWritable=Can't write to %1.%n%nChoose a folder in your user profile — updates are installed there too.
ru.DirNotWritable=Нет прав на запись в %1.%n%nВыберите папку в профиле пользователя — туда же ставятся обновления.
en.Downgrade=A newer version (%1) is already installed.%n%nReplace it with the older %2?
ru.Downgrade=Уже установлена более новая версия (%1).%n%nЗаменить её более старой %2?

[Files]
#ifexist Arm64Exe
Source: "{#Arm64Exe}"; DestDir: "{app}"; DestName: "{#ExeName}"; Flags: ignoreversion; Check: IsArm64
Source: "{#X64Exe}"; DestDir: "{app}"; DestName: "{#ExeName}"; Flags: ignoreversion; Check: not IsArm64
#else
Source: "{#X64Exe}"; DestDir: "{app}"; DestName: "{#ExeName}"; Flags: ignoreversion
#endif

[InstallDelete]
; Хвосты самообновления (src/update/swap.rs): `.old`, `.old.<pid>`, `.new`.
Type: files; Name: "{app}\{#ExeName}.old*"
Type: files; Name: "{app}\{#ExeName}.new"

[UninstallDelete]
Type: files; Name: "{app}\{#ExeName}.old*"
Type: files; Name: "{app}\{#ExeName}.new"
Type: dirifempty; Name: "{app}"

[Icons]
; Агент работает в текущей папке — стартуем из профиля, а не из папки установки.
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#ExeName}"; WorkingDir: "{%USERPROFILE}"

[Run]
Filename: "{code:WindowsTerminalPath}"; Parameters: "new-tab --title {#AppName} -d ""{%USERPROFILE}"" ""{app}\{#ExeName}"""; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: postinstall nowait skipifsilent; Check: HasWindowsTerminal
Filename: "{app}\{#ExeName}"; WorkingDir: "{%USERPROFILE}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: postinstall nowait skipifsilent; Check: not HasWindowsTerminal

[Code]
const
  EnvKey = 'Environment';

function WindowsTerminalPath(Param: String): String;
begin
  Result := ExpandConstant('{localappdata}\Microsoft\WindowsApps\wt.exe');
end;

function HasWindowsTerminal: Boolean;
begin
  Result := FileExists(WindowsTerminalPath(''));
end;

function PathHasDir(const Paths, Dir: String): Boolean;
begin
  Result := Pos(';' + Uppercase(Dir) + ';', ';' + Uppercase(Paths) + ';') > 0;
end;

procedure AddToUserPath(const Dir: String);
var
  Paths: String;
begin
  if not RegQueryStringValue(HKCU, EnvKey, 'Path', Paths) then
    Paths := '';
  if PathHasDir(Paths, Dir) then
    Exit;
  if (Paths <> '') and (Paths[Length(Paths)] <> ';') then
    Paths := Paths + ';';
  RegWriteExpandStringValue(HKCU, EnvKey, 'Path', Paths + Dir);
end;

procedure RemoveFromUserPath(const Dir: String);
var
  Paths: String;
  P: Integer;
begin
  if not RegQueryStringValue(HKCU, EnvKey, 'Path', Paths) then
    Exit;
  Paths := ';' + Paths + ';';
  P := Pos(';' + Uppercase(Dir) + ';', Uppercase(Paths));
  if P = 0 then
    Exit;
  Delete(Paths, P, Length(Dir) + 1);
  Paths := Copy(Paths, 2, Length(Paths) - 2);
  RegWriteExpandStringValue(HKCU, EnvKey, 'Path', Paths);
end;

const
  UninstallKey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{743230F1-F710-4EF9-9F8A-CEE2AAB13D61}_is1';

// Самообновление держит DisplayVersion актуальной — по ней ловим установку старой версии.
function InitializeSetup: Boolean;
var
  Installed: String;
  InstalledPacked, OurPacked: Int64;
begin
  Result := True;
  if RegQueryStringValue(HKCU, UninstallKey, 'DisplayVersion', Installed)
     and StrToVersion(Installed, InstalledPacked)
     and StrToVersion('{#AppVersion}', OurPacked)
     and (ComparePackedVersion(InstalledPacked, OurPacked) > 0) then
    Result := SuppressibleMsgBox(FmtMessage(CustomMessage('Downgrade'), [Installed, '{#AppVersion}']),
                                 mbConfirmation, MB_YESNO or MB_DEFBUTTON2, IDNO) = IDYES;
end;

procedure CurPageChanged(CurPageID: Integer);
begin
  // Ready-страницы нет, поэтому установка стартует прямо с экрана папки.
  if CurPageID = wpSelectDir then
    WizardForm.NextButton.Caption := SetupMessage(msgButtonInstall);
end;

// Туда же пишет /update — папка без прав записи сломает и установку, и обновления.
function NextButtonClick(CurPageID: Integer): Boolean;
var
  Dir, Probe, FirstCreated: String;
begin
  Result := True;
  if CurPageID <> wpSelectDir then
    Exit;
  Dir := RemoveBackslashUnlessRoot(WizardDirValue);
  // Самая верхняя папка, которой ещё нет: её и убираем после пробы (ForceDirectories создаёт цепочку).
  FirstCreated := '';
  if not DirExists(Dir) then
  begin
    FirstCreated := Dir;
    while not DirExists(ExtractFileDir(FirstCreated)) and (ExtractFileDir(FirstCreated) <> FirstCreated) do
      FirstCreated := ExtractFileDir(FirstCreated);
  end;
  Probe := AddBackslash(Dir) + '.pooprusteek-write-test';
  Result := ForceDirectories(Dir) and SaveStringToFile(Probe, '', False);
  DeleteFile(Probe);
  if FirstCreated <> '' then
    DelTree(FirstCreated, True, False, True);
  if not Result then
    SuppressibleMsgBox(FmtMessage(CustomMessage('DirNotWritable'), [Dir]), mbError, MB_OK, IDOK);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
    AddToUserPath(ExpandConstant('{app}'));
end;

// Файлы настроек в ~\.pooprusteek. Остальное там (AGENTS.md, скиллы, входы аккаунтов) — ваше.
procedure AskDeleteConfigFiles;
var
  ConfigDir: String;
  Names: array of String;
  I: Integer;
  Found: Boolean;
begin
  ConfigDir := ExpandConstant('{%USERPROFILE}\.pooprusteek');
  SetArrayLength(Names, 3);
  Names[0] := 'config.toml';
  Names[1] := 'config.toml.pre-gateway';
  Names[2] := 'config.toml.lock';
  Found := False;
  for I := 0 to GetArrayLength(Names) - 1 do
    if FileExists(ConfigDir + '\' + Names[I]) then
      Found := True;
  if not Found then
    Exit;
  if MsgBox(FmtMessage(CustomMessage('DeleteConfigFiles'), [ConfigDir]),
            mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
  begin
    for I := 0 to GetArrayLength(Names) - 1 do
      DeleteFile(ConfigDir + '\' + Names[I]);
    // Пустую папку убираем, непустую (ваши правила) RemoveDir не тронет.
    RemoveDir(ConfigDir);
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  NewData, OldData, Shown: String;
begin
  if CurUninstallStep = usUninstall then
    RemoveFromUserPath(ExpandConstant('{app}'));
  if (CurUninstallStep <> usPostUninstall) or UninstallSilent then
    Exit;
  // Данные и кэш живут в локальной папке; прежнее место (до переезда) — Roaming.
  NewData := ExpandConstant('{localappdata}\pooprusteek');
  OldData := ExpandConstant('{userappdata}\pooprusteek');
  Shown := '';
  if DirExists(NewData) then
    Shown := NewData;
  if DirExists(OldData) then
  begin
    if Shown <> '' then
      Shown := Shown + #13#10;
    Shown := Shown + OldData;
  end;
  if (Shown <> '') and
     (MsgBox(FmtMessage(CustomMessage('DeleteUserData'), [Shown]),
             mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES) then
  begin
    if DirExists(NewData) then
      DelTree(NewData, True, True, True);
    if DirExists(OldData) then
      DelTree(OldData, True, True, True);
  end;
  AskDeleteConfigFiles;
end;
