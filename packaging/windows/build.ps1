param(
    [string]$FfmpegSdk = $env:FFMPEG_DIR,
    [string]$LibClang = $env:LIBCLANG_PATH,
    [string]$InnoCompiler = $env:VIDEOFERRY_INNO_COMPILER,
    [string]$SignTool = $env:VIDEOFERRY_WINDOWS_SIGNTOOL,
    [string]$SigningThumbprint = $env:VIDEOFERRY_WINDOWS_SIGNING_CERT_THUMBPRINT,
    [string]$TimestampUrl = $env:VIDEOFERRY_WINDOWS_SIGNING_TIMESTAMP_URL,
    [switch]$SkipInstaller,
    [switch]$SkipSmoke
)

$ErrorActionPreference = "Stop"
$workspaceRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$workspaceManifest = [IO.File]::ReadAllText((Join-Path $workspaceRoot 'Cargo.toml'))
$appVersionMatch = [Regex]::Match($workspaceManifest, '(?m)^version\s*=\s*"([^"]+)"\s*$')
if (-not $appVersionMatch.Success) {
    throw "Unable to read the application version from the workspace Cargo.toml"
}
$appVersion = $appVersionMatch.Groups[1].Value
$toolchainFile = Join-Path $workspaceRoot "rust-toolchain.toml"
$toolchainText = [IO.File]::ReadAllText($toolchainFile)
$toolchainMatch = [Regex]::Match($toolchainText, '(?m)^\s*channel\s*=\s*"([^"]+)"\s*$')
if (-not $toolchainMatch.Success) {
    throw "Unable to read the pinned Rust toolchain from $toolchainFile"
}
$expectedRustVersion = $toolchainMatch.Groups[1].Value
$actualRustVersion = (& rustc --version).Trim()
if ($LASTEXITCODE -ne 0 -or -not $actualRustVersion.StartsWith("rustc $expectedRustVersion ", [StringComparison]::Ordinal)) {
    throw "Release builds require rustc $expectedRustVersion exactly; active compiler is '$actualRustVersion'"
}

function Assert-WorkspaceChild([string]$Path) {
    $absolute = [System.IO.Path]::GetFullPath($Path)
    $prefix = $workspaceRoot.TrimEnd('\') + '\'
    if (-not $absolute.StartsWith($prefix, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing to modify a path outside the Rust workspace: $absolute"
    }
}

$resolvedSignTool = $null
if (-not [string]::IsNullOrWhiteSpace($SigningThumbprint)) {
    $SigningThumbprint = $SigningThumbprint.Replace(" ", "")
    if ($SigningThumbprint -notmatch '^[0-9A-Fa-f]{40}$') {
        throw "Windows signing certificate thumbprint must contain exactly 40 hexadecimal characters"
    }
    if ([string]::IsNullOrWhiteSpace($TimestampUrl)) {
        throw "Set VIDEOFERRY_WINDOWS_SIGNING_TIMESTAMP_URL when release signing is enabled"
    }
    $parsedTimestampUrl = $null
    if (-not [Uri]::TryCreate($TimestampUrl, [UriKind]::Absolute, [ref]$parsedTimestampUrl) -or
        $parsedTimestampUrl.Scheme -notin @('http', 'https')) {
        throw "Windows signing timestamp URL must be an absolute HTTP(S) URL: $TimestampUrl"
    }
    if ([string]::IsNullOrWhiteSpace($SignTool)) {
        $signCommand = Get-Command signtool.exe -ErrorAction SilentlyContinue
        if ($null -ne $signCommand) {
            $resolvedSignTool = $signCommand.Source
        }
    } else {
        $resolvedSignTool = (Resolve-Path -LiteralPath $SignTool).Path
    }
    if ([string]::IsNullOrWhiteSpace($resolvedSignTool) -or
        -not (Test-Path -LiteralPath $resolvedSignTool -PathType Leaf)) {
        throw "signtool.exe was not found; pass -SignTool or set VIDEOFERRY_WINDOWS_SIGNTOOL"
    }
}

function Invoke-WindowsCodeSign([string]$Path) {
    if ([string]::IsNullOrWhiteSpace($SigningThumbprint)) {
        return
    }
    & $resolvedSignTool sign /sha1 $SigningThumbprint /fd SHA256 /tr $TimestampUrl /td SHA256 $Path
    if ($LASTEXITCODE -ne 0) {
        throw "Authenticode signing failed: $Path"
    }
    $signature = Get-AuthenticodeSignature -LiteralPath $Path
    if ($signature.Status -ne [Management.Automation.SignatureStatus]::Valid) {
        throw "Authenticode verification failed for ${Path}: $($signature.StatusMessage)"
    }
}

if ([string]::IsNullOrWhiteSpace($FfmpegSdk)) {
    $FfmpegSdk = Join-Path $workspaceRoot ".local\ffmpeg\ffmpeg-9.0.1-full_build-shared"
}
$FfmpegSdk = (Resolve-Path -LiteralPath $FfmpegSdk).Path
foreach ($required in @("include", "lib", "bin")) {
    if (-not (Test-Path -LiteralPath (Join-Path $FfmpegSdk $required))) {
        throw "FFmpeg SDK is missing '$required': $FfmpegSdk"
    }
}
if (-not (Test-Path -LiteralPath (Join-Path $FfmpegSdk "bin\avcodec-63.dll"))) {
    throw "FFmpeg SDK does not match the pinned libavcodec 63 runtime"
}
if ([string]::IsNullOrWhiteSpace($LibClang)) {
    $LibClang = "C:\Program Files\LLVM\bin"
}
if (-not (Test-Path -LiteralPath $LibClang)) {
    throw "libclang directory was not found: $LibClang"
}

$env:FFMPEG_DIR = $FfmpegSdk
$env:LIBCLANG_PATH = $LibClang
$env:PATH = "$(Join-Path $FfmpegSdk 'bin');$env:PATH"

Push-Location $workspaceRoot
try {
    cargo build --locked --release -p videoferry-app --features native-ffmpeg
    if ($LASTEXITCODE -ne 0) {
        throw "Rust release build failed"
    }

    $distRoot = Join-Path $workspaceRoot "dist\windows"
    $appDir = Join-Path $distRoot "VideoFerry"
    Assert-WorkspaceChild $distRoot
    Assert-WorkspaceChild $appDir
    if (Test-Path -LiteralPath $appDir) {
        Remove-Item -LiteralPath $appDir -Recurse -Force
    }
    New-Item -ItemType Directory -Path $appDir -Force | Out-Null
    New-Item -ItemType Directory -Path (Join-Path $appDir "lut\dji") -Force | Out-Null

    Copy-Item -LiteralPath (Join-Path $workspaceRoot "target\release\videoferry.exe") -Destination (Join-Path $appDir "VideoFerry.exe")
    $runtimeDlls = @(
        "avcodec-63.dll",
        "avfilter-12.dll",
        "avformat-63.dll",
        "avutil-61.dll",
        "swresample-7.dll",
        "swscale-10.dll"
    )
    foreach ($runtimeDll in $runtimeDlls) {
        $sourceDll = Join-Path $FfmpegSdk "bin\$runtimeDll"
        if (-not (Test-Path -LiteralPath $sourceDll -PathType Leaf)) {
            throw "FFmpeg SDK is missing required runtime DLL: $sourceDll"
        }
        Copy-Item -LiteralPath $sourceDll -Destination $appDir
    }
    Copy-Item -LiteralPath (Join-Path $workspaceRoot "engine-manifest.toml") -Destination $appDir
    Copy-Item -LiteralPath (Join-Path $workspaceRoot "README.md") -Destination $appDir
    Copy-Item -LiteralPath (Join-Path $workspaceRoot "LICENSE") -Destination (Join-Path $appDir "VIDEOFERRY-LICENSE.txt")
    Copy-Item -LiteralPath (Join-Path $workspaceRoot "docs\UPGRADING_FFMPEG.md") -Destination (Join-Path $appDir "UPGRADING_FFMPEG.md")
    Copy-Item -LiteralPath (Join-Path $workspaceRoot "crates\app\assets\fonts\OFL.txt") -Destination (Join-Path $appDir "NOTO-SANS-CJK-LICENSE.txt")
    Copy-Item -Path (Join-Path $workspaceRoot "assets\lut\dji\*.cube") -Destination (Join-Path $appDir "lut\dji")
    if (Test-Path -LiteralPath (Join-Path $FfmpegSdk "LICENSE")) {
        Copy-Item -LiteralPath (Join-Path $FfmpegSdk "LICENSE") -Destination (Join-Path $appDir "FFMPEG-LICENSE.txt")
    }

    $packagedExecutable = Join-Path $appDir "VideoFerry.exe"
    Invoke-WindowsCodeSign $packagedExecutable
    & (Join-Path $workspaceRoot "testing\windows-package-dependencies.ps1") -PackageDirectory $appDir
    if ($LASTEXITCODE -ne 0) {
        throw "Windows package dependency closure failed"
    }
    $runtimeReportPath = Join-Path $appDir ".runtime-verification.txt"
    $originalPath = $env:PATH
    $originalRuntimeReportPath = $env:VIDEOFERRY_RUNTIME_REPORT_PATH
    try {
        $env:PATH = "$(Join-Path $env:SystemRoot 'System32');$env:SystemRoot"
        $env:VIDEOFERRY_RUNTIME_REPORT_PATH = $runtimeReportPath
        $runtimeProcess = Start-Process -FilePath $packagedExecutable -ArgumentList '--verify-runtime' -Wait -PassThru -WindowStyle Hidden
        if ($runtimeProcess.ExitCode -ne 0) {
            $failureReport = if (Test-Path -LiteralPath $runtimeReportPath) {
                [IO.File]::ReadAllText($runtimeReportPath)
            } else {
                'no runtime report was written'
            }
            throw "Packaged direct runtime verification failed with exit code $($runtimeProcess.ExitCode): $failureReport"
        }
        if (-not (Test-Path -LiteralPath $runtimeReportPath -PathType Leaf)) {
            throw 'Packaged direct runtime verification wrote no report'
        }
        $runtimeReport = [IO.File]::ReadAllText($runtimeReportPath)
    } finally {
        $env:PATH = $originalPath
        $env:VIDEOFERRY_RUNTIME_REPORT_PATH = $originalRuntimeReportPath
        Remove-Item -LiteralPath $runtimeReportPath -Force -ErrorAction SilentlyContinue
    }
    foreach ($requiredPattern in @(
        '^runtime=ok$',
        '^engine=FFmpeg 9\.0\.1.*libavformat 63\.1\.101.*libavcodec 63\.1\.101.*libavfilter 12\.1\.101.*libavutil 61\.1\.101.*GPL',
        '^required_encoders=aac,ac3,libsvtav1,libx264,libx265,mov_text,srt$',
        '^stabilization=',
        '^muxers=matroska,mp4$'
    )) {
        if ($runtimeReport -notmatch "(?m)$requiredPattern") {
            throw "Packaged direct runtime report omitted '$requiredPattern': $runtimeReport"
        }
    }

    $zipPath = Join-Path $distRoot "VideoFerry-$appVersion-windows-x86_64.zip"
    Assert-WorkspaceChild $zipPath
    if (Test-Path -LiteralPath $zipPath) {
        Remove-Item -LiteralPath $zipPath -Force
    }
    Compress-Archive -LiteralPath $appDir -DestinationPath $zipPath -CompressionLevel Optimal

    if (-not $SkipSmoke) {
        & (Join-Path $PSScriptRoot 'smoke-slint.ps1') -Archive $zipPath
    }

    if (-not $SkipInstaller) {
        $iscc = $null
        if (-not [string]::IsNullOrWhiteSpace($InnoCompiler)) {
            $iscc = (Resolve-Path -LiteralPath $InnoCompiler).Path
            if (-not (Test-Path -LiteralPath $iscc -PathType Leaf)) {
                throw "Inno Setup compiler is not a file: $iscc"
            }
        } else {
            $command = Get-Command ISCC.exe -ErrorAction SilentlyContinue
            if ($null -ne $command) {
                $iscc = $command.Source
            } else {
                $localCompiler = Join-Path $workspaceRoot '.local\inno-setup\ISCC.exe'
                if (Test-Path -LiteralPath $localCompiler -PathType Leaf) {
                    $iscc = $localCompiler
                }
            }
        }
        if ($iscc) {
            $installerOutput = Join-Path $distRoot "installer"
            New-Item -ItemType Directory -Path $installerOutput -Force | Out-Null
            $installerEnvironmentNames = @(
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
                'VIDEOFERRY_INSTALLER_ICON'
            )
            $savedInstallerEnvironment = @{}
            foreach ($name in $installerEnvironmentNames) {
                $savedInstallerEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
                [Environment]::SetEnvironmentVariable($name, $null, 'Process')
            }
            try {
                $env:VIDEOFERRY_PACKAGE_DIR = $appDir
                $env:VIDEOFERRY_INSTALLER_OUTPUT = $installerOutput
                $env:VIDEOFERRY_INSTALLER_APP_VERSION = $appVersion
                $env:VIDEOFERRY_INSTALLER_BASE_FILENAME = "VideoFerrySetup-$appVersion-windows-x86_64"
                $env:VIDEOFERRY_INSTALLER_ICON = Join-Path $workspaceRoot 'crates\app\assets\app-icon.ico'
                & $iscc (Join-Path $PSScriptRoot "VideoFerry.iss")
                if ($LASTEXITCODE -ne 0) {
                    throw "Inno Setup failed with exit code $LASTEXITCODE. Review the compiler output above."
                }
                $installerPath = Join-Path $installerOutput "VideoFerrySetup-$appVersion-windows-x86_64.exe"
                if (-not (Test-Path -LiteralPath $installerPath -PathType Leaf)) {
                    throw "Inno Setup did not produce the expected installer: $installerPath"
                }
                Invoke-WindowsCodeSign $installerPath
            } finally {
                foreach ($name in $installerEnvironmentNames) {
                    [Environment]::SetEnvironmentVariable($name, $savedInstallerEnvironment[$name], 'Process')
                }
            }
        } else {
            Write-Warning "ISCC.exe was not found; portable ZIP was built, installer was skipped. Pass -InnoCompiler or set VIDEOFERRY_INNO_COMPILER."
        }
    }

    Write-Host "Portable application: $appDir"
    Write-Host "Portable archive: $zipPath"
} finally {
    Pop-Location
}
