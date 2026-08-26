param(
    [Parameter(Mandatory = $true)]
    [string]$PackageDirectory,
    [string]$DumpBin = $env:VIDEOFERRY_DUMPBIN
)

$ErrorActionPreference = 'Stop'
$PackageDirectory = (Resolve-Path -LiteralPath $PackageDirectory).Path

if ([string]::IsNullOrWhiteSpace($DumpBin)) {
    $command = Get-Command dumpbin.exe -ErrorAction SilentlyContinue
    if ($null -ne $command) {
        $DumpBin = $command.Source
    }
}
if ([string]::IsNullOrWhiteSpace($DumpBin)) {
    $programFilesX86 = [Environment]::GetFolderPath('ProgramFilesX86')
    $vswhere = Join-Path $programFilesX86 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (Test-Path -LiteralPath $vswhere -PathType Leaf) {
        $DumpBin = @(& $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -find 'VC\Tools\MSVC\**\bin\Hostx64\x64\dumpbin.exe') |
            Select-Object -First 1
    }
}
if ([string]::IsNullOrWhiteSpace($DumpBin) -or
    -not (Test-Path -LiteralPath $DumpBin -PathType Leaf)) {
    throw 'dumpbin.exe was not found; install the Visual C++ x64 build tools or set VIDEOFERRY_DUMPBIN'
}
$DumpBin = (Resolve-Path -LiteralPath $DumpBin).Path

$applicationName = 'VideoFerry.exe'
$runtimeDlls = @(
    'avcodec-63.dll',
    'avfilter-12.dll',
    'avformat-63.dll',
    'avutil-61.dll',
    'swresample-7.dll',
    'swscale-10.dll'
)
$expectedPackageDlls = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
foreach ($runtimeDll in $runtimeDlls) {
    [void]$expectedPackageDlls.Add($runtimeDll)
}
$actualPackageDlls = @(Get-ChildItem -LiteralPath $PackageDirectory -Filter '*.dll' -File |
    ForEach-Object Name)
if ($actualPackageDlls.Count -ne $runtimeDlls.Count -or
    $actualPackageDlls.Where({ -not $expectedPackageDlls.Contains($_) }).Count -ne 0) {
    throw "Packaged DLL set is not the minimal pinned runtime. Expected '$($runtimeDlls -join ', ')'; found '$($actualPackageDlls -join ', ')'"
}

$importsByBinary = @{}
foreach ($name in @($applicationName) + $runtimeDlls) {
    $binary = Join-Path $PackageDirectory $name
    if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
        throw "Packaged binary is missing: $binary"
    }
    $output = & $DumpBin /nologo /dependents $binary
    if ($LASTEXITCODE -ne 0) {
        throw "Unable to inspect PE dependencies: $binary"
    }
    $imports = @($output |
        Select-String -Pattern '^    [A-Za-z0-9_.-]+\.dll$' |
        ForEach-Object { $_.Line.Trim() })
    $importsByBinary[$name] = $imports
    $dynamicCrt = @($imports | Where-Object { $_ -match '^(VCRUNTIME|MSVCP)[A-Za-z0-9_.-]*\.dll$' })
    if ($dynamicCrt.Count -ne 0) {
        throw "$name depends on the non-bundled Visual C++ runtime: $($dynamicCrt -join ', ')"
    }
    foreach ($import in $imports) {
        if (($import -match '^(avcodec|avfilter|avformat|avutil|swresample|swscale)-[0-9]+\.dll$') -and
            -not $expectedPackageDlls.Contains($import)) {
            throw "$name imports an unbundled FFmpeg runtime: $import"
        }
    }
}

$applicationImports = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
foreach ($import in $importsByBinary[$applicationName]) {
    [void]$applicationImports.Add($import)
}
foreach ($runtimeDll in $runtimeDlls) {
    if (-not $applicationImports.Contains($runtimeDll)) {
        throw "$applicationName does not directly import expected runtime $runtimeDll"
    }
}

Write-Host "Windows package dependency closure passed (static Rust CRT and $($runtimeDlls.Count) pinned FFmpeg DLLs): $PackageDirectory"
