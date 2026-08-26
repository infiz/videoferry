param(
    [string]$ArchivePath = "",
    [switch]$VerifyOnly
)

$ErrorActionPreference = "Stop"
$workspaceRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot "..\..")).Path
$manifestPath = Join-Path $workspaceRoot "engine-manifest.toml"
$manifest = [IO.File]::ReadAllText($manifestPath)

function Get-TomlString([string]$Text, [string]$Name) {
    $escapedName = [Regex]::Escape($Name)
    $match = [Regex]::Match(
        $Text,
        ('(?m)^\s*' + $escapedName + '\s*=\s*"([^"]+)"\s*$')
    )
    if (-not $match.Success) {
        throw "Missing '$Name' in $manifestPath"
    }
    return $match.Groups[1].Value
}

function Get-TomlSection([string]$Text, [string]$Name) {
    $escapedName = [Regex]::Escape($Name)
    $match = [Regex]::Match(
        $Text,
        "(?ms)^\[$escapedName\]\s*(.*?)(?=^\[|\z)"
    )
    if (-not $match.Success) {
        throw "Missing section [$Name] in $manifestPath"
    }
    return $match.Groups[1].Value
}

function Assert-WorkspaceChild([string]$Path, [string]$AllowedRoot, [string]$Label) {
    $absolute = [IO.Path]::GetFullPath($Path)
    $allowed = [IO.Path]::GetFullPath($AllowedRoot).TrimEnd('\')
    $prefix = $allowed + '\'
    if (-not $absolute.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "$Label must stay under ${allowed}: $absolute"
    }
    return $absolute
}

function Assert-PinnedSdk([string]$SdkPath, [string]$Version) {
    $required = @(
        "include\libavcodec\avcodec.h",
        "include\libavfilter\avfilter.h",
        "include\libavformat\avformat.h",
        "include\libavutil\avutil.h",
        "bin\avcodec-63.dll",
        "bin\avfilter-12.dll",
        "bin\avformat-63.dll",
        "bin\avutil-61.dll",
        "bin\swresample-7.dll",
        "bin\swscale-10.dll",
        "LICENSE"
    )
    foreach ($relative in $required) {
        $candidate = Join-Path $SdkPath $relative
        if (-not (Test-Path -LiteralPath $candidate -PathType Leaf)) {
            throw "FFmpeg $Version SDK is missing '$relative': $SdkPath"
        }
    }
    if (Get-ChildItem -LiteralPath (Join-Path $SdkPath "bin") -File |
        Where-Object { $_.Name -in @("ffmpeg.exe", "ffprobe.exe") } |
        Select-Object -First 1) {
        Write-Host "The development SDK contains ffmpeg/ffprobe tools; packaging excludes them."
    }
}

$ffmpegVersion = Get-TomlString $manifest "ffmpeg_version"
$windowsSection = Get-TomlSection $manifest "windows_x86_64"
$archiveName = Get-TomlString $windowsSection "archive"
$archiveUrl = Get-TomlString $windowsSection "url"
$expectedSha256 = (Get-TomlString $windowsSection "sha256").ToUpperInvariant()
$runtimeDirectory = Get-TomlString $windowsSection "runtime_directory"
if (-not $archiveUrl.StartsWith("https://", [StringComparison]::OrdinalIgnoreCase)) {
    throw "The pinned Windows FFmpeg URL must use HTTPS: $archiveUrl"
}

$sdkRoot = Join-Path $workspaceRoot ".local\ffmpeg"
$downloadRoot = Join-Path $workspaceRoot ".local\downloads"
$sdkPath = Assert-WorkspaceChild (Join-Path $workspaceRoot $runtimeDirectory) $sdkRoot "FFmpeg SDK"

if (Test-Path -LiteralPath $sdkPath -PathType Container) {
    Assert-PinnedSdk $sdkPath $ffmpegVersion
    if ([string]::IsNullOrWhiteSpace($ArchivePath)) {
        Write-Host "Pinned FFmpeg $ffmpegVersion SDK is already installed: $sdkPath"
        exit 0
    }
}

if ([string]::IsNullOrWhiteSpace($ArchivePath)) {
    New-Item -ItemType Directory -Path $downloadRoot -Force | Out-Null
    $archive = Assert-WorkspaceChild (Join-Path $downloadRoot $archiveName) $downloadRoot "Download"
} else {
    $archive = (Resolve-Path -LiteralPath $ArchivePath).Path
}

if (-not (Test-Path -LiteralPath $archive -PathType Leaf)) {
    if ($VerifyOnly) {
        throw "Pinned archive is not available for verification: $archive"
    }
    $partial = Assert-WorkspaceChild "$archive.download" $downloadRoot "Partial download"
    try {
        Invoke-WebRequest -Uri $archiveUrl -OutFile $partial -UseBasicParsing
        $downloadHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $partial).Hash
        if ($downloadHash -ne $expectedSha256) {
            throw "FFmpeg archive checksum mismatch: expected $expectedSha256, got $downloadHash"
        }
        Move-Item -LiteralPath $partial -Destination $archive
    } finally {
        Remove-Item -LiteralPath $partial -Force -ErrorAction SilentlyContinue
    }
}

$actualSha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $archive).Hash
if ($actualSha256 -ne $expectedSha256) {
    throw "FFmpeg archive checksum mismatch: expected $expectedSha256, got $actualSha256"
}
Write-Host "Verified FFmpeg $ffmpegVersion archive SHA-256: $actualSha256"

if ($VerifyOnly) {
    if (-not (Test-Path -LiteralPath $sdkPath -PathType Container)) {
        throw "Pinned FFmpeg SDK is not installed: $sdkPath"
    }
    Assert-PinnedSdk $sdkPath $ffmpegVersion
    Write-Host "Verified pinned FFmpeg SDK: $sdkPath"
    exit 0
}
if (Test-Path -LiteralPath $sdkPath) {
    throw "Refusing to replace an existing SDK directory: $sdkPath"
}

New-Item -ItemType Directory -Path $sdkRoot -Force | Out-Null
$staging = Assert-WorkspaceChild (Join-Path $sdkRoot ".extract-$([Guid]::NewGuid().ToString('N'))") $sdkRoot "Extraction directory"
New-Item -ItemType Directory -Path $staging | Out-Null
try {
    & tar.exe -xf $archive -C $staging
    if ($LASTEXITCODE -ne 0) {
        throw "Unable to extract the pinned FFmpeg archive"
    }
    $extracted = Join-Path $staging "ffmpeg-$ffmpegVersion-full_build-shared"
    if (-not (Test-Path -LiteralPath $extracted -PathType Container)) {
        throw "Pinned archive did not contain the expected SDK directory: $extracted"
    }
    Assert-PinnedSdk $extracted $ffmpegVersion
    Move-Item -LiteralPath $extracted -Destination $sdkPath
} finally {
    if (Test-Path -LiteralPath $staging -PathType Container) {
        $resolvedStaging = (Resolve-Path -LiteralPath $staging).Path
        Assert-WorkspaceChild $resolvedStaging $sdkRoot "Extraction cleanup" | Out-Null
        Remove-Item -LiteralPath $resolvedStaging -Recurse -Force
    }
}

Assert-PinnedSdk $sdkPath $ffmpegVersion
Write-Host "Installed pinned FFmpeg $ffmpegVersion SDK: $sdkPath"
