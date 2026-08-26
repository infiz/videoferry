[CmdletBinding()]
param(
    [string]$OutputDirectory = (Join-Path $PSScriptRoot '..\.local\product-screenshots'),
    [Parameter(Mandatory)]
    [string]$QueueFolder
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class VideoFerryScreenshotWindow {
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr handle);
    [DllImport("user32.dll")] public static extern bool MoveWindow(IntPtr handle, int x, int y, int width, int height, bool repaint);
}
'@

$workspaceRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$application = Join-Path $workspaceRoot 'dist\windows\VideoFerry\VideoFerry.exe'
$ffmpeg = Join-Path $workspaceRoot '.local\ffmpeg\ffmpeg-9.0.1-full_build-shared\bin\ffmpeg.exe'
foreach ($path in @($application, $ffmpeg)) {
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Required file not found: $path" }
}

$captureRoot = Join-Path $workspaceRoot '.local\product-screenshot-capture'
$captureRootFull = [IO.Path]::GetFullPath($captureRoot)
$allowedPrefix = [IO.Path]::GetFullPath((Join-Path $workspaceRoot '.local')).TrimEnd('\') + '\'
if (-not $captureRootFull.StartsWith($allowedPrefix, [StringComparison]::OrdinalIgnoreCase)) {
    throw "Refusing to replace an unexpected capture directory: $captureRootFull"
}
if (Test-Path -LiteralPath $captureRootFull) { Remove-Item -LiteralPath $captureRootFull -Recurse -Force }
New-Item -ItemType Directory -Path $captureRootFull, $OutputDirectory -Force | Out-Null

$isolatedAppData = Join-Path $captureRootFull 'appdata'
$isolatedLocalAppData = Join-Path $captureRootFull 'localappdata'
$isolatedTemp = Join-Path $captureRootFull 'temp'
$stateDirectory = Join-Path $isolatedLocalAppData 'VideoFerry'
New-Item -ItemType Directory -Path $isolatedAppData, $stateDirectory, $isolatedTemp -Force | Out-Null

$savedEnvironment = @{}
foreach ($name in @('APPDATA', 'LOCALAPPDATA', 'TEMP', 'TMP', 'SLINT_SCALE_FACTOR')) {
    $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}
$env:APPDATA = $isolatedAppData
$env:LOCALAPPDATA = $isolatedLocalAppData
$env:TEMP = $isolatedTemp
$env:TMP = $isolatedTemp
$env:SLINT_SCALE_FACTOR = '1'

function Write-Queue([array]$Tasks) {
    $state = @{ version = 2; was_running = $false; tasks = $Tasks }
    $json = $state | ConvertTo-Json -Depth 10
    [IO.File]::WriteAllText((Join-Path $stateDirectory 'queue.json'), $json, [Text.UTF8Encoding]::new($false))
}

function New-TaskState([string]$Name, [string]$Target, [string]$Id) {
    return @{
        name = $Name
        target_paths = @($Target)
        source_root = if (Test-Path -LiteralPath $Target -PathType Container) { $Target } else { $null }
        settings = @{
            mode = 'TV'; encoder = 'x265'; fps_raw = '__auto__'; target_fps = $null
            share_lowest_fps = $true; quality_crf = '28'; quality_preset = 'medium'
            stabilize_strength = 'Balanced'; trim_start = ''; trim_end = ''; apply_lut = $false
            photo_interval_seconds = 4.0; slideshow_resolution = '1080p'; slideshow_width = 1920
            slideshow_height = 1080; slideshow_fps = 30; slideshow_collage_enabled = $false
            slideshow_audio_paths = @(); metadata = 'remove'
        }
        queued_time = '2026-08-25 19:37:32'; task_data_id = $Id; status = 'pending'
        complete_time = ''; error = $null; skipped_paths = @()
    }
}

function Start-App {
    $process = Start-Process -FilePath $application -WorkingDirectory (Split-Path $application) -PassThru
    for ($attempt = 0; $attempt -lt 100; $attempt++) {
        Start-Sleep -Milliseconds 100
        $process.Refresh()
        if ($process.HasExited) { throw "VideoFerry exited with code $($process.ExitCode)" }
        if ($process.MainWindowHandle -ne [IntPtr]::Zero) { break }
    }
    if ($process.MainWindowHandle -eq [IntPtr]::Zero) { throw 'VideoFerry did not create a window' }
    [void][VideoFerryScreenshotWindow]::MoveWindow($process.MainWindowHandle, 40, 40, 1180, 760, $true)
    [void][VideoFerryScreenshotWindow]::SetForegroundWindow($process.MainWindowHandle)
    Start-Sleep -Milliseconds 700
    return $process
}

function Stop-App([Diagnostics.Process]$Process) {
    if ($null -eq $Process -or $Process.HasExited) { return }
    $Process.CloseMainWindow() | Out-Null
    if (-not $Process.WaitForExit(3000)) { $Process.Kill(); $Process.WaitForExit() }
}

function Find-Control([Diagnostics.Process]$Process, [string]$Name) {
    $processCondition = [Windows.Automation.PropertyCondition]::new(
        [Windows.Automation.AutomationElement]::ProcessIdProperty,
        $Process.Id
    )
    $window = [Windows.Automation.AutomationElement]::RootElement.FindFirst(
        [Windows.Automation.TreeScope]::Children,
        $processCondition
    )
    if ($null -eq $window) { return $null }
    $condition = [Windows.Automation.PropertyCondition]::new([Windows.Automation.AutomationElement]::NameProperty, $Name)
    return $window.FindFirst([Windows.Automation.TreeScope]::Descendants, $condition)
}

function Invoke-Control([Diagnostics.Process]$Process, [string]$Name) {
    for ($attempt = 0; $attempt -lt 50; $attempt++) {
        $element = Find-Control $Process $Name
        if ($null -ne $element) {
            $pattern = $element.GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern)
            $pattern.Invoke()
            Start-Sleep -Milliseconds 650
            return
        }
        Start-Sleep -Milliseconds 100
    }
    throw "Control not found: $Name"
}

function Save-Window([Diagnostics.Process]$Process, [string]$Filename) {
    $Process.Refresh()
    [void][VideoFerryScreenshotWindow]::MoveWindow($Process.MainWindowHandle, 40, 40, 1180, 760, $true)
    [void][VideoFerryScreenshotWindow]::SetForegroundWindow($Process.MainWindowHandle)
    Start-Sleep -Milliseconds 400
    $Process.Refresh()
    $window = [Windows.Automation.AutomationElement]::FromHandle($Process.MainWindowHandle)
    if ($null -eq $window) { throw 'Unable to read the VideoFerry window bounds' }
    $bounds = $window.Current.BoundingRectangle
    $left = [int][Math]::Round($bounds.Left)
    $top = [int][Math]::Round($bounds.Top)
    $width = [int][Math]::Round($bounds.Width)
    $height = [int][Math]::Round($bounds.Height)
    $bitmap = [Drawing.Bitmap]::new($width, $height, [Drawing.Imaging.PixelFormat]::Format24bppRgb)
    $graphics = [Drawing.Graphics]::FromImage($bitmap)
    try {
        $graphics.CopyFromScreen($left, $top, 0, 0, [Drawing.Size]::new($width, $height))
        $bitmap.Save((Join-Path $OutputDirectory $Filename), [Drawing.Imaging.ImageFormat]::Png)
    } finally {
        $graphics.Dispose()
        $bitmap.Dispose()
    }
}

$process = $null
try {
    Write-Queue @()
    $process = Start-App
    Invoke-Control $process 'New task'
    Save-Window $process '03-settings.png'
    Stop-App $process
    $process = $null

    if (-not (Test-Path -LiteralPath $QueueFolder -PathType Container)) {
        throw "Queue screenshot folder not found: $QueueFolder"
    }
    Write-Queue @((New-TaskState 'TV — Video library' $QueueFolder 'product-queue'))
    $process = Start-App
    Start-Sleep -Seconds 2
    Save-Window $process '01-queue.png'
    Stop-App $process
    $process = $null

    $media = Join-Path $captureRootFull 'current-file-demo.mp4'
    & $ffmpeg -hide_banner -loglevel error -f lavfi -i 'testsrc2=size=1280x720:rate=25' -f lavfi -i 'sine=frequency=440:sample_rate=48000' -t 90 -c:v libx264 -preset ultrafast -crf 28 -c:a aac -shortest -y $media
    if ($LASTEXITCODE -ne 0) { throw 'Unable to generate the isolated screenshot video' }
    Write-Queue @((New-TaskState 'TV — Current conversion' $media 'product-converting'))
    $process = Start-App
    Invoke-Control $process 'Start queue'
    Start-Sleep -Seconds 2
    Save-Window $process '02-converting.png'
} finally {
    Stop-App $process
    foreach ($name in $savedEnvironment.Keys) {
        [Environment]::SetEnvironmentVariable($name, $savedEnvironment[$name], 'Process')
    }
}

Write-Host "Product screenshots: $OutputDirectory"
