param(
    [string]$Archive,
    [int]$LaunchSeconds = 2
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes

$workspaceRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$workspaceManifest = [IO.File]::ReadAllText((Join-Path $workspaceRoot 'Cargo.toml'))
$appVersion = [Regex]::Match($workspaceManifest, '(?m)^version\s*=\s*"([^"]+)"\s*$').Groups[1].Value
if ([string]::IsNullOrWhiteSpace($Archive)) {
    $Archive = Join-Path $workspaceRoot "dist\windows\VideoFerry-$appVersion-windows-x86_64.zip"
}
$Archive = (Resolve-Path -LiteralPath $Archive).Path
$syntheticFixtureBase64Path = Join-Path $workspaceRoot 'testing\assets\synthetic-smoke-source.mp4.base64'
if (-not (Test-Path -LiteralPath $syntheticFixtureBase64Path -PathType Leaf)) {
    throw "Synthetic package smoke fixture is missing: $syntheticFixtureBase64Path"
}
if ($LaunchSeconds -lt 1) {
    throw 'LaunchSeconds must be at least 1'
}

$smokeRoot = Join-Path $workspaceRoot '.local\package-smoke'
New-Item -ItemType Directory -Path $smokeRoot -Force | Out-Null
$runRoot = Join-Path $smokeRoot ([guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $runRoot | Out-Null

function Assert-SmokeChild([string]$Path) {
    $absolute = [IO.Path]::GetFullPath($Path)
    $prefix = [IO.Path]::GetFullPath($smokeRoot).TrimEnd('\') + '\'
    if (-not $absolute.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing to modify a path outside the package smoke root: $absolute"
    }
}

function Get-FileSha256([string]$Path) {
    $stream = [IO.File]::OpenRead($Path)
    $algorithm = [Security.Cryptography.SHA256]::Create()
    try {
        return ([BitConverter]::ToString($algorithm.ComputeHash($stream))).Replace('-', '')
    } finally {
        $algorithm.Dispose()
        $stream.Dispose()
    }
}

function Assert-Condition([bool]$Condition, [string]$Message) {
    if (-not $Condition) {
        throw $Message
    }
}

function Find-AccessibleElement {
    param(
        [System.Windows.Automation.AutomationElementCollection]$Elements,
        [string]$Name,
        [string]$ControlType
    )
    for ($index = 0; $index -lt $Elements.Count; $index++) {
        $element = $Elements.Item($index)
        try {
            if ($element.Current.Name -eq $Name -and $element.Current.ControlType.ProgrammaticName -eq $ControlType) {
                return $element
            }
        } catch {
            continue
        }
    }
    return $null
}

function Find-AccessibleElementWithPrefix {
    param(
        [System.Windows.Automation.AutomationElementCollection]$Elements,
        [string]$Prefix,
        [string]$ControlType
    )
    for ($index = 0; $index -lt $Elements.Count; $index++) {
        $element = $Elements.Item($index)
        try {
            if ($element.Current.Name.StartsWith($Prefix) -and $element.Current.ControlType.ProgrammaticName -eq $ControlType) {
                return $element
            }
        } catch {
            continue
        }
    }
    return $null
}

function Assert-AccessiblePattern {
    param(
        [System.Windows.Automation.AutomationElement]$Element,
        [System.Windows.Automation.AutomationPattern]$Pattern,
        [string]$Description
    )
    $provider = $null
    Assert-Condition ($Element.TryGetCurrentPattern($Pattern, [ref]$provider)) "$Description does not expose $($Pattern.ProgrammaticName)"
}

function Get-ProcessAccessibleElements([int]$ProcessId) {
    $condition = [System.Windows.Automation.PropertyCondition]::new(
        [System.Windows.Automation.AutomationElement]::ProcessIdProperty,
        $ProcessId
    )
    $elements = [System.Windows.Automation.AutomationElement]::RootElement.FindAll(
        [System.Windows.Automation.TreeScope]::Descendants,
        $condition
    )
    Write-Output -NoEnumerate $elements
}

function Select-AccessibleWorkflow {
    param(
        [int]$ProcessId,
        [string]$Name
    )
    $option = $null
    for ($attempt = 0; $attempt -lt 10 -and $null -eq $option; $attempt++) {
        $elements = Get-ProcessAccessibleElements $ProcessId
        $selector = Find-AccessibleElement $elements 'Workflow' 'ControlType.ComboBox'
        Assert-Condition ($null -ne $selector) 'Workflow selector disappeared from the accessibility tree'
        $provider = $selector.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
        $provider.Invoke()
        Start-Sleep -Milliseconds 150
        $elements = Get-ProcessAccessibleElements $ProcessId
        $option = Find-AccessibleElement $elements $Name 'ControlType.Button'
    }
    Assert-Condition ($null -ne $option) "Workflow option is not accessible: $Name"
    Assert-Condition $option.Current.IsKeyboardFocusable "Workflow option is not keyboard focusable: $Name"
    $provider = $option.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
    $provider.Invoke()
    Start-Sleep -Milliseconds 250
}

function Assert-ModeControl {
    param(
        [System.Windows.Automation.AutomationElementCollection]$Elements,
        [string]$Name,
        [string]$ControlType
    )
    $element = Find-AccessibleElement $Elements $Name $ControlType
    Assert-Condition ($null -ne $element) "Missing mode-specific accessible control: $Name ($ControlType)"
    Assert-Condition $element.Current.IsKeyboardFocusable "Mode-specific control is not keyboard focusable: $Name"
    return $element
}

function Assert-ModeControlAbsent {
    param(
        [System.Windows.Automation.AutomationElementCollection]$Elements,
        [string]$Name,
        [string]$ControlType
    )
    $element = Find-AccessibleElement $Elements $Name $ControlType
    Assert-Condition ($null -eq $element) "Unexpected mode-specific accessible control: $Name ($ControlType)"
}

function Get-AppWindow([int]$ProcessId) {
    $condition = [System.Windows.Automation.PropertyCondition]::new(
        [System.Windows.Automation.AutomationElement]::ProcessIdProperty,
        $ProcessId
    )
    return [System.Windows.Automation.AutomationElement]::RootElement.FindFirst(
        [System.Windows.Automation.TreeScope]::Children,
        $condition
    )
}

$process = $null
$lockedStream = $null
$savedEnvironment = @{}
foreach ($name in @('PATH', 'FFMPEG_DIR', 'LIBCLANG_PATH', 'APPDATA', 'LOCALAPPDATA', 'TEMP', 'TMP')) {
    $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}

try {
    Expand-Archive -LiteralPath $Archive -DestinationPath $runRoot
    $forbidden = @(Get-ChildItem -LiteralPath $runRoot -Recurse -File | Where-Object {
        $_.Name -in @('ffmpeg.exe', 'ffprobe.exe')
    })
    if ($forbidden.Count -ne 0) {
        throw 'Portable archive contains forbidden FFmpeg subprocess executables'
    }

    $applications = @(Get-ChildItem -LiteralPath $runRoot -Recurse -File -Filter 'VideoFerry.exe')
    if ($applications.Count -ne 1) {
        throw "Expected exactly one packaged application, found $($applications.Count)"
    }

    $isolatedAppData = Join-Path $runRoot 'appdata'
    $isolatedLocalAppData = Join-Path $runRoot 'localappdata'
    $isolatedTemp = Join-Path $runRoot 'temp'
    $unrelatedWorkingDirectory = Join-Path $runRoot 'working'
    foreach ($directory in @($isolatedAppData, $isolatedLocalAppData, $isolatedTemp, $unrelatedWorkingDirectory)) {
        New-Item -ItemType Directory -Path $directory | Out-Null
    }

    $env:PATH = "$(Join-Path $env:SystemRoot 'System32');$env:SystemRoot"
    Remove-Item Env:FFMPEG_DIR -ErrorAction SilentlyContinue
    Remove-Item Env:LIBCLANG_PATH -ErrorAction SilentlyContinue
    $env:APPDATA = $isolatedAppData
    $env:LOCALAPPDATA = $isolatedLocalAppData
    $env:TEMP = $isolatedTemp
    $env:TMP = $isolatedTemp

    $malformedInput = Join-Path $runRoot 'malformed-input.mp4'
    [IO.File]::WriteAllBytes($malformedInput, [byte[]](0x00, 0x11, 0x22, 0x33, 0x44))
    $successfulInput = Join-Path $runRoot 'successful-input.mp4'
    $syntheticBytes = [Convert]::FromBase64String([IO.File]::ReadAllText($syntheticFixtureBase64Path).Trim())
    [IO.File]::WriteAllBytes($successfulInput, $syntheticBytes)
    Assert-Condition ($syntheticBytes.Length -eq 3856) 'Synthetic package smoke fixture has an unexpected size'
    Assert-Condition ((Get-FileSha256 $successfulInput) -eq '4A1A967B4DC9C1417C5CDD6E7AEDC67FFC3C0667EEF08E27B815369BBF3B6FE9') 'Synthetic package smoke fixture checksum mismatch'
    $stateDirectory = Join-Path $isolatedLocalAppData 'VideoFerry'
    New-Item -ItemType Directory -Path $stateDirectory | Out-Null
    $queueStatePath = Join-Path $stateDirectory 'queue.json'
    $queueState = @{
        version = 2
        was_running = $false
        tasks = @(
            @{
                name = 'Malformed media safety fixture'
                target_paths = @($malformedInput)
                source_root = $null
                settings = @{
                    mode = 'TV'
                    encoder = 'x265'
                    fps_raw = 'None'
                    target_fps = $null
                    share_lowest_fps = $false
                    quality_crf = '28'
                    quality_preset = 'ultrafast'
                    stabilize_strength = 'Balanced'
                    trim_start = '00:00'
                    trim_end = '00:10'
                    apply_lut = $false
                    photo_interval_seconds = 4.0
                    slideshow_resolution = '1080p'
                    slideshow_fps = 30
                    slideshow_collage_enabled = $false
                    slideshow_audio_paths = @()
                    slideshow_image_paths = @()
                    slideshow_review_image_paths = @()
                    metadata = 'remove'
                }
                queued_time = '2026-08-25 00:00:00'
                task_data_id = 'package-smoke-malformed-task'
                status = 'pending'
                complete_time = ''
                error = $null
                skipped_paths = @()
            },
            @{
                name = 'Successful media safety fixture'
                target_paths = @($successfulInput)
                source_root = $null
                settings = @{
                    mode = 'TV'
                    encoder = 'x265'
                    fps_raw = 'None'
                    target_fps = $null
                    share_lowest_fps = $false
                    quality_crf = '32'
                    quality_preset = 'ultrafast'
                    stabilize_strength = 'Balanced'
                    trim_start = '00:00'
                    trim_end = '00:10'
                    apply_lut = $false
                    photo_interval_seconds = 4.0
                    slideshow_resolution = '1080p'
                    slideshow_fps = 30
                    slideshow_collage_enabled = $false
                    slideshow_audio_paths = @()
                    slideshow_image_paths = @()
                    slideshow_review_image_paths = @()
                    metadata = 'remove'
                }
                queued_time = '2026-08-25 00:00:01'
                task_data_id = 'package-smoke-successful-task'
                status = 'pending'
                complete_time = ''
                error = $null
                skipped_paths = @()
            }
        )
    }
    [IO.File]::WriteAllText(
        $queueStatePath,
        ($queueState | ConvertTo-Json -Depth 8),
        [Text.UTF8Encoding]::new($false)
    )

    $process = Start-Process -FilePath $applications[0].FullName `
        -WorkingDirectory $unrelatedWorkingDirectory -WindowStyle Hidden -PassThru
    Start-Sleep -Seconds $LaunchSeconds
    $process.Refresh()
    if ($process.HasExited) {
        throw "Packaged application exited during clean-environment smoke test with code $($process.ExitCode)"
    }

    $windowCondition = [System.Windows.Automation.PropertyCondition]::new(
        [System.Windows.Automation.AutomationElement]::ProcessIdProperty,
        $process.Id
    )
    $window = [System.Windows.Automation.AutomationElement]::RootElement.FindFirst(
        [System.Windows.Automation.TreeScope]::Children,
        $windowCondition
    )
    Assert-Condition ($null -ne $window) 'Packaged application did not expose a Windows UI Automation window'
    Assert-Condition ($window.Current.Name -eq 'VideoFerry') 'Application window has no stable accessible name'
    Assert-Condition $window.Current.IsKeyboardFocusable 'Application window is not keyboard focusable'

    $elements = $window.FindAll(
        [System.Windows.Automation.TreeScope]::Descendants,
        [System.Windows.Automation.Condition]::TrueCondition
    )
    Assert-Condition ($elements.Count -ge 40) "Accessibility tree is unexpectedly sparse: $($elements.Count) elements"

    $requiredControls = @(
        [pscustomobject]@{ Name = 'Add files'; Type = 'ControlType.Button' },
        [pscustomobject]@{ Name = 'Add folder'; Type = 'ControlType.Button' },
        [pscustomobject]@{ Name = 'Clear queue'; Type = 'ControlType.Button' },
        [pscustomobject]@{ Name = 'Run queue'; Type = 'ControlType.Button' },
        [pscustomobject]@{ Name = 'Workflow'; Type = 'ControlType.ComboBox' },
        [pscustomobject]@{ Name = 'Encoder'; Type = 'ControlType.ComboBox' },
        [pscustomobject]@{ Name = 'Frame rate'; Type = 'ControlType.ComboBox' },
        [pscustomobject]@{ Name = 'Quality (CRF)'; Type = 'ControlType.Spinner' },
        [pscustomobject]@{ Name = 'Encoding speed'; Type = 'ControlType.ComboBox' },
        [pscustomobject]@{ Name = 'Prevent system sleep while converting'; Type = 'ControlType.CheckBox' },
        [pscustomobject]@{ Name = 'Frame preview'; Type = 'ControlType.CheckBox' },
        [pscustomobject]@{ Name = 'Conversion queue'; Type = 'ControlType.Button' },
        [pscustomobject]@{ Name = 'Completed history (0)'; Type = 'ControlType.Button' },
        [pscustomobject]@{ Name = 'Processes'; Type = 'ControlType.Button' },
        [pscustomobject]@{ Name = 'Activity log'; Type = 'ControlType.Button' },
        [pscustomobject]@{ Name = 'About'; Type = 'ControlType.Button' }
    )
    $accessible = @{}
    foreach ($required in $requiredControls) {
        $element = Find-AccessibleElement $elements $required.Name $required.Type
        Assert-Condition ($null -ne $element) "Missing accessible $($required.Type): $($required.Name)"
        Assert-Condition $element.Current.IsKeyboardFocusable "Accessible control is not keyboard focusable: $($required.Name)"
        $accessible[$required.Name] = $element
    }

    $interactiveTypes = @(
        'ControlType.Button',
        'ControlType.CheckBox',
        'ControlType.ComboBox',
        'ControlType.Edit',
        'ControlType.MenuItem',
        'ControlType.Spinner',
        'ControlType.TabItem'
    )
    for ($index = 0; $index -lt $elements.Count; $index++) {
        $element = $elements.Item($index)
        if ($interactiveTypes -contains $element.Current.ControlType.ProgrammaticName) {
            Assert-Condition (-not [string]::IsNullOrWhiteSpace($element.Current.Name)) "Interactive accessibility element has no name: $($element.Current.ControlType.ProgrammaticName)"
        }
    }

    Assert-AccessiblePattern $accessible['Add files'] ([System.Windows.Automation.InvokePattern]::Pattern) 'Add files button'
    Assert-AccessiblePattern $accessible['Workflow'] ([System.Windows.Automation.InvokePattern]::Pattern) 'Workflow selector'
    Assert-AccessiblePattern $accessible['Workflow'] ([System.Windows.Automation.SelectionPattern]::Pattern) 'Workflow selector'
    Assert-AccessiblePattern $accessible['Frame preview'] ([System.Windows.Automation.TogglePattern]::Pattern) 'Frame preview checkbox'
    Assert-AccessiblePattern $accessible['Quality (CRF)'] ([System.Windows.Automation.RangeValuePattern]::Pattern) 'Quality control'

    $accessible['Add files'].SetFocus()
    Start-Sleep -Milliseconds 100
    $focused = [System.Windows.Automation.AutomationElement]::FocusedElement
    Assert-Condition ($focused.Current.ProcessId -eq $process.Id) 'Keyboard focus did not enter the application accessibility tree'
    Assert-Condition ($focused.Current.Name -eq 'Add files') "Keyboard focus reached an unexpected control: $($focused.Current.Name)"

    Assert-AccessiblePattern $accessible['About'] ([System.Windows.Automation.InvokePattern]::Pattern) 'About tab'
    $provider = $accessible['About'].GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
    $provider.Invoke()
    $engineLine = $null
    for ($attempt = 0; $attempt -lt 40 -and $null -eq $engineLine; $attempt++) {
        Start-Sleep -Milliseconds 250
        $elements = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
        $engineLine = Find-AccessibleElementWithPrefix $elements 'FFmpeg 9.0.1' 'ControlType.Text'
    }
    Assert-Condition ($null -ne $engineLine) 'About view did not report the pinned FFmpeg 9.0.1 runtime'
    Assert-Condition ($engineLine.Current.Name -like '*GPL*') 'About view did not report the active FFmpeg license'
    Assert-Condition ($null -ne (Find-AccessibleElement $elements "Application version $appVersion" 'ControlType.Text')) 'About view did not report the application version'
    Assert-Condition ($null -ne (Find-AccessibleElement $elements 'Rust language level 1.98 (toolchain pinned to 1.98.0)' 'ControlType.Text')) 'About view did not report the pinned Rust toolchain'
    Assert-Condition ($null -ne (Find-AccessibleElement $elements 'Licensing' 'ControlType.Text')) 'About view did not expose licensing information'
    $queueTab = Find-AccessibleElement $elements 'Conversion queue' 'ControlType.Button'
    Assert-Condition ($null -ne $queueTab) 'Conversion queue tab disappeared from the About view'
    $provider = $queueTab.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
    $provider.Invoke()
    Start-Sleep -Milliseconds 250

    Select-AccessibleWorkflow $process.Id 'Trim'
    $elements = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
    $trimStart = Assert-ModeControl $elements 'Start' 'ControlType.Edit'
    $trimEnd = Assert-ModeControl $elements 'End' 'ControlType.Edit'
    Assert-AccessiblePattern $trimStart ([System.Windows.Automation.ValuePattern]::Pattern) 'Trim start input'
    Assert-AccessiblePattern $trimEnd ([System.Windows.Automation.ValuePattern]::Pattern) 'Trim end input'
    Assert-ModeControlAbsent $elements 'Encoder' 'ControlType.ComboBox'
    Assert-ModeControlAbsent $elements 'Frame rate' 'ControlType.ComboBox'
    Assert-ModeControlAbsent $elements 'Quality (CRF)' 'ControlType.Spinner'
    Assert-ModeControlAbsent $elements 'Encoding speed' 'ControlType.ComboBox'
    $trimAddFolder = Find-AccessibleElement $elements 'Add folder' 'ControlType.Button'
    Assert-Condition ($null -ne $trimAddFolder -and -not $trimAddFolder.Current.IsEnabled) 'Trim must disable Add folder'

    Select-AccessibleWorkflow $process.Id 'Camera videos'
    $elements = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
    [void](Assert-ModeControl $elements 'Encoder' 'ControlType.ComboBox')
    [void](Assert-ModeControl $elements 'Frame rate' 'ControlType.ComboBox')
    [void](Assert-ModeControl $elements 'Quality (CRF)' 'ControlType.Spinner')
    [void](Assert-ModeControl $elements 'Encoding speed' 'ControlType.ComboBox')
    $cameraLut = Assert-ModeControl $elements 'Apply matching DJI LUT' 'ControlType.CheckBox'
    Assert-Condition ($null -ne (Find-AccessibleElement $elements 'DJI OsmoAction6 → action6.cube' 'ControlType.Text')) 'Camera workflow did not expose the Action 6 LUT map'
    Assert-Condition ($null -ne (Find-AccessibleElement $elements 'DJI OsmoPocket3 → pocket3.cube' 'ControlType.Text')) 'Camera workflow did not expose the Pocket 3 LUT map'
    Assert-AccessiblePattern $cameraLut ([System.Windows.Automation.TogglePattern]::Pattern) 'Camera LUT checkbox'
    $cameraAddFolder = Find-AccessibleElement $elements 'Add folder' 'ControlType.Button'
    Assert-Condition ($null -ne $cameraAddFolder -and $cameraAddFolder.Current.IsEnabled) 'Camera workflow must enable Add folder'

    Select-AccessibleWorkflow $process.Id 'Animation'
    $elements = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
    [void](Assert-ModeControl $elements 'Encoder' 'ControlType.ComboBox')
    [void](Assert-ModeControl $elements 'Frame rate' 'ControlType.ComboBox')
    [void](Assert-ModeControl $elements 'Quality (CRF)' 'ControlType.Spinner')
    [void](Assert-ModeControl $elements 'Encoding speed' 'ControlType.ComboBox')
    Assert-ModeControlAbsent $elements 'Apply matching DJI LUT' 'ControlType.CheckBox'
    $animationAddFolder = Find-AccessibleElement $elements 'Add folder' 'ControlType.Button'
    Assert-Condition ($null -ne $animationAddFolder -and $animationAddFolder.Current.IsEnabled) 'Animation workflow must enable Add folder'

    Select-AccessibleWorkflow $process.Id 'Stabilize'
    $elements = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
    [void](Assert-ModeControl $elements 'Encoder' 'ControlType.ComboBox')
    [void](Assert-ModeControl $elements 'Quality (CRF)' 'ControlType.Spinner')
    [void](Assert-ModeControl $elements 'Encoding speed' 'ControlType.ComboBox')
    $strength = Assert-ModeControl $elements 'Strength' 'ControlType.ComboBox'
    Assert-AccessiblePattern $strength ([System.Windows.Automation.SelectionPattern]::Pattern) 'Stabilization strength selector'
    Assert-ModeControlAbsent $elements 'Frame rate' 'ControlType.ComboBox'
    Assert-ModeControlAbsent $elements 'Apply matching DJI LUT' 'ControlType.CheckBox'

    Select-AccessibleWorkflow $process.Id 'Photo slideshow'
    $elements = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
    [void](Assert-ModeControl $elements 'Encoder' 'ControlType.ComboBox')
    [void](Assert-ModeControl $elements 'Quality (CRF)' 'ControlType.Spinner')
    [void](Assert-ModeControl $elements 'Encoding speed' 'ControlType.ComboBox')
    $photoInterval = Assert-ModeControl $elements 'Photo interval (seconds)' 'ControlType.Spinner'
    $slideshowFps = Assert-ModeControl $elements 'Frames per second' 'ControlType.Spinner'
    [void](Assert-ModeControl $elements 'Resolution' 'ControlType.ComboBox')
    $collage = Assert-ModeControl $elements 'Group portrait photos into collage slides' 'ControlType.CheckBox'
    [void](Assert-ModeControl $elements 'Add audio' 'ControlType.Button')
    Assert-Condition ($null -ne (Find-AccessibleElement $elements 'Output: slideshow.mp4 in the selected folder or beside the first image.' 'ControlType.Text')) 'Photo slideshow workflow did not expose its output location'
    Assert-AccessiblePattern $photoInterval ([System.Windows.Automation.RangeValuePattern]::Pattern) 'Photo interval control'
    Assert-AccessiblePattern $slideshowFps ([System.Windows.Automation.RangeValuePattern]::Pattern) 'Slideshow FPS control'
    Assert-AccessiblePattern $collage ([System.Windows.Automation.TogglePattern]::Pattern) 'Slideshow collage checkbox'
    Assert-ModeControlAbsent $elements 'Frame rate' 'ControlType.ComboBox'
    Assert-ModeControlAbsent $elements 'Apply matching DJI LUT' 'ControlType.CheckBox'

    Select-AccessibleWorkflow $process.Id 'TV'

    $elements = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
    $fixtureRow = Assert-ModeControl $elements 'Malformed media safety fixture' 'ControlType.Button'
    Assert-AccessiblePattern $fixtureRow ([System.Windows.Automation.InvokePattern]::Pattern) 'Queue fixture row'
    $provider = $fixtureRow.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
    $provider.Invoke()
    Start-Sleep -Milliseconds 250

    $elements = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
    $moveDown = Find-AccessibleElement $elements 'Move down' 'ControlType.Button'
    Assert-Condition ($null -ne $moveDown -and $moveDown.Current.IsEnabled) 'Selecting the first pending task did not enable Move down'
    $provider = $moveDown.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
    $provider.Invoke()
    Start-Sleep -Milliseconds 250
    $storedQueue = [IO.File]::ReadAllText($queueStatePath) | ConvertFrom-Json
    Assert-Condition ($storedQueue.tasks[0].task_data_id -eq 'package-smoke-successful-task') 'Move down did not persist the new queue order'
    Assert-Condition ($storedQueue.tasks[1].task_data_id -eq 'package-smoke-malformed-task') 'Move down persisted an unexpected second queue item'

    $elements = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
    $moveUp = Find-AccessibleElement $elements 'Move up' 'ControlType.Button'
    Assert-Condition ($null -ne $moveUp -and $moveUp.Current.IsEnabled) 'Moved pending task did not enable Move up'
    $provider = $moveUp.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
    $provider.Invoke()
    Start-Sleep -Milliseconds 250
    $storedQueue = [IO.File]::ReadAllText($queueStatePath) | ConvertFrom-Json
    Assert-Condition ($storedQueue.tasks[0].task_data_id -eq 'package-smoke-malformed-task') 'Move up did not restore and persist the original queue order'

    $elements = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
    $runSelected = Find-AccessibleElement $elements 'Run selected' 'ControlType.Button'
    Assert-Condition ($null -ne $runSelected -and $runSelected.Current.IsEnabled) 'Selecting a pending task did not enable Run selected'
    $provider = $runSelected.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
    $provider.Invoke()

    $retry = $null
    for ($attempt = 0; $attempt -lt 80 -and $null -eq $retry; $attempt++) {
        Start-Sleep -Milliseconds 250
        $process.Refresh()
        if ($process.HasExited) {
            throw "Packaged application exited while processing the malformed queue fixture with code $($process.ExitCode)"
        }
        $elements = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
        $candidate = Find-AccessibleElement $elements 'Retry / rerun selected' 'ControlType.Button'
        if ($null -ne $candidate -and $candidate.Current.IsEnabled) {
            $retry = $candidate
        }
    }
    Assert-Condition ($null -ne $retry) 'Malformed queue fixture did not reach the rerunnable completed-with-failure state'
    Assert-Condition ($null -ne (Find-AccessibleElement $elements 'Completed' 'ControlType.Text')) 'Malformed queue fixture did not expose its completed row status'

    $failureActivity = $null
    for ($index = 0; $index -lt $elements.Count; $index++) {
        $element = $elements.Item($index)
        try {
            if ($element.Current.ControlType.ProgrammaticName -eq 'ControlType.Text' -and $element.Current.Name.StartsWith('Task finished with failed file ')) {
                $failureActivity = $element.Current.Name
                break
            }
        } catch {
            continue
        }
    }
    Assert-Condition ($null -ne $failureActivity) 'Malformed queue fixture did not report the failed source clearly'
    Assert-Condition (Test-Path -LiteralPath $malformedInput -PathType Leaf) 'Malformed source was removed after conversion failure'
    Assert-Condition ((Get-Item -LiteralPath $malformedInput).Length -eq 5) 'Malformed source changed after conversion failure'
    Assert-Condition (-not (Test-Path -LiteralPath (Join-Path $runRoot 'malformed-input.mkv'))) 'Malformed queue fixture published an output'
    $partialArtifacts = @(Get-ChildItem -LiteralPath $runRoot -Recurse -Force -File | Where-Object {
        $_.Name -like '*.videoferry-partial-*' -or $_.Name -like '*.videoferry-stage-*'
    })
    Assert-Condition ($partialArtifacts.Count -eq 0) 'Malformed queue fixture left a staged or partial output'

    $storedQueue = [IO.File]::ReadAllText($queueStatePath) | ConvertFrom-Json
    Assert-Condition ($storedQueue.tasks.Count -eq 2) 'Malformed queue fixture changed the persisted task count'
    $storedMalformedTask = $storedQueue.tasks | Where-Object { $_.task_data_id -eq 'package-smoke-malformed-task' } | Select-Object -First 1
    Assert-Condition ($null -ne $storedMalformedTask -and $storedMalformedTask.status -eq 'completed') 'Completed-with-failure task was not persisted as completed'
    Assert-Condition ($storedMalformedTask.error -eq '1 file(s) failed') 'Failed-file count was not persisted'

    $provider = $retry.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
    $provider.Invoke()
    Start-Sleep -Milliseconds 250
    $elements = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
    $runSelected = Find-AccessibleElement $elements 'Run selected' 'ControlType.Button'
    Assert-Condition ($null -ne $runSelected -and $runSelected.Current.IsEnabled) 'Rerun did not restore the task to a runnable pending state'
    Assert-Condition ($null -ne (Find-AccessibleElement $elements 'Pending' 'ControlType.Text')) 'Rerun did not expose the pending row status'
    $storedQueue = [IO.File]::ReadAllText($queueStatePath) | ConvertFrom-Json
    $storedMalformedTask = $storedQueue.tasks | Where-Object { $_.task_data_id -eq 'package-smoke-malformed-task' } | Select-Object -First 1
    Assert-Condition ($storedMalformedTask.status -eq 'pending') 'Rerun did not persist the pending task state'
    Assert-Condition ($null -eq $storedMalformedTask.error) 'Rerun did not clear the persisted task error'

    $successfulRow = Assert-ModeControl $elements 'Successful media safety fixture' 'ControlType.Button'
    Assert-AccessiblePattern $successfulRow ([System.Windows.Automation.InvokePattern]::Pattern) 'Successful queue fixture row'
    $provider = $successfulRow.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
    $provider.Invoke()
    Start-Sleep -Milliseconds 250
    $elements = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
    $runSelected = Find-AccessibleElement $elements 'Run selected' 'ControlType.Button'
    Assert-Condition ($null -ne $runSelected -and $runSelected.Current.IsEnabled) 'Selecting the valid fixture did not enable Run selected'
    $provider = $runSelected.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
    $provider.Invoke()

    $historyTab = $null
    for ($attempt = 0; $attempt -lt 120 -and $null -eq $historyTab; $attempt++) {
        Start-Sleep -Milliseconds 250
        $process.Refresh()
        if ($process.HasExited) {
            throw "Packaged application exited while processing the valid queue fixture with code $($process.ExitCode)"
        }
        $elements = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
        $historyTab = Find-AccessibleElement $elements 'Completed history (1)' 'ControlType.Button'
    }
    Assert-Condition ($null -ne $historyTab) 'Valid queue fixture did not create one completed-history entry'

    $successfulOutput = Join-Path $runRoot 'successful-input.mkv'
    $successfulBackup = Join-Path $runRoot 'original\successful-input.mp4'
    Assert-Condition (-not (Test-Path -LiteralPath $successfulInput)) 'Converted source was not moved into original/'
    Assert-Condition (Test-Path -LiteralPath $successfulOutput -PathType Leaf) 'Valid queue fixture did not publish an output'
    Assert-Condition ((Get-Item -LiteralPath $successfulOutput).Length -gt 0) 'Valid queue fixture published an empty output'
    Assert-Condition (Test-Path -LiteralPath $successfulBackup -PathType Leaf) 'Valid queue fixture did not preserve the original source'
    Assert-Condition ((Get-FileSha256 $successfulBackup) -eq '4A1A967B4DC9C1417C5CDD6E7AEDC67FFC3C0667EEF08E27B815369BBF3B6FE9') 'Original backup does not match the synthetic input'
    $partialArtifacts = @(Get-ChildItem -LiteralPath $runRoot -Recurse -Force -File | Where-Object {
        $_.Name -like '*.videoferry-partial-*' -or $_.Name -like '*.videoferry-stage-*'
    })
    Assert-Condition ($partialArtifacts.Count -eq 0) 'Successful queue fixture left a staged or partial output'

    $storedQueue = [IO.File]::ReadAllText($queueStatePath) | ConvertFrom-Json
    Assert-Condition ($storedQueue.tasks.Count -eq 2) 'Successful queue fixture changed the persisted task count'
    $storedMalformedTask = $storedQueue.tasks | Where-Object { $_.task_data_id -eq 'package-smoke-malformed-task' } | Select-Object -First 1
    $storedSuccessfulTask = $storedQueue.tasks | Where-Object { $_.task_data_id -eq 'package-smoke-successful-task' } | Select-Object -First 1
    Assert-Condition ($null -ne $storedMalformedTask -and $storedMalformedTask.status -eq 'pending') 'Successful conversion changed the rerunnable malformed task'
    Assert-Condition ($null -ne $storedSuccessfulTask -and $storedSuccessfulTask.status -eq 'completed') 'Successful task was not persisted as completed'
    Assert-Condition (-not [string]::IsNullOrWhiteSpace($storedSuccessfulTask.complete_time)) 'Successful task completion time was not persisted'
    Assert-Condition ($null -eq $storedSuccessfulTask.error) 'Successful task persisted an unexpected error'

    $historyPath = Join-Path $stateDirectory 'completed_history.json'
    Assert-Condition (Test-Path -LiteralPath $historyPath -PathType Leaf) 'Completed history file was not created'
    $storedHistory = [IO.File]::ReadAllText($historyPath) | ConvertFrom-Json -NoEnumerate
    Assert-Condition ($storedHistory.Count -eq 1) 'Completed history does not contain exactly one row'
    Assert-Condition ($storedHistory[0].Count -eq 14) 'Completed history row does not match the Rust fourteen-column schema'
    Assert-Condition ($storedHistory[0][0] -eq $successfulOutput) 'Completed history output path is incorrect'
    Assert-Condition ($storedHistory[0][1] -eq 'TV') 'Completed history workflow is incorrect'
    Assert-Condition ($storedHistory[0][8] -eq '64x64') 'Completed history source resolution is incorrect'
    Assert-Condition ($storedHistory[0][9] -eq '10') 'Completed history source FPS is incorrect'
    Assert-Condition ($storedHistory[0][10] -eq '10') 'Completed history output FPS is incorrect'
    Assert-Condition ($storedHistory[0][11] -eq 'x265') 'Completed history encoder is incorrect'
    Assert-Condition ($storedHistory[0][12] -eq '32') 'Completed history quality is incorrect'
    Assert-Condition ($storedHistory[0][13] -eq 'ultrafast') 'Completed history preset is incorrect'

    Assert-AccessiblePattern $historyTab ([System.Windows.Automation.InvokePattern]::Pattern) 'Completed history tab'
    $provider = $historyTab.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
    $provider.Invoke()
    Start-Sleep -Milliseconds 250
    $elements = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
    $clearHistory = Find-AccessibleElement $elements 'Clear history' 'ControlType.Button'
    Assert-Condition ($null -ne $clearHistory -and $clearHistory.Current.IsEnabled) 'Completed-history view did not expose its populated state'
    Assert-Condition ($null -ne (Find-AccessibleElement $elements $successfulOutput 'ControlType.Text')) 'Completed-history view did not expose the converted output path'

    $provider = $clearHistory.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
    $provider.Invoke()
    Start-Sleep -Milliseconds 250
    $storedHistoryJson = [IO.File]::ReadAllText($historyPath)
    Assert-Condition ($storedHistoryJson -match '^\s*\[\s*\]\s*$') 'Clear history did not persist an empty Rust history list'
    $elements = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
    $emptyHistoryTab = Find-AccessibleElement $elements 'Completed history (0)' 'ControlType.Button'
    Assert-Condition ($null -ne $emptyHistoryTab) 'Clear history did not update the history count'
    $clearHistory = Find-AccessibleElement $elements 'Clear history' 'ControlType.Button'
    Assert-Condition ($null -ne $clearHistory -and -not $clearHistory.Current.IsEnabled) 'Clear history remained enabled after the history was emptied'

    $queueTab = Find-AccessibleElement $elements 'Conversion queue' 'ControlType.Button'
    Assert-Condition ($null -ne $queueTab) 'Conversion queue tab disappeared after clearing history'
    $provider = $queueTab.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
    $provider.Invoke()
    Start-Sleep -Milliseconds 250
    $elements = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
    $successfulRow = Assert-ModeControl $elements 'Successful media safety fixture' 'ControlType.Button'
    $provider = $successfulRow.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
    $provider.Invoke()
    Start-Sleep -Milliseconds 250
    $elements = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
    $removeSelected = Find-AccessibleElement $elements 'Remove selected' 'ControlType.Button'
    Assert-Condition ($null -ne $removeSelected -and $removeSelected.Current.IsEnabled) 'Selecting a completed task did not enable Remove selected'
    $provider = $removeSelected.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
    $provider.Invoke()
    Start-Sleep -Milliseconds 250
    $storedQueue = [IO.File]::ReadAllText($queueStatePath) | ConvertFrom-Json
    Assert-Condition (@($storedQueue.tasks).Count -eq 1) 'Remove selected did not persist exactly one remaining queue task'
    Assert-Condition ($storedQueue.tasks[0].task_data_id -eq 'package-smoke-malformed-task') 'Remove selected removed the wrong queue task'

    $elements = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
    $clearQueue = Find-AccessibleElement $elements 'Clear queue' 'ControlType.Button'
    Assert-Condition ($null -ne $clearQueue -and $clearQueue.Current.IsEnabled) 'Clear queue was not enabled while the worker was idle'
    $provider = $clearQueue.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
    $provider.Invoke()
    Start-Sleep -Milliseconds 250
    $storedQueue = [IO.File]::ReadAllText($queueStatePath) | ConvertFrom-Json
    Assert-Condition (@($storedQueue.tasks).Count -eq 0) 'Clear queue did not persist an empty task list'

    $process.Refresh()
    Stop-Process -Id $process.Id -Force
    $process.WaitForExit()
    $process.Dispose()
    $process = $null

    $recoveryInput = Join-Path $runRoot 'restart-recovery-input.mp4'
    [IO.File]::WriteAllBytes($recoveryInput, $syntheticBytes)
    $alreadyCompletedInput = Join-Path $runRoot 'already-completed-input.mp4'
    $alreadyCompletedBackup = Join-Path $runRoot 'original\already-completed-input.mp4'
    [IO.File]::WriteAllBytes($alreadyCompletedBackup, $syntheticBytes)
    Assert-Condition (-not (Test-Path -LiteralPath $alreadyCompletedInput)) 'Aggregate recovery fixture unexpectedly has an already-completed source'
    $recoveryTask = $queueState.tasks[1].Clone()
    $recoveryTask.name = 'Interrupted queue recovery fixture'
    $recoveryTask.target_paths = @($alreadyCompletedInput, $recoveryInput)
    $recoveryTask.task_data_id = 'package-smoke-recovery-task'
    $recoveryTask.queued_time = '2026-08-25 00:00:02'
    $recoveryTask.status = 'pending'
    $recoveryTask.complete_time = ''
    $recoveryTask.error = $null
    $recoveryState = @{
        version = 2
        was_running = $false
        tasks = @($recoveryTask)
    }
    [IO.File]::WriteAllText(
        $queueStatePath,
        ($recoveryState | ConvertTo-Json -Depth 8),
        [Text.UTF8Encoding]::new($false)
    )
    $lockedStream = [IO.File]::Open(
        $recoveryInput,
        [IO.FileMode]::Open,
        [IO.FileAccess]::ReadWrite,
        [IO.FileShare]::None
    )

    $process = Start-Process -FilePath $applications[0].FullName `
        -WorkingDirectory $unrelatedWorkingDirectory -WindowStyle Hidden -PassThru
    Start-Sleep -Seconds $LaunchSeconds
    $process.Refresh()
    Assert-Condition (-not $process.HasExited) 'Packaged application exited before the interruption-recovery test began'
    $window = Get-AppWindow $process.Id
    Assert-Condition ($null -ne $window) 'Interruption-recovery launch did not expose a Windows UI Automation window'

    $runQueue = $null
    for ($attempt = 0; $attempt -lt 40 -and $null -eq $runQueue; $attempt++) {
        $elements = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
        $candidate = Find-AccessibleElement $elements 'Run queue' 'ControlType.Button'
        if ($null -ne $candidate -and $candidate.Current.IsEnabled) {
            $runQueue = $candidate
            break
        }
        Start-Sleep -Milliseconds 250
    }
    Assert-Condition ($null -ne $runQueue) 'Recovered fixture did not expose an enabled Run queue control'
    $provider = $runQueue.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
    $provider.Invoke()

    $persistedActiveQueue = $null
    for ($attempt = 0; $attempt -lt 40 -and $null -eq $persistedActiveQueue; $attempt++) {
        Start-Sleep -Milliseconds 250
        $candidate = [IO.File]::ReadAllText($queueStatePath) | ConvertFrom-Json
        if ($candidate.was_running -and $candidate.tasks[0].status -eq 'running') {
            $persistedActiveQueue = $candidate
        }
    }
    Assert-Condition ($null -ne $persistedActiveQueue) 'Active queue state was not persisted before interruption'
    $elements = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
    $stopCurrent = Find-AccessibleElement $elements 'Stop current' 'ControlType.Button'
    Assert-Condition ($null -ne $stopCurrent -and $stopCurrent.Current.IsEnabled) 'Locked input did not keep a real conversion worker active'
    Assert-Condition ($null -ne (Find-AccessibleElement $elements 'File #:' 'ControlType.Text')) 'Active progress did not expose the Python-compatible File # field'
    Assert-Condition ($null -ne (Find-AccessibleElement $elements '2/2' 'ControlType.Text')) 'Aggregate recovery worker did not report the current file as 2/2'
    Assert-Condition ($null -ne (Find-AccessibleElement $elements 'Camera Model:' 'ControlType.Text')) 'Active progress did not expose the Python-compatible Camera Model field'
    Assert-Condition ($null -ne (Find-AccessibleElement $elements 'Applying LUT:' 'ControlType.Text')) 'Active progress did not expose the Python-compatible Applying LUT field'

    $pause = Find-AccessibleElement $elements 'Pause' 'ControlType.Button'
    Assert-Condition ($null -ne $pause -and $pause.Current.IsEnabled) 'Active worker did not expose an enabled Pause control'
    $provider = $pause.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
    $provider.Invoke()
    $resume = $null
    for ($attempt = 0; $attempt -lt 20 -and $null -eq $resume; $attempt++) {
        Start-Sleep -Milliseconds 100
        $elements = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
        $resume = Find-AccessibleElement $elements 'Resume' 'ControlType.Button'
    }
    Assert-Condition ($null -ne $resume -and $resume.Current.IsEnabled) 'Pause did not switch the active worker to Resume'
    Assert-Condition ($null -ne (Find-AccessibleElement $elements 'Paused' 'ControlType.Text')) 'Pause did not expose the paused queue status'
    $pausedQueue = [IO.File]::ReadAllText($queueStatePath) | ConvertFrom-Json
    Assert-Condition ($pausedQueue.was_running -and $pausedQueue.tasks[0].status -eq 'running') 'Paused worker was not persisted as recoverable active work'

    $provider = $resume.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
    $provider.Invoke()
    $pause = $null
    for ($attempt = 0; $attempt -lt 20 -and $null -eq $pause; $attempt++) {
        Start-Sleep -Milliseconds 100
        $elements = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
        $pause = Find-AccessibleElement $elements 'Pause' 'ControlType.Button'
    }
    Assert-Condition ($null -ne $pause -and $pause.Current.IsEnabled) 'Resume did not restore the active Pause control'
    Assert-Condition ($null -ne (Find-AccessibleElement $elements 'Running' 'ControlType.Text')) 'Resume did not restore the running queue status'

    $pauseAfterCurrent = Find-AccessibleElement $elements 'Pause after current' 'ControlType.Button'
    Assert-Condition ($null -ne $pauseAfterCurrent -and $pauseAfterCurrent.Current.IsEnabled) 'Running queue did not expose Pause after current'
    $provider = $pauseAfterCurrent.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
    $provider.Invoke()
    Start-Sleep -Milliseconds 200
    $elements = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
    $pauseAfterCurrent = Find-AccessibleElement $elements 'Pause after current' 'ControlType.Button'
    Assert-Condition ($null -ne $pauseAfterCurrent -and -not $pauseAfterCurrent.Current.IsEnabled) 'Pause after current remained enabled after activation'

    $childProcesses = @(Get-CimInstance Win32_Process -Filter "ParentProcessId = $($process.Id)")
    Assert-Condition ($childProcesses.Count -eq 0) 'Direct FFmpeg conversion unexpectedly launched a child process'
    $processesTab = Find-AccessibleElement $elements 'Processes' 'ControlType.Button'
    Assert-Condition ($null -ne $processesTab) 'Processes tab disappeared while conversion was active'
    $provider = $processesTab.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
    $provider.Invoke()
    Start-Sleep -Milliseconds 250
    $elements = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
    Assert-Condition ($null -ne (Find-AccessibleElement $elements 'Native FFmpeg worker' 'ControlType.Text')) 'Processes view did not expose the in-process native worker'
    Assert-Condition ($null -ne (Find-AccessibleElement $elements $process.Id.ToString() 'ControlType.Text')) 'Processes view did not report the application process ID'
    Assert-Condition ($null -ne (Find-AccessibleElement $elements $recoveryInput 'ControlType.Text')) 'Processes view did not report the active input'

    Stop-Process -Id $process.Id -Force
    $process.WaitForExit()
    $process.Dispose()
    $process = $null
    $lockedStream.Dispose()
    $lockedStream = $null

    Assert-Condition (Test-Path -LiteralPath $recoveryInput -PathType Leaf) 'Interrupted conversion removed its source'
    Assert-Condition ((Get-FileSha256 $recoveryInput) -eq '4A1A967B4DC9C1417C5CDD6E7AEDC67FFC3C0667EEF08E27B815369BBF3B6FE9') 'Interrupted conversion changed its source'
    Assert-Condition (-not (Test-Path -LiteralPath (Join-Path $runRoot 'restart-recovery-input.mkv'))) 'Interrupted conversion published an output before restart'
    $partialArtifacts = @(Get-ChildItem -LiteralPath $runRoot -Recurse -Force -File | Where-Object {
        $_.Name -like '*.videoferry-partial-*' -or $_.Name -like '*.videoferry-stage-*'
    })
    Assert-Condition ($partialArtifacts.Count -eq 0) 'Interrupted locked-input conversion left a staged or partial output'

    $process = Start-Process -FilePath $applications[0].FullName `
        -WorkingDirectory $unrelatedWorkingDirectory -WindowStyle Hidden -PassThru
    Start-Sleep -Seconds $LaunchSeconds
    $process.Refresh()
    Assert-Condition (-not $process.HasExited) 'Packaged application exited while restarting the interrupted queue'
    $window = Get-AppWindow $process.Id
    Assert-Condition ($null -ne $window) 'Restarted packaged application did not expose a Windows UI Automation window'

    $recoveredHistoryTab = $null
    for ($attempt = 0; $attempt -lt 120 -and $null -eq $recoveredHistoryTab; $attempt++) {
        Start-Sleep -Milliseconds 250
        $process.Refresh()
        if ($process.HasExited) {
            throw "Packaged application exited while resuming the interrupted queue with code $($process.ExitCode)"
        }
        $elements = $window.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
        $recoveredHistoryTab = Find-AccessibleElement $elements 'Completed history (1)' 'ControlType.Button'
    }
    Assert-Condition ($null -ne $recoveredHistoryTab) 'Restarted application did not automatically finish the persisted active queue'

    $recoveryOutput = Join-Path $runRoot 'restart-recovery-input.mkv'
    $recoveryBackup = Join-Path $runRoot 'original\restart-recovery-input.mp4'
    Assert-Condition (-not (Test-Path -LiteralPath $recoveryInput)) 'Restart recovery did not move the completed source into original/'
    Assert-Condition (Test-Path -LiteralPath $recoveryOutput -PathType Leaf) 'Restart recovery did not publish the converted output'
    Assert-Condition ((Get-Item -LiteralPath $recoveryOutput).Length -gt 0) 'Restart recovery published an empty output'
    Assert-Condition (Test-Path -LiteralPath $recoveryBackup -PathType Leaf) 'Restart recovery did not preserve the original source'
    Assert-Condition ((Get-FileSha256 $recoveryBackup) -eq '4A1A967B4DC9C1417C5CDD6E7AEDC67FFC3C0667EEF08E27B815369BBF3B6FE9') 'Restart recovery backup does not match the interrupted source'
    $storedQueue = [IO.File]::ReadAllText($queueStatePath) | ConvertFrom-Json
    Assert-Condition (-not $storedQueue.was_running) 'Restart recovery left the persisted queue marked as running'
    Assert-Condition ($storedQueue.tasks[0].status -eq 'completed') 'Restart recovery did not persist task completion'
    $storedHistory = [IO.File]::ReadAllText($historyPath) | ConvertFrom-Json -NoEnumerate
    Assert-Condition ($storedHistory.Count -eq 1) 'Restart recovery did not create exactly one completed-history row'
    Assert-Condition ($storedHistory[0][0] -eq $recoveryOutput) 'Restart recovery history points to the wrong output'
    $partialArtifacts = @(Get-ChildItem -LiteralPath $runRoot -Recurse -Force -File | Where-Object {
        $_.Name -like '*.videoferry-partial-*' -or $_.Name -like '*.videoferry-stage-*'
    })
    Assert-Condition ($partialArtifacts.Count -eq 0) 'Restart recovery left a staged or partial output'

    Write-Output "Clean portable GUI smoke passed ($($requiredControls.Count) startup controls, all 6 workflows, About/runtime licensing, live File # and camera/LUT progress, pause/resume/pause-after-current, in-process worker proof, durable queue/history editing, failure/rerun, successful backup/history, and interruption/restart recovery lifecycles): $Archive"
} finally {
    if ($null -ne $lockedStream) {
        $lockedStream.Dispose()
    }
    if ($null -ne $process) {
        $process.Refresh()
        if (-not $process.HasExited) {
            Stop-Process -Id $process.Id -Force
            $process.WaitForExit()
        }
        $process.Dispose()
    }
    foreach ($name in $savedEnvironment.Keys) {
        [Environment]::SetEnvironmentVariable($name, $savedEnvironment[$name], 'Process')
    }
    Assert-SmokeChild $runRoot
    if (Test-Path -LiteralPath $runRoot) {
        Remove-Item -LiteralPath $runRoot -Recurse -Force
    }
}
