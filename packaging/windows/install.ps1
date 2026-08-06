[CmdletBinding()]
param(
    [string]$SourceBinary = (Join-Path $PSScriptRoot "needle.exe"),
    [string]$SourceCodexRuntime = (Join-Path $PSScriptRoot "runtime"),
    [string]$InstallDirectory = (Join-Path $env:LOCALAPPDATA "Programs\Needle"),
    [switch]$SkipPathUpdate
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

if (-not (Test-Path -LiteralPath $SourceBinary -PathType Leaf)) {
    throw "Needle binary was not found at $SourceBinary"
}
if (-not (Test-Path -LiteralPath $SourceCodexRuntime -PathType Container)) {
    throw "Needle's managed Codex runtime was not found at $SourceCodexRuntime"
}

$requiredRuntimeFiles = @(
    "bin\codex.exe",
    "bin\codex-code-mode-host.exe",
    "codex-path\rg.exe",
    "codex-resources\codex-command-runner.exe",
    "codex-resources\codex-windows-sandbox-setup.exe",
    "codex-package.json"
)
foreach ($relativePath in $requiredRuntimeFiles) {
    $runtimePath = Join-Path $SourceCodexRuntime $relativePath
    if (-not (Test-Path -LiteralPath $runtimePath -PathType Leaf)) {
        throw "Needle's managed Codex runtime is incomplete: missing $relativePath"
    }
}

$source = (Resolve-Path -LiteralPath $SourceBinary).Path
$runtimeSource = (Resolve-Path -LiteralPath $SourceCodexRuntime).Path
[System.IO.Directory]::CreateDirectory($InstallDirectory) | Out-Null
$destination = Join-Path $InstallDirectory "needle.exe"
if (-not [System.StringComparer]::OrdinalIgnoreCase.Equals($source, $destination)) {
    Copy-Item -LiteralPath $source -Destination $destination -Force
}

$runtimeDirectory = Join-Path $InstallDirectory "runtime"
[System.IO.Directory]::CreateDirectory($runtimeDirectory) | Out-Null
if (-not [System.StringComparer]::OrdinalIgnoreCase.Equals($runtimeSource, $runtimeDirectory)) {
    Get-ChildItem -LiteralPath $runtimeSource | Copy-Item -Destination $runtimeDirectory -Recurse -Force
}

if (-not $SkipPathUpdate) {
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $entries = @($userPath -split ";" | Where-Object { $_ })
    $normalizedInstallDirectory = $InstallDirectory.TrimEnd([System.IO.Path]::DirectorySeparatorChar)
    if (-not ($entries | Where-Object {
        [System.StringComparer]::OrdinalIgnoreCase.Equals(
            $_.TrimEnd([System.IO.Path]::DirectorySeparatorChar),
            $normalizedInstallDirectory
        )
    })) {
        $updatedPath = (@($entries) + $InstallDirectory) -join ";"
        [Environment]::SetEnvironmentVariable("Path", $updatedPath, "User")
    }
}

Write-Host "Needle installed at $destination"
Write-Host "Open a new terminal in a Git repository, then run:"
Write-Host "  needle enable"
