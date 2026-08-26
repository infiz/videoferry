param(
    [string]$InnoCompiler = $env:VIDEOFERRY_INNO_COMPILER,
    [string]$PackageDir = '',
    [switch]$KeepArtifacts
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$workspaceRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if ([string]::IsNullOrWhiteSpace($PackageDir)) {
    $PackageDir = Join-Path $workspaceRoot 'dist\windows\VideoFerry'
}
$PackageDir = (Resolve-Path -LiteralPath $PackageDir).Path
if (-not (Test-Path -LiteralPath (Join-Path $PackageDir 'VideoFerry.exe') -PathType Leaf)) {
    throw "Packaged application is missing: $PackageDir"
}
if ([string]::IsNullOrWhiteSpace($InnoCompiler)) {
    $localCompiler = Join-Path $workspaceRoot '.local\inno-setup\ISCC.exe'
    $command = Get-Command ISCC.exe -ErrorAction SilentlyContinue
    if ($null -ne $command) {
        $InnoCompiler = $command.Source
    } elseif (Test-Path -LiteralPath $localCompiler -PathType Leaf) {
        $InnoCompiler = $localCompiler
    } else {
        throw 'ISCC.exe was not found; pass -InnoCompiler or set VIDEOFERRY_INNO_COMPILER.'
    }
}
$InnoCompiler = (Resolve-Path -LiteralPath $InnoCompiler).Path

function Assert-WorkspaceChild([string]$Path) {
    $absolute = [IO.Path]::GetFullPath($Path)
    $prefix = $workspaceRoot.TrimEnd('\') + '\'
    if (-not $absolute.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing installer-lifecycle access outside the Rust workspace: $absolute"
    }
}

function Invoke-HiddenProcess([string]$FilePath, [string]$Arguments, [string]$Description) {
    $process = Start-Process -FilePath $FilePath -ArgumentList $Arguments -Wait -PassThru -WindowStyle Hidden
    if ($process.ExitCode -ne 0) {
        throw "$Description failed with exit code $($process.ExitCode)"
    }
}

$runsRoot = Join-Path $workspaceRoot '.local\installer-lifecycle'
New-Item -ItemType Directory -Path $runsRoot -Force | Out-Null
$runId = [guid]::NewGuid().ToString('N')
$runRoot = Join-Path $runsRoot $runId
$installDir = Join-Path $runRoot 'installed'
$installerOutput = Join-Path $runRoot 'installers'
$firstPackage = Join-Path $runRoot 'package-1'
$secondPackage = Join-Path $runRoot 'package-2'
foreach ($path in @($runRoot, $installDir, $installerOutput, $firstPackage, $secondPackage)) {
    Assert-WorkspaceChild $path
}
New-Item -ItemType Directory -Path $installerOutput, $firstPackage, $secondPackage -Force | Out-Null

Get-ChildItem -LiteralPath $PackageDir -Force | Copy-Item -Destination $firstPackage -Recurse -Force
Get-ChildItem -LiteralPath $PackageDir -Force | Copy-Item -Destination $secondPackage -Recurse -Force
[IO.File]::WriteAllText((Join-Path $firstPackage 'lifecycle-version.txt'), 'first')
[IO.File]::WriteAllText((Join-Path $secondPackage 'lifecycle-version.txt'), 'second')

$guid = [guid]::NewGuid().ToString().ToUpperInvariant()
$installerAppId = "{{$guid}"
$registryAppId = "{$guid}"
$appName = "VideoFerry Lifecycle $($runId.Substring(0, 8))"
$registryKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\$($registryAppId)_is1"
$shortcut = Join-Path ([Environment]::GetFolderPath('Programs')) "$appName.lnk"
$desktopShortcut = Join-Path ([Environment]::GetFolderPath('Desktop')) "$appName.lnk"
$uninstaller = Join-Path $installDir 'unins000.exe'
$installedExecutable = Join-Path $installDir 'VideoFerry.exe'
$userFile = Join-Path $installDir 'user-preserved.txt'

$savedEnvironment = @{}
foreach ($name in @(
    'VIDEOFERRY_PACKAGE_DIR',
    'VIDEOFERRY_INSTALLER_OUTPUT',
    'VIDEOFERRY_INSTALLER_APP_ID',
    'VIDEOFERRY_INSTALLER_APP_NAME',
    'VIDEOFERRY_INSTALLER_APP_VERSION',
    'VIDEOFERRY_INSTALLER_DEFAULT_DIR',
    'VIDEOFERRY_INSTALLER_BASE_FILENAME',
    'VIDEOFERRY_INSTALLER_CREATE_ICONS',
    'VIDEOFERRY_INSTALLER_COMPRESSION',
    'VIDEOFERRY_INSTALLER_SOLID_COMPRESSION',
    'VIDEOFERRY_RUNTIME_REPORT_PATH'
)) {
    $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}

function Build-TestInstaller([string]$Source, [string]$Version, [string]$BaseFilename) {
    $env:VIDEOFERRY_PACKAGE_DIR = $Source
    $env:VIDEOFERRY_INSTALLER_OUTPUT = $installerOutput
    $env:VIDEOFERRY_INSTALLER_APP_ID = $installerAppId
    $env:VIDEOFERRY_INSTALLER_APP_NAME = $appName
    $env:VIDEOFERRY_INSTALLER_APP_VERSION = $Version
    $env:VIDEOFERRY_INSTALLER_DEFAULT_DIR = $installDir
    $env:VIDEOFERRY_INSTALLER_BASE_FILENAME = $BaseFilename
    $env:VIDEOFERRY_INSTALLER_CREATE_ICONS = '0'
    $env:VIDEOFERRY_INSTALLER_COMPRESSION = 'zip'
    $env:VIDEOFERRY_INSTALLER_SOLID_COMPRESSION = 'no'
    & $InnoCompiler (Join-Path $workspaceRoot 'packaging\windows\VideoFerry.iss') | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "Inno Setup failed for $Version"
    }
    $result = Join-Path $installerOutput "$BaseFilename.exe"
    if (-not (Test-Path -LiteralPath $result -PathType Leaf)) {
        throw "Inno Setup produced no installer for $Version"
    }
    return $result
}

function Assert-InstalledVersion([string]$ExpectedMarker, [string]$ExpectedVersion) {
    if (-not (Test-Path -LiteralPath $installedExecutable -PathType Leaf)) {
        throw 'Installed executable is missing'
    }
    $actualMarker = [IO.File]::ReadAllText((Join-Path $installDir 'lifecycle-version.txt'))
    if ($actualMarker -ne $ExpectedMarker) {
        throw "Installed marker mismatch (expected '$ExpectedMarker', got '$actualMarker')"
    }
    if (-not (Test-Path -LiteralPath $registryKey)) {
        throw "Isolated uninstall registry key is missing: $registryKey"
    }
    $displayVersion = (Get-ItemProperty -LiteralPath $registryKey).DisplayVersion
    if ($displayVersion -ne $ExpectedVersion) {
        throw "Installed version mismatch (expected '$ExpectedVersion', got '$displayVersion')"
    }
    $runtimeReportPath = Join-Path $runRoot "runtime-$ExpectedMarker.txt"
    $originalPath = $env:PATH
    try {
        $env:PATH = "$(Join-Path $env:SystemRoot 'System32');$env:SystemRoot"
        $env:VIDEOFERRY_RUNTIME_REPORT_PATH = $runtimeReportPath
        Invoke-HiddenProcess $installedExecutable '--verify-runtime' 'Installed direct runtime verification'
    } finally {
        $env:PATH = $originalPath
    }
    $runtimeReport = [IO.File]::ReadAllText($runtimeReportPath)
    if ($runtimeReport -notmatch '(?m)^runtime=ok$' -or $runtimeReport -notmatch '(?m)^engine=FFmpeg 9\.0\.1.*GPL') {
        throw "Installed direct runtime report is invalid: $runtimeReport"
    }
}

$installed = $false
try {
    $firstInstaller = Build-TestInstaller $firstPackage '0.1.0-lifecycle.1' 'VideoFerryLifecycle-1'
    $secondInstaller = Build-TestInstaller $secondPackage '0.1.0-lifecycle.2' 'VideoFerryLifecycle-2'
    $installArguments = '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /NOICONS /DIR="{0}" /LOG="{1}"'

    Invoke-HiddenProcess $firstInstaller ($installArguments -f $installDir, (Join-Path $runRoot 'install-1.log')) 'Isolated installer'
    $installed = $true
    Assert-InstalledVersion 'first' '0.1.0-lifecycle.1'
    [IO.File]::WriteAllText($userFile, 'preserve across upgrade')

    Invoke-HiddenProcess $secondInstaller ($installArguments -f $installDir, (Join-Path $runRoot 'install-2.log')) 'Isolated upgrade'
    Assert-InstalledVersion 'second' '0.1.0-lifecycle.2'
    if ([IO.File]::ReadAllText($userFile) -ne 'preserve across upgrade') {
        throw 'Upgrade did not preserve an unrelated user file'
    }
    Remove-Item -LiteralPath $userFile -Force

    if ((Test-Path -LiteralPath $shortcut) -or (Test-Path -LiteralPath $desktopShortcut)) {
        throw 'Silent /NOICONS lifecycle test unexpectedly created a shortcut'
    }
    Invoke-HiddenProcess $uninstaller '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART' 'Isolated uninstall'
    $installed = $false
    if ((Test-Path -LiteralPath $installedExecutable) -or (Test-Path -LiteralPath $registryKey)) {
        throw 'Isolated uninstall left the executable or uninstall registration behind'
    }
    Write-Output "Isolated Windows install, upgrade, runtime, and uninstall lifecycle passed: $runRoot"
} finally {
    if ($installed -and (Test-Path -LiteralPath $uninstaller -PathType Leaf)) {
        try {
            Invoke-HiddenProcess $uninstaller '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART' 'Lifecycle cleanup uninstall'
        } catch {
            Write-Warning $_
        }
    }
    foreach ($name in $savedEnvironment.Keys) {
        [Environment]::SetEnvironmentVariable($name, $savedEnvironment[$name], 'Process')
    }
    if (-not $KeepArtifacts -and (Test-Path -LiteralPath $runRoot)) {
        $resolvedRunRoot = (Resolve-Path -LiteralPath $runRoot).Path
        Assert-WorkspaceChild $resolvedRunRoot
        if ((Split-Path -Leaf $resolvedRunRoot) -eq $runId) {
            try {
                Remove-Item -LiteralPath $resolvedRunRoot -Recurse -Force
            } catch {
                Write-Warning "Unable to remove lifecycle artifacts: $_"
            }
        }
    }
}
