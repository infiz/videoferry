[CmdletBinding()]
param(
    [string]$Application,
    [string]$OutputDirectory = (Join-Path $PSScriptRoot '..\.local\ui-regression')
)

$ErrorActionPreference = 'Stop'
$workspaceRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if ([string]::IsNullOrWhiteSpace($Application)) {
    $Application = Join-Path $workspaceRoot 'target\debug\videoferry.exe'
}
$captureScript = Join-Path $PSScriptRoot 'capture-product-screenshots.ps1'

foreach ($size in @(
    @{ Name = 'minimum-900x620'; Width = 900; Height = 620; Scale = '1' },
    @{ Name = 'default-1180x760'; Width = 1180; Height = 760; Scale = '1' },
    @{ Name = 'large-1440x900'; Width = 1440; Height = 900; Scale = '1' },
    @{ Name = 'scaled-125-minimum'; Width = 1125; Height = 775; Scale = '1.25' },
    @{ Name = 'scaled-150-minimum'; Width = 1350; Height = 930; Scale = '1.5' }
)) {
    $destination = Join-Path $OutputDirectory $size.Name
    & $captureScript `
        -Application $Application `
        -OutputDirectory $destination `
        -WindowWidth $size.Width `
        -WindowHeight $size.Height `
        -ScaleFactor $size.Scale
}

Write-Host "UI regression screenshots: $OutputDirectory"
