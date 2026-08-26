[CmdletBinding()]
param(
    [string]$FfmpegSdk = $env:FFMPEG_DIR,
    [string]$LibClang = $env:LIBCLANG_PATH,
    [string]$InnoCompiler = $env:VIDEOFERRY_INNO_COMPILER,
    [string]$SignTool = $env:VIDEOFERRY_WINDOWS_SIGNTOOL,
    [string]$SigningThumbprint = $env:VIDEOFERRY_WINDOWS_SIGNING_CERT_THUMBPRINT,
    [string]$TimestampUrl = $env:VIDEOFERRY_WINDOWS_SIGNING_TIMESTAMP_URL,
    [switch]$SkipFfmpegSdkInstall,
    [switch]$SkipSmoke
)

$ErrorActionPreference = "Stop"

function Enter-WindowsPackageBuildLock([string]$WorkspaceRoot) {
    $lockDirectory = Join-Path $WorkspaceRoot ".local\build-locks"
    New-Item -ItemType Directory -Path $lockDirectory -Force | Out-Null
    $lockPath = Join-Path $lockDirectory "windows-package.lock"
    $deadline = [DateTime]::UtcNow.AddMinutes(15)
    $reportedWait = $false
    while ($true) {
        try {
            return [IO.File]::Open(
                $lockPath,
                [IO.FileMode]::OpenOrCreate,
                [IO.FileAccess]::ReadWrite,
                [IO.FileShare]::None
            )
        } catch [IO.IOException] {
            if (-not $reportedWait) {
                Write-Host "Another Windows package build is running; waiting for it to finish..."
                $reportedWait = $true
            }
            if ([DateTime]::UtcNow -ge $deadline) {
                throw "Timed out waiting for the Windows package build lock: $lockPath"
            }
            Start-Sleep -Milliseconds 500
        }
    }
}

if ($env:OS -ne "Windows_NT") {
    throw "The Windows package and installer must be built on Windows."
}

$workspaceRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot "..")).Path
$workspaceManifest = [IO.File]::ReadAllText((Join-Path $workspaceRoot 'Cargo.toml'))
$appVersionMatch = [Regex]::Match($workspaceManifest, '(?m)^version\s*=\s*"([^"]+)"\s*$')
if (-not $appVersionMatch.Success) {
    throw "Unable to read the application version from the workspace Cargo.toml"
}
$appVersion = $appVersionMatch.Groups[1].Value
$sdkInstaller = Join-Path $workspaceRoot "packaging\windows\install-ffmpeg-sdk.ps1"
$packageBuilder = Join-Path $workspaceRoot "packaging\windows\build.ps1"

function Resolve-InnoCompiler([string]$RequestedCompiler) {
    if (-not [string]::IsNullOrWhiteSpace($RequestedCompiler)) {
        $resolved = (Resolve-Path -LiteralPath $RequestedCompiler).Path
        if (-not (Test-Path -LiteralPath $resolved -PathType Leaf)) {
            throw "Inno Setup compiler is not a file: $resolved"
        }
        return $resolved
    }

    $command = Get-Command ISCC.exe -ErrorAction SilentlyContinue
    if ($null -ne $command) {
        return $command.Source
    }

    $candidates = @(
        (Join-Path $workspaceRoot ".local\inno-setup\ISCC.exe"),
        (Join-Path ${env:ProgramFiles(x86)} "Inno Setup 6\ISCC.exe"),
        (Join-Path $env:ProgramFiles "Inno Setup 6\ISCC.exe"),
        (Join-Path $env:LOCALAPPDATA "Programs\Inno Setup 6\ISCC.exe")
    )
    foreach ($candidate in $candidates) {
        if (-not [string]::IsNullOrWhiteSpace($candidate) -and
            (Test-Path -LiteralPath $candidate -PathType Leaf)) {
            return (Resolve-Path -LiteralPath $candidate).Path
        }
    }

    throw @"
Inno Setup 6 (ISCC.exe) was not found. Install Inno Setup or pass
-InnoCompiler C:\path\to\ISCC.exe. The installer is required by this script.
"@
}

function Get-PinnedFfmpegSdkPath {
    $manifestPath = Join-Path $workspaceRoot "engine-manifest.toml"
    $manifest = [IO.File]::ReadAllText($manifestPath)
    $windowsSection = [Regex]::Match(
        $manifest,
        '(?ms)^\[windows_x86_64\]\s*(.*?)(?=^\[|\z)'
    )
    if (-not $windowsSection.Success) {
        throw "Missing [windows_x86_64] in $manifestPath"
    }
    $runtimeDirectory = [Regex]::Match(
        $windowsSection.Groups[1].Value,
        '(?m)^\s*runtime_directory\s*=\s*"([^"]+)"\s*$'
    )
    if (-not $runtimeDirectory.Success) {
        throw "Missing Windows runtime_directory in $manifestPath"
    }
    return [IO.Path]::GetFullPath((Join-Path $workspaceRoot $runtimeDirectory.Groups[1].Value))
}

$resolvedInnoCompiler = Resolve-InnoCompiler $InnoCompiler

if ([string]::IsNullOrWhiteSpace($FfmpegSdk)) {
    $FfmpegSdk = Get-PinnedFfmpegSdkPath
    $sdkMarker = Join-Path $FfmpegSdk "bin\avcodec-63.dll"
    if (-not $SkipFfmpegSdkInstall -and
        -not (Test-Path -LiteralPath $sdkMarker -PathType Leaf)) {
        Write-Host "Installing the pinned Windows FFmpeg SDK..."
        & $sdkInstaller
        if ($LASTEXITCODE -ne 0) {
            throw "Pinned FFmpeg SDK installation failed."
        }
    }
}

$buildArguments = @{
    FfmpegSdk = $FfmpegSdk
    InnoCompiler = $resolvedInnoCompiler
    SkipSmoke = $SkipSmoke
}
if (-not [string]::IsNullOrWhiteSpace($LibClang)) {
    $buildArguments.LibClang = $LibClang
}
if (-not [string]::IsNullOrWhiteSpace($SignTool)) {
    $buildArguments.SignTool = $SignTool
}
if (-not [string]::IsNullOrWhiteSpace($SigningThumbprint)) {
    $buildArguments.SigningThumbprint = $SigningThumbprint
}
if (-not [string]::IsNullOrWhiteSpace($TimestampUrl)) {
    $buildArguments.TimestampUrl = $TimestampUrl
}

$packageBuildLock = Enter-WindowsPackageBuildLock $workspaceRoot
try {
    Write-Host "Building the Windows package and installer..."
    & $packageBuilder @buildArguments
    if ($LASTEXITCODE -ne 0) {
        throw "Windows package build failed."
    }

    $packageDirectory = Join-Path $workspaceRoot "dist\windows\VideoFerry"
    $portableArchive = Join-Path $workspaceRoot "dist\windows\VideoFerry-$appVersion-windows-x86_64.zip"
    $installer = Join-Path $workspaceRoot "dist\windows\installer\VideoFerrySetup-$appVersion-windows-x86_64.exe"
    foreach ($artifact in @($packageDirectory, $portableArchive, $installer)) {
        if (-not (Test-Path -LiteralPath $artifact)) {
            throw "Windows build did not produce the expected artifact: $artifact"
        }
    }

    Write-Host "Windows package: $packageDirectory"
    Write-Host "Portable archive: $portableArchive"
    Write-Host "Installer: $installer"
} finally {
    $packageBuildLock.Dispose()
}
