[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [int]$ParentProcessId,
    [Parameter(Mandatory = $true)]
    [string]$InstallDirectory,
    [Parameter(Mandatory = $true)]
    [string]$Executable,
    [Parameter(Mandatory = $true)]
    [string]$RuntimeDirectory,
    [switch]$ValidateOnly
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$separators = [char[]]@(
    [System.IO.Path]::DirectorySeparatorChar,
    [System.IO.Path]::AltDirectorySeparatorChar
)
$install = [System.IO.Path]::GetFullPath($InstallDirectory).TrimEnd($separators)
$executablePath = [System.IO.Path]::GetFullPath($Executable)
$runtimePath = [System.IO.Path]::GetFullPath($RuntimeDirectory).TrimEnd($separators)
$scriptPath = [System.IO.Path]::GetFullPath($PSCommandPath)

$comparer = [System.StringComparer]::OrdinalIgnoreCase
if (-not $comparer.Equals([System.IO.Path]::GetDirectoryName($executablePath), $install)) {
    throw "Needle executable is outside the managed installation directory"
}
if (-not $comparer.Equals([System.IO.Path]::GetFileName($executablePath), "needle.exe")) {
    throw "Needle executable has an unexpected file name"
}
if (-not $comparer.Equals([System.IO.Path]::GetDirectoryName($runtimePath), $install) -or
    -not $comparer.Equals([System.IO.Path]::GetFileName($runtimePath), "runtime")) {
    throw "Needle runtime is outside the managed installation directory"
}
if (-not $comparer.Equals([System.IO.Path]::GetDirectoryName($scriptPath), $install)) {
    throw "Needle uninstaller is outside the managed installation directory"
}

$required = @(
    $executablePath,
    (Join-Path $runtimePath "bin\codex.exe"),
    (Join-Path $runtimePath "bin\codex-code-mode-host.exe"),
    (Join-Path $runtimePath "codex-path\rg.exe"),
    (Join-Path $runtimePath "codex-resources\codex-command-runner.exe"),
    (Join-Path $runtimePath "codex-resources\codex-windows-sandbox-setup.exe"),
    (Join-Path $runtimePath "codex-package.json")
)
foreach ($path in $required) {
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Needle managed installation is incomplete: missing $path"
    }
}

if ($ValidateOnly) {
    exit 0
}

Wait-Process -Id $ParentProcessId -ErrorAction SilentlyContinue

Remove-Item -LiteralPath $executablePath -Force
Remove-Item -LiteralPath $runtimePath -Recurse -Force
Remove-Item -LiteralPath $scriptPath -Force

if ((Get-ChildItem -LiteralPath $install -Force | Measure-Object).Count -eq 0) {
    Remove-Item -LiteralPath $install -Force
}
