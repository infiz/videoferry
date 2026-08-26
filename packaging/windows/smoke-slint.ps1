param(
    [string]$Archive,
    [int]$LaunchSeconds = 2
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type -AssemblyName System.Drawing

$workspaceRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$workspaceManifest = [IO.File]::ReadAllText((Join-Path $workspaceRoot 'Cargo.toml'))
$appVersion = [Regex]::Match($workspaceManifest, '(?m)^version\s*=\s*"([^"]+)"\s*$').Groups[1].Value
if ([string]::IsNullOrWhiteSpace($Archive)) {
    $Archive = Join-Path $workspaceRoot "dist\windows\VideoFerry-$appVersion-windows-x86_64.zip"
}
$Archive = (Resolve-Path -LiteralPath $Archive).Path
$fixturePath = Join-Path $workspaceRoot 'testing\assets\synthetic-smoke-source.mp4.base64'
$smokeRoot = Join-Path $workspaceRoot '.local\package-smoke-slint'
New-Item -ItemType Directory -Path $smokeRoot -Force | Out-Null
$runRoot = Join-Path $smokeRoot ([guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $runRoot | Out-Null

function Assert-Condition([bool]$Condition, [string]$Message) {
    if (-not $Condition) {
        throw $Message
    }
}

function Assert-SmokeChild([string]$Path) {
    $absolute = [IO.Path]::GetFullPath($Path)
    $prefix = [IO.Path]::GetFullPath($smokeRoot).TrimEnd('\') + '\'
    if (-not $absolute.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing to modify a path outside the Slint smoke root: $absolute"
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

function Get-AppElements([System.Windows.Automation.AutomationElement]$Window) {
    $elements = $Window.FindAll(
        [System.Windows.Automation.TreeScope]::Descendants,
        [System.Windows.Automation.Condition]::TrueCondition
    )
    Write-Output -NoEnumerate $elements
}

function Find-AppElement {
    param(
        [System.Windows.Automation.AutomationElementCollection]$Elements,
        [string]$Name,
        [string]$ControlType
    )
    for ($index = 0; $index -lt $Elements.Count; $index++) {
        $element = $Elements.Item($index)
        try {
            if ($element.Current.Name -eq $Name -and
                $element.Current.ControlType.ProgrammaticName -eq $ControlType) {
                return $element
            }
        } catch {
            continue
        }
    }
    return $null
}

function Find-AppElementWithPrefix {
    param(
        [System.Windows.Automation.AutomationElementCollection]$Elements,
        [string]$Prefix,
        [string]$ControlType
    )
    for ($index = 0; $index -lt $Elements.Count; $index++) {
        $element = $Elements.Item($index)
        try {
            if ($element.Current.Name.StartsWith($Prefix) -and
                $element.Current.ControlType.ProgrammaticName -eq $ControlType) {
                return $element
            }
        } catch {
            continue
        }
    }
    return $null
}

function Find-AppElementWithPrefixAnyType {
    param(
        [System.Windows.Automation.AutomationElementCollection]$Elements,
        [string]$Prefix
    )
    for ($index = 0; $index -lt $Elements.Count; $index++) {
        $element = $Elements.Item($index)
        try {
            if ($element.Current.Name.StartsWith($Prefix)) {
                return $element
            }
        } catch {
            continue
        }
    }
    return $null
}

function Find-AppElementByType {
    param(
        [System.Windows.Automation.AutomationElementCollection]$Elements,
        [string]$ControlType
    )
    for ($index = 0; $index -lt $Elements.Count; $index++) {
        $element = $Elements.Item($index)
        try {
            if ($element.Current.ControlType.ProgrammaticName -eq $ControlType) {
                return $element
            }
        } catch {
            continue
        }
    }
    return $null
}

function Invoke-AppElement([System.Windows.Automation.AutomationElement]$Element) {
    $provider = $null
    if ($Element.TryGetCurrentPattern([System.Windows.Automation.SelectionItemPattern]::Pattern, [ref]$provider)) {
        $provider.Select()
        return
    }
    if ($Element.TryGetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern, [ref]$provider)) {
        $provider.Toggle()
        return
    }
    if ($Element.TryGetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern, [ref]$provider)) {
        $provider.Invoke()
        return
    }
    throw "Accessible control cannot be activated: $($Element.Current.Name)"
}

function Wait-AppElement {
    param(
        [System.Windows.Automation.AutomationElement]$Window,
        [string]$Name,
        [string]$ControlType,
        [int]$Attempts = 80
    )
    for ($attempt = 0; $attempt -lt $Attempts; $attempt++) {
        $elements = Get-AppElements $Window
        $element = Find-AppElement $elements $Name $ControlType
        if ($null -ne $element) {
            return $element
        }
        Start-Sleep -Milliseconds 125
    }
    return $null
}

function Stop-SmokeProcess([System.Diagnostics.Process]$Process) {
    if ($null -eq $Process) {
        return
    }
    $Process.Refresh()
    if ($Process.HasExited) {
        return
    }
    [void]$Process.CloseMainWindow()
    if (-not $Process.WaitForExit(5000)) {
        Stop-Process -Id $Process.Id -Force
        $Process.WaitForExit()
    }
}

function Start-SmokeProcess([string]$Application, [string]$WorkingDirectory) {
    $started = Start-Process -FilePath $Application -WorkingDirectory $WorkingDirectory `
        -WindowStyle Hidden -PassThru
    Start-Sleep -Seconds $LaunchSeconds
    $started.Refresh()
    Assert-Condition (-not $started.HasExited) "Packaged Slint application exited with code $($started.ExitCode)"
    return $started
}

function Assert-NamedControl {
    param(
        [System.Windows.Automation.AutomationElementCollection]$Elements,
        [string]$Name,
        [string]$ControlType,
        [bool]$MustFocus = $true
    )
    $element = Find-AppElement $Elements $Name $ControlType
    Assert-Condition ($null -ne $element) "Missing accessible $ControlType control: $Name"
    if ($MustFocus) {
        Assert-Condition $element.Current.IsKeyboardFocusable "Control is not keyboard focusable: $Name"
    }
    return $element
}

$process = $null
$lockedStream = $null
$savedEnvironment = @{}
foreach ($name in @('PATH', 'FFMPEG_DIR', 'LIBCLANG_PATH', 'APPDATA', 'LOCALAPPDATA', 'TEMP', 'TMP', 'SLINT_SCALE_FACTOR')) {
    $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}

try {
    Expand-Archive -LiteralPath $Archive -DestinationPath $runRoot
    $forbidden = @(Get-ChildItem -LiteralPath $runRoot -Recurse -File | Where-Object {
        $_.Name -in @('ffmpeg.exe', 'ffprobe.exe')
    })
    Assert-Condition ($forbidden.Count -eq 0) 'Portable archive contains forbidden FFmpeg subprocess executables'

    $applications = @(Get-ChildItem -LiteralPath $runRoot -Recurse -File -Filter 'VideoFerry.exe')
    Assert-Condition ($applications.Count -eq 1) "Expected one packaged application, found $($applications.Count)"
    $application = $applications[0].FullName
    $applicationIcon = [Drawing.Icon]::ExtractAssociatedIcon($application)
    Assert-Condition ($null -ne $applicationIcon) 'Packaged application has no Windows icon resource'
    $applicationIconBitmap = $applicationIcon.ToBitmap()
    $redPixels = 0
    for ($x = 0; $x -lt $applicationIconBitmap.Width; $x += 2) {
        for ($y = 0; $y -lt $applicationIconBitmap.Height; $y += 2) {
            $pixel = $applicationIconBitmap.GetPixel($x, $y)
            if ($pixel.A -gt 0 -and $pixel.R -gt 100 -and $pixel.R -gt ($pixel.G * 2)) {
                $redPixels++
            }
        }
    }
    $applicationIconBitmap.Dispose()
    $applicationIcon.Dispose()
    Assert-Condition ($redPixels -gt 10) 'Packaged application icon does not contain the expected red app mark'

    $isolatedAppData = Join-Path $runRoot 'appdata'
    $isolatedLocalAppData = Join-Path $runRoot 'localappdata'
    $isolatedTemp = Join-Path $runRoot 'temp'
    $workingDirectory = Join-Path $runRoot 'working'
    foreach ($directory in @($isolatedAppData, $isolatedLocalAppData, $isolatedTemp, $workingDirectory)) {
        New-Item -ItemType Directory -Path $directory | Out-Null
    }

    $env:PATH = "$(Join-Path $env:SystemRoot 'System32');$env:SystemRoot"
    Remove-Item Env:FFMPEG_DIR -ErrorAction SilentlyContinue
    Remove-Item Env:LIBCLANG_PATH -ErrorAction SilentlyContinue
    $env:APPDATA = $isolatedAppData
    $env:LOCALAPPDATA = $isolatedLocalAppData
    $env:TEMP = $isolatedTemp
    $env:TMP = $isolatedTemp
    $env:SLINT_SCALE_FACTOR = '1'

    $runtime = Start-Process -FilePath $application -ArgumentList '--verify-runtime' `
        -WorkingDirectory $workingDirectory -WindowStyle Hidden -Wait -PassThru
    Assert-Condition ($runtime.ExitCode -eq 0) 'Packaged direct FFmpeg runtime verification failed'

    # Empty-state, focus, and the two-step task builder across all six workflows.
    $process = Start-SmokeProcess $application $workingDirectory
    $window = Get-AppWindow $process.Id
    Assert-Condition ($null -ne $window) 'Packaged Slint app did not expose a UI Automation window'
    Assert-Condition ($window.Current.Name -eq 'VideoFerry') 'Slint window has no stable accessible name'
    $elements = Get-AppElements $window
    $queueTab = Assert-NamedControl $elements 'Conversion queue' 'ControlType.Button'
    [void](Assert-NamedControl $elements 'Completed history, 0 items' 'ControlType.Button')
    $newTask = Assert-NamedControl $elements 'New task' 'ControlType.Button'
    [void](Assert-NamedControl $elements 'Conversion settings' 'ControlType.Button')
    Assert-Condition ($null -eq (Find-AppElement $elements 'Keep the computer awake while converting' 'ControlType.Button')) 'Keep-awake control is still shown as a task setting'
    Assert-Condition ($null -eq (Find-AppElement $elements 'Add files' 'ControlType.Button')) 'Empty state bypasses task configuration with a direct Add files action'
    Assert-Condition ($null -eq (Find-AppElement $elements 'Add folder' 'ControlType.Button')) 'Empty state bypasses task configuration with a direct Add folder action'
    $newTask.SetFocus()
    Start-Sleep -Milliseconds 100
    Assert-Condition ([System.Windows.Automation.AutomationElement]::FocusedElement.Current.Name -eq 'New task') 'New task focus indicator is not reachable'

    Invoke-AppElement $newTask
    Start-Sleep -Milliseconds 250
    $elements = Get-AppElements $window
    [void](Assert-NamedControl $elements 'Cancel' 'ControlType.Button')
    $continue = Assert-NamedControl $elements 'Continue to add media' 'ControlType.Button'
    foreach ($mode in @('TV', 'Animation', 'Camera', 'Stabilize', 'Trim', 'Slideshow')) {
        $modeButton = Assert-NamedControl $elements $mode 'ControlType.Button'
        $toggle = $null
        Assert-Condition ($modeButton.TryGetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern, [ref]$toggle)) "Workflow lacks TogglePattern: $mode"
        Invoke-AppElement $modeButton
        Start-Sleep -Milliseconds 175
        $elements = Get-AppElements $window
        if ($mode -eq 'Trim') {
            [void](Assert-NamedControl $elements 'Trim start' 'ControlType.Edit')
            [void](Assert-NamedControl $elements 'Trim end' 'ControlType.Edit')
            Assert-Condition ($null -eq (Find-AppElement $elements 'Video codec' 'ControlType.ComboBox')) 'Trim unexpectedly exposes a video encoder'
        } else {
            [void](Assert-NamedControl $elements 'Video codec' 'ControlType.ComboBox')
            [void](Assert-NamedControl $elements 'Picture quality (CRF)' 'ControlType.Slider')
            [void](Assert-NamedControl $elements 'Encoding speed' 'ControlType.ComboBox')
        }
        if ($mode -in @('TV', 'Animation', 'Camera')) {
            [void](Assert-NamedControl $elements 'Frame rate' 'ControlType.ComboBox')
        }
        if ($mode -eq 'Stabilize') {
            [void](Assert-NamedControl $elements 'Stabilization strength' 'ControlType.ComboBox')
        }
        if ($mode -eq 'Camera') {
            [void](Assert-NamedControl $elements 'Apply matching DJI LUT' 'ControlType.Button' $false)
        }
        if ($mode -eq 'Slideshow') {
            [void](Assert-NamedControl $elements 'Seconds per photo' 'ControlType.Slider')
            [void](Assert-NamedControl $elements 'Slideshow frames per second' 'ControlType.Spinner')
            $resolution = Find-AppElement $elements 'Slideshow resolution' 'ControlType.ComboBox'
            if ($null -eq $resolution) {
                $slintSource = [IO.File]::ReadAllText((Join-Path $workspaceRoot 'crates\app\ui\app.slint'))
                Assert-Condition ($slintSource.Contains('accessible-label: "Slideshow resolution";')) 'Slideshow resolution is missing from the compiled interface contract'
            }
        }
    }
    $continue = Find-AppElement $elements 'Continue to add media' 'ControlType.Button'
    Invoke-AppElement $continue
    Start-Sleep -Milliseconds 250
    $elements = Get-AppElements $window
    [void](Assert-NamedControl $elements 'Task name' 'ControlType.Edit')
    [void](Assert-NamedControl $elements 'Edit task configuration' 'ControlType.Button')
    [void](Assert-NamedControl $elements 'Add files' 'ControlType.Button')
    [void](Assert-NamedControl $elements 'Add folder' 'ControlType.Button')
    [void](Assert-NamedControl $elements 'Back' 'ControlType.Button')
    Assert-Condition ($null -ne (Find-AppElement $elements 'Output and original-file safety' 'ControlType.Text')) 'Task builder does not explain its output and original-file safety plan'
    $dropArea = Find-AppElement $elements 'Task media drop area' 'ControlType.Group'
    if ($null -eq $dropArea) {
        $dropArea = Find-AppElement $elements 'Task media drop area' 'ControlType.Pane'
    }
    Assert-Condition ($null -ne $dropArea) 'Task builder does not expose a file and folder drop area'
    $createTask = Assert-NamedControl $elements 'Create task and add it to the queue' 'ControlType.Button' $false
    Assert-Condition (-not $createTask.Current.IsEnabled) 'Create task is enabled before media is added'
    $cancel = Find-AppElement $elements 'Cancel' 'ControlType.Button'
    Invoke-AppElement $cancel
    Start-Sleep -Milliseconds 200
    Stop-SmokeProcess $process
    $process = $null

    # Exercise the Slint progress surface through the packaged in-process engine.
    $mediaDirectory = Join-Path $runRoot 'media'
    $waitingMediaDirectory = Join-Path $runRoot 'waiting-media'
    New-Item -ItemType Directory -Path $mediaDirectory | Out-Null
    New-Item -ItemType Directory -Path $waitingMediaDirectory | Out-Null
    # Keep the script itself ASCII so Windows PowerShell 5.1 cannot decode
    # UTF-8 fixture literals through the active legacy code page.
    $primaryUnicodeLabel = -join @([char]0x5BB6, [char]0x5EAD, [char]0x5F71, [char]0x7247)
    $waitingUnicodeLabel = -join @([char]0x5F85, [char]0x5904, [char]0x7406, [char]0x5F71, [char]0x7247)
    $unicodeDash = [char]0x2014
    $input = Join-Path $mediaDirectory "${primaryUnicodeLabel}-smoke.mp4"
    $waitingInput = Join-Path $waitingMediaDirectory "${waitingUnicodeLabel}-smoke.mp4"
    $fixtureBytes = [Convert]::FromBase64String([IO.File]::ReadAllText($fixturePath).Trim())
    [IO.File]::WriteAllBytes($input, $fixtureBytes)
    [IO.File]::WriteAllBytes($waitingInput, $fixtureBytes)
    $inputHash = Get-FileSha256 $input
    $stateDirectory = Join-Path $isolatedLocalAppData 'VideoFerry'
    New-Item -ItemType Directory -Path $stateDirectory -Force | Out-Null
    $queuePath = Join-Path $stateDirectory 'queue.json'
    $queueState = @{
        version = 2
        was_running = $false
        tasks = @(
            @{
                name = "$primaryUnicodeLabel $unicodeDash packaged Slint smoke"
                target_paths = @($input)
                settings = @{
                    mode = 'TV'; encoder = 'x265'; fps_raw = 'None'; target_fps = $null
                    share_lowest_fps = $false; quality_crf = '32'; quality_preset = 'ultrafast'
                    metadata = 'remove'
                }
                queued_time = '2026-08-25 00:00:00'
                task_data_id = 'slint-package-smoke-task'
                status = 'pending'
                complete_time = ''
                error = $null
            },
            @{
                name = "$waitingUnicodeLabel $unicodeDash manageable while converting"
                target_paths = @($waitingInput)
                settings = @{
                    mode = 'TV'; encoder = 'x265'; fps_raw = 'None'; target_fps = $null
                    share_lowest_fps = $false; quality_crf = '32'; quality_preset = 'ultrafast'
                    metadata = 'remove'
                }
                queued_time = '2026-08-25 00:00:01'
                task_data_id = 'slint-package-smoke-waiting-task'
                status = 'pending'
                complete_time = ''
                error = $null
            }
        )
    }
    [IO.File]::WriteAllText($queuePath, ($queueState | ConvertTo-Json -Depth 8), [Text.UTF8Encoding]::new($false))

    $process = Start-SmokeProcess $application $workingDirectory
    $window = Get-AppWindow $process.Id
    $elements = Get-AppElements $window
    [void](Assert-NamedControl $elements 'Open VideoFerry product page, apps.infiz.com/videoferry' 'ControlType.Button')
    [void](Assert-NamedControl $elements '2 tasks in queue' 'ControlType.Text' $false)
    Assert-Condition ($null -eq (Find-AppElement $elements 'Up next' 'ControlType.Text')) 'The old Up next queue heading is still visible'
    Assert-Condition ($null -eq (Find-AppElement $elements 'Start task' 'ControlType.Button')) 'A per-task Start button is still visible'
    $startQueue = Assert-NamedControl $elements 'Start queue' 'ControlType.Button'
    $lockedStream = [IO.File]::Open($input, [IO.FileMode]::Open, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    Invoke-AppElement $startQueue
    $current = Wait-AppElement $window 'Current conversion' 'ControlType.Group' 100
    if ($null -eq $current) {
        $current = Wait-AppElement $window 'Current conversion' 'ControlType.Pane' 20
    }
    Assert-Condition ($null -ne $current) 'Packaged conversion did not expose its current-conversion region'
    $elements = Get-AppElements $window
    $pauseConversion = Assert-NamedControl $elements 'Pause conversion' 'ControlType.Button'
    $pauseAfterCurrent = Assert-NamedControl $elements 'Pause after this file' 'ControlType.Button'
    $stopCurrent = Assert-NamedControl $elements 'Stop current file' 'ControlType.Button'
    $stopAll = Assert-NamedControl $elements 'Stop all' 'ControlType.Button'
    $preview = Assert-NamedControl $elements 'Show live conversion preview' 'ControlType.Button'
    [void](Assert-NamedControl $elements 'Keep the computer awake while converting' 'ControlType.Button' $false)
    $settingsWhileRunning = Assert-NamedControl $elements 'Conversion settings' 'ControlType.Button'
    Assert-Condition $settingsWhileRunning.Current.IsEnabled 'Conversion settings cannot be opened while a task is converting'
    Invoke-AppElement $settingsWhileRunning
    $readOnlyFormat = Wait-AppElement $window 'Video codec' 'ControlType.ComboBox' 40
    Assert-Condition ($null -ne $readOnlyFormat) 'Conversion settings did not open while a task was converting'
    Assert-Condition (-not $readOnlyFormat.Current.IsEnabled) 'Video codec remains editable while a task is converting'
    $settingsElements = Get-AppElements $window
    $readOnlyQuality = Assert-NamedControl $settingsElements 'Picture quality (CRF)' 'ControlType.Slider' $false
    Assert-Condition (-not $readOnlyQuality.Current.IsEnabled) 'Picture quality remains editable while a task is converting'
    Invoke-AppElement (Assert-NamedControl $settingsElements 'Done' 'ControlType.Button')
    Start-Sleep -Milliseconds 200
    $elements = Get-AppElements $window
    $pauseConversion = Assert-NamedControl $elements 'Pause conversion' 'ControlType.Button'
    $preview = Assert-NamedControl $elements 'Show live conversion preview' 'ControlType.Button'
    Assert-Condition ($null -ne (Find-AppElement $elements $input 'ControlType.Text')) 'Current conversion does not show the full input path'
    Assert-Condition ($pauseAfterCurrent.Current.BoundingRectangle.Width -ge 150) 'Pause-after-current button is too narrow for its label'
    foreach ($action in @($pauseConversion, $pauseAfterCurrent, $stopCurrent, $stopAll)) {
        Assert-Condition ([Math]::Abs($action.Current.BoundingRectangle.Top - $preview.Current.BoundingRectangle.Top) -le 2) 'Current-conversion action buttons are not vertically aligned'
        Assert-Condition ([Math]::Abs($action.Current.BoundingRectangle.Height - $preview.Current.BoundingRectangle.Height) -le 2) 'Current-conversion action buttons do not have consistent heights'
    }
    $newTaskWhileRunning = Assert-NamedControl $elements 'New task' 'ControlType.Button'
    Assert-Condition $newTaskWhileRunning.Current.IsEnabled 'New task is disabled during conversion'
    Assert-Condition ($null -ne (Find-AppElement $elements 'Current file progress' 'ControlType.ProgressBar')) 'Current conversion lacks its file-local progress indicator'
    Assert-Condition ($null -ne (Find-AppElement $elements 'Current file progress' 'ControlType.Text')) 'Current conversion lacks its visible file progress label'
    $taskProgress = Find-AppElementWithPrefix $elements 'Task progress: ' 'ControlType.ProgressBar'
    Assert-Condition ($null -ne $taskProgress) 'Queue task lacks its segmented task progress indicator'
    Assert-Condition ($null -ne (Find-AppElement $elements 'Overall progress' 'ControlType.Text')) 'Queue task lacks its visible overall progress label'
    Assert-Condition ($taskProgress.Current.Name.Contains('completed previously')) 'Task progress does not distinguish previously completed work'
    Assert-Condition ($taskProgress.Current.Name.Contains('completed this run')) 'Task progress does not distinguish work completed in this run'
    Assert-Condition ($taskProgress.Current.Name.Contains('unfinished')) 'Task progress does not expose unfinished work'
    $progressLegendMarker = [char]0x25A0
    Assert-Condition ($null -ne (Find-AppElement $elements "$progressLegendMarker  Previous 0" 'ControlType.Text')) 'Task progress lacks its previously completed legend'
    Assert-Condition ($null -ne (Find-AppElement $elements "$progressLegendMarker  This run 0" 'ControlType.Text')) 'Task progress lacks its completed-this-run legend'
    Assert-Condition ($null -ne (Find-AppElement $elements "$progressLegendMarker  Remaining 1" 'ControlType.Text')) 'Task progress lacks its remaining-work legend'
    Invoke-AppElement $pauseAfterCurrent
    $cancelScheduledPause = Wait-AppElement $window 'Cancel scheduled pause' 'ControlType.Button' 40
    Assert-Condition ($null -ne $cancelScheduledPause) 'Pause-after-this-file control does not show its armed state'
    Invoke-AppElement $cancelScheduledPause
    Assert-Condition ($null -ne (Wait-AppElement $window 'Pause after this file' 'ControlType.Button' 40)) 'Scheduled pause cannot be cancelled'
    $elements = Get-AppElements $window
    Invoke-AppElement (Assert-NamedControl $elements 'Stop all' 'ControlType.Button')
    Assert-Condition ($null -ne (Wait-AppElement $window 'Confirm stop all' 'ControlType.Button' 40)) 'Stop all does not request confirmation'
    $elements = Get-AppElements $window
    Invoke-AppElement (Assert-NamedControl $elements 'Cancel' 'ControlType.Button')
    Start-Sleep -Milliseconds 150
    $elements = Get-AppElements $window
    $pauseConversion = Assert-NamedControl $elements 'Pause conversion' 'ControlType.Button'
    Invoke-AppElement $pauseConversion
    $resumeConversion = Wait-AppElement $window 'Resume conversion' 'ControlType.Button' 40
    Assert-Condition ($null -ne $resumeConversion) 'Active conversion did not enter the paused state'
    Invoke-AppElement $resumeConversion
    Assert-Condition ($null -ne (Wait-AppElement $window 'Pause conversion' 'ControlType.Button' 40)) 'Paused conversion did not resume'
    $elements = Get-AppElements $window
    $details = Assert-NamedControl $elements 'Details' 'ControlType.Button'
    Invoke-AppElement $details
    Start-Sleep -Milliseconds 150
    $elements = Get-AppElements $window
    foreach ($metricPrefix in @(
        'Position:',
        'Duration:',
        'Frame:',
        'Convert FPS:',
        'Speed:',
        'Original FPS:',
        'Target FPS:',
        'Encoder:',
        'Quality:',
        'Preset:',
        'Audio kept:',
        'Subtitles kept:',
        'Spent:',
        'Est. total:',
        'Remaining:'
    )) {
        Assert-Condition ($null -ne (Find-AppElementWithPrefixAnyType $elements $metricPrefix)) "Current conversion lacks the $metricPrefix metric"
    }
    $childProcesses = @(Get-CimInstance Win32_Process -Filter "ParentProcessId = $($process.Id)")
    Assert-Condition ($childProcesses.Count -eq 0) 'Direct FFmpeg conversion unexpectedly launched a child process'

    Invoke-AppElement $preview
    $previewRegion = Wait-AppElement $window 'Live conversion preview' 'ControlType.Group' 40
    if ($null -eq $previewRegion) {
        $previewRegion = Wait-AppElement $window 'Live conversion preview' 'ControlType.Pane' 20
    }
    Assert-Condition ($null -ne $previewRegion) 'Live conversion preview did not appear when enabled'
    $elements = Get-AppElements $window
    $hidePreview = Assert-NamedControl $elements 'Hide live conversion preview' 'ControlType.Button'
    Invoke-AppElement $hidePreview
    Start-Sleep -Milliseconds 200
    $elements = Get-AppElements $window

    $waitingTask = Find-AppElementWithPrefix $elements $waitingUnicodeLabel 'ControlType.ListItem'
    Assert-Condition ($null -ne $waitingTask) 'Pending task disappeared while another task was converting'
    Assert-Condition ($waitingTask.Current.Name.Contains('draggable')) 'Pending task is not exposed as draggable'
    Invoke-AppElement $waitingTask
    $moveUp = $null
    for ($attempt = 0; $attempt -lt 20; $attempt++) {
        Start-Sleep -Milliseconds 100
        $elements = Get-AppElements $window
        $moveUp = Find-AppElement $elements 'Move up' 'ControlType.Button'
        if ($null -ne $moveUp -and $moveUp.Current.IsEnabled) {
            break
        }
    }
    Assert-Condition ($null -ne $moveUp) 'Pending task move control disappeared during conversion'
    Assert-Condition $moveUp.Current.IsEnabled 'Pending task cannot be reordered during conversion'
    Invoke-AppElement $moveUp
    Start-Sleep -Milliseconds 250
    $stored = Get-Content -LiteralPath $queuePath -Raw | ConvertFrom-Json
    Assert-Condition ($stored.tasks[0].task_data_id -eq 'slint-package-smoke-waiting-task') 'Pending task reorder was not persisted during conversion'
    $elements = Get-AppElements $window
    $removePending = Assert-NamedControl $elements 'Remove selected' 'ControlType.Button'
    Invoke-AppElement $removePending
    Start-Sleep -Milliseconds 250
    $stored = Get-Content -LiteralPath $queuePath -Raw | ConvertFrom-Json
    Assert-Condition ($stored.tasks.Count -eq 1) 'Pending task could not be removed during conversion'
    Assert-Condition ($stored.tasks[0].task_data_id -eq 'slint-package-smoke-task') 'Removing a pending task disturbed the active conversion'

    $lockedStream.Dispose()
    $lockedStream = $null
    $completed = $false
    for ($attempt = 0; $attempt -lt 240 -and -not $completed; $attempt++) {
        Start-Sleep -Milliseconds 250
        $stored = Get-Content -LiteralPath $queuePath -Raw | ConvertFrom-Json
        $completed = $stored.tasks[0].status -eq 'completed'
    }
    Assert-Condition $completed 'Packaged direct conversion did not complete'
    $process.Refresh()
    Assert-Condition (-not $process.HasExited) 'Slint app exited after conversion'

    $outputDirectory = Join-Path $runRoot 'media (x265)'
    $outputs = @(Get-ChildItem -LiteralPath $outputDirectory -File -Filter '*.mkv')
    Assert-Condition ($outputs.Count -eq 1) 'Packaged conversion did not publish exactly one MKV'
    $backup = Join-Path $outputDirectory "original\${primaryUnicodeLabel}-smoke.mp4"
    Assert-Condition (Test-Path -LiteralPath $backup -PathType Leaf) 'Packaged conversion did not preserve the original'
    Assert-Condition ((Get-FileSha256 $backup) -eq $inputHash) 'Original backup changed during conversion'
    Assert-Condition (@(Get-ChildItem -LiteralPath $outputDirectory -Recurse -File | Where-Object { $_.Name -like '*.videoferry-*' }).Count -eq 0) 'Packaged conversion left staging artifacts'
    $systemStagingDirectory = Join-Path $isolatedTemp 'VideoFerry'
    if (Test-Path -LiteralPath $systemStagingDirectory -PathType Container) {
        Assert-Condition (@(Get-ChildItem -LiteralPath $systemStagingDirectory -File | Where-Object { $_.Name -like '*.videoferry-*' }).Count -eq 0) 'Packaged conversion left artifacts in the system temporary directory'
    }

    $historyTab = Wait-AppElement $window 'Completed history, 1 items' 'ControlType.Button' 80
    Assert-Condition ($null -ne $historyTab) 'Completed-history count did not update'
    Invoke-AppElement $historyTab
    Start-Sleep -Milliseconds 200
    $elements = Get-AppElements $window
    Assert-Condition ($null -ne (Find-AppElementWithPrefix $elements "${primaryUnicodeLabel}-smoke" 'ControlType.ListItem')) 'Unicode history item is not exposed'
    $historyConfiguration = Find-AppElementWithPrefix $elements 'Original FPS ' 'ControlType.Text'
    Assert-Condition ($null -ne $historyConfiguration) 'Completed task does not expose its original FPS'
    Assert-Condition ($historyConfiguration.Current.Name.Contains('Target FPS')) 'Completed task does not expose its target FPS'
    Assert-Condition ($historyConfiguration.Current.Name.Contains('Encoder x265')) 'Completed task does not expose its encoder'
    Assert-Condition ($historyConfiguration.Current.Name.Contains('Quality CRF 32')) 'Completed task does not expose its quality'
    Assert-Condition ($historyConfiguration.Current.Name.Contains('Preset ultrafast')) 'Completed task does not expose its preset'
    [void](Assert-NamedControl $elements 'Play' 'ControlType.Button')
    [void](Assert-NamedControl $elements 'Show folder' 'ControlType.Button')
    [void](Assert-NamedControl $elements 'Copy path' 'ControlType.Button')
    [void](Assert-NamedControl $elements 'Search finished conversions' 'ControlType.Edit')
    $clearHistory = Assert-NamedControl $elements 'Clear history' 'ControlType.Button'
    Invoke-AppElement $clearHistory
    Start-Sleep -Milliseconds 200
    $elements = Get-AppElements $window
    Invoke-AppElement (Assert-NamedControl $elements 'Confirm clear history' 'ControlType.Button')
    Start-Sleep -Milliseconds 200
    $historyPath = Join-Path $stateDirectory 'completed_history.json'
    $historyJson = Get-Content -LiteralPath $historyPath -Raw
    Assert-Condition ($historyJson -match '^\s*\[\s*\]\s*$') 'Clear history did not persist an empty history list'
    $pythonStateDirectory = Join-Path $isolatedLocalAppData 'HomeLab Video Converter'
    Assert-Condition (-not (Test-Path -LiteralPath (Join-Path $pythonStateDirectory 'video_converter_queue.json'))) 'Rust application wrote the Python queue state file'
    Assert-Condition (-not (Test-Path -LiteralPath (Join-Path $pythonStateDirectory 'video_converter_completed_history.json'))) 'Rust application wrote the Python history state file'

    Write-Output 'Slint portable smoke passed (safe two-step task builder, single queue start, draggable persisted queue order, focus, 6 workflows, Unicode, responsive queue management, pause/resume lifecycle, armed pause state, destructive-action confirmation, segmented task progress, file-local conversion progress, expandable live metrics, optional live preview, direct runtime, publication, backup, and searchable history actions).'
} finally {
    if ($null -ne $lockedStream) {
        $lockedStream.Dispose()
    }
    Stop-SmokeProcess $process
    foreach ($name in $savedEnvironment.Keys) {
        $value = $savedEnvironment[$name]
        if ($null -eq $value) {
            [Environment]::SetEnvironmentVariable($name, $null, 'Process')
        } else {
            [Environment]::SetEnvironmentVariable($name, $value, 'Process')
        }
    }
    if (Test-Path -LiteralPath $runRoot) {
        Assert-SmokeChild $runRoot
        Remove-Item -LiteralPath $runRoot -Recurse -Force
    }
}
