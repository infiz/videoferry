#define PackageDir GetEnv("VIDEOFERRY_PACKAGE_DIR")
#define InstallerOutput GetEnv("VIDEOFERRY_INSTALLER_OUTPUT")
#define InstallerAppId GetEnv("VIDEOFERRY_INSTALLER_APP_ID")
#define InstallerAppName GetEnv("VIDEOFERRY_INSTALLER_APP_NAME")
#define InstallerAppVersion GetEnv("VIDEOFERRY_INSTALLER_APP_VERSION")
#define InstallerDefaultDir GetEnv("VIDEOFERRY_INSTALLER_DEFAULT_DIR")
#define InstallerBaseFilename GetEnv("VIDEOFERRY_INSTALLER_BASE_FILENAME")
#define InstallerCreateIcons GetEnv("VIDEOFERRY_INSTALLER_CREATE_ICONS")
#define InstallerCompression GetEnv("VIDEOFERRY_INSTALLER_COMPRESSION")
#define InstallerSolidCompression GetEnv("VIDEOFERRY_INSTALLER_SOLID_COMPRESSION")
#define InstallerIcon GetEnv("VIDEOFERRY_INSTALLER_ICON")

#if InstallerAppId == ""
    #define InstallerAppId "{{6F49B315-67C3-4D31-B9C8-A13CE3A9A9A8}"
#endif
#if InstallerAppName == ""
    #define InstallerAppName "VideoFerry"
#endif
#if InstallerAppVersion == ""
    #define InstallerAppVersion "1.0.2"
#endif
#if InstallerDefaultDir == ""
    #define InstallerDefaultDir "{localappdata}\Programs\VideoFerry"
#endif
#if InstallerBaseFilename == ""
    #define InstallerBaseFilename "VideoFerrySetup-1.0.2-windows-x86_64"
#endif
#if InstallerCreateIcons == ""
    #define InstallerCreateIcons "1"
#endif
#if InstallerCompression == ""
    #define InstallerCompression "lzma2"
#endif
#if InstallerSolidCompression == ""
    #define InstallerSolidCompression "yes"
#endif

[Setup]
AppId={#InstallerAppId}
AppName={#InstallerAppName}
AppVersion={#InstallerAppVersion}
AppPublisher=VideoFerry contributors
DefaultDirName={#InstallerDefaultDir}
DefaultGroupName={#InstallerAppName}
DisableProgramGroupPage=yes
OutputDir={#InstallerOutput}
OutputBaseFilename={#InstallerBaseFilename}
Compression={#InstallerCompression}
SolidCompression={#InstallerSolidCompression}
WizardStyle=modern
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
UninstallDisplayIcon={app}\VideoFerry.exe
#if InstallerIcon != ""
SetupIconFile={#InstallerIcon}
#endif

[Files]
Source: "{#PackageDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

#if InstallerCreateIcons == "1"
[Icons]
Name: "{autoprograms}\{#InstallerAppName}"; Filename: "{app}\VideoFerry.exe"
Name: "{autodesktop}\{#InstallerAppName}"; Filename: "{app}\VideoFerry.exe"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Additional shortcuts:"; Flags: unchecked
#endif

[Run]
Filename: "{app}\VideoFerry.exe"; Description: "Launch VideoFerry"; Flags: nowait postinstall skipifsilent
