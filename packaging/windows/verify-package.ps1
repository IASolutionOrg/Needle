[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Archive,
    [Parameter(Mandatory = $true)]
    [string]$ChecksumFile,
    [Parameter(Mandatory = $true)]
    [string]$WorkDirectory,
    [Parameter(Mandatory = $true)]
    [string]$Repository
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$archivePath = (Resolve-Path -LiteralPath $Archive).Path
$checksumPath = (Resolve-Path -LiteralPath $ChecksumFile).Path
$repositoryPath = (Resolve-Path -LiteralPath $Repository).Path
$expectedHash = (Get-Content -LiteralPath $checksumPath -Raw).Trim().ToLowerInvariant()
if ($expectedHash -notmatch "^[0-9a-f]{64}$") {
    throw "Needle archive checksum file is malformed"
}
$actualHash = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
if ($actualHash -ne $expectedHash) {
    throw "Needle archive checksum mismatch: expected $expectedHash, got $actualHash"
}

$work = [System.IO.Path]::GetFullPath($WorkDirectory)
if (Test-Path -LiteralPath $work) {
    if ((Get-ChildItem -LiteralPath $work -Force | Measure-Object).Count -ne 0) {
        throw "Needle package verification directory must be empty: $work"
    }
} else {
    [System.IO.Directory]::CreateDirectory($work) | Out-Null
}

$extracted = Join-Path $work "extracted"
$installed = Join-Path $work "installed"
$data = Join-Path $work "data"
Expand-Archive -LiteralPath $archivePath -DestinationPath $extracted

$requiredArchiveFiles = @(
    "needle.exe",
    "install.ps1",
    "uninstall.ps1",
    "CODEX_RUNTIME.md",
    "LICENSE.md",
    "README.md",
    "runtime\bin\codex.exe",
    "runtime\bin\codex-code-mode-host.exe",
    "runtime\codex-path\rg.exe",
    "runtime\codex-resources\codex-command-runner.exe",
    "runtime\codex-resources\codex-windows-sandbox-setup.exe",
    "runtime\codex-package.json"
)
foreach ($relativePath in $requiredArchiveFiles) {
    $path = Join-Path $extracted $relativePath
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Needle archive is incomplete: missing $relativePath"
    }
}

$packagedNeedle = Join-Path $extracted "needle.exe"
& $packagedNeedle --version | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw "Packaged Needle executable failed its version probe"
}

& (Join-Path $extracted "install.ps1") -InstallDirectory $installed -SkipPathUpdate
$installedNeedle = Join-Path $installed "needle.exe"
$installedRuntime = Join-Path $installed "runtime"
$installedUninstaller = Join-Path $installed "uninstall.ps1"
foreach ($path in @($installedNeedle, $installedUninstaller)) {
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Needle isolated installation is incomplete: missing $path"
    }
}
foreach ($relativePath in $requiredArchiveFiles | Where-Object { $_.StartsWith("runtime\") }) {
    $path = Join-Path $installed $relativePath
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Needle isolated installation is incomplete: missing $relativePath"
    }
}

& $installedNeedle uninstall --help | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw "Installed Needle executable failed its uninstall help probe"
}
& $installedUninstaller `
    -ParentProcessId $PID `
    -InstallDirectory $installed `
    -Executable $installedNeedle `
    -RuntimeDirectory $installedRuntime `
    -ValidateOnly
if ($LASTEXITCODE -ne 0) {
    throw "Installed Needle uninstaller failed validation"
}

$stdout = Join-Path $work "serve.stdout"
$stderr = Join-Path $work "serve.stderr"
$arguments = @(
    "serve",
    "--data-dir", ('"{0}"' -f $data),
    "--repository", ('"{0}"' -f $repositoryPath)
)
$server = Start-Process `
    -FilePath $installedNeedle `
    -ArgumentList $arguments `
    -RedirectStandardOutput $stdout `
    -RedirectStandardError $stderr `
    -WindowStyle Hidden `
    -PassThru
try {
    $deadline = [DateTime]::UtcNow.AddSeconds(15)
    $launchUrl = $null
    while ([DateTime]::UtcNow -lt $deadline) {
        if (Test-Path -LiteralPath $stdout) {
            $launchUrl = Get-Content -LiteralPath $stdout | ForEach-Object {
                if ($_ -match "^Needle control plane: (http://.+)$") { $Matches[1] }
            } | Select-Object -First 1
        }
        if ($launchUrl) {
            break
        }
        if ($server.HasExited) {
            $serverError = if (Test-Path -LiteralPath $stderr) {
                Get-Content -LiteralPath $stderr -Raw
            } else {
                ""
            }
            throw "Installed Needle server exited before startup: $serverError"
        }
        Start-Sleep -Milliseconds 100
    }
    if (-not $launchUrl) {
        throw "Installed Needle server did not report a launch URL"
    }
    $response = Invoke-WebRequest -Uri $launchUrl -UseBasicParsing
    if ($response.StatusCode -ne 200 -or
        $response.Content -notmatch '<div id="root"></div>' -or
        $response.Content -match '__NEEDLE_CSRF__') {
        throw "Installed Needle executable did not serve its embedded web control plane"
    }
} finally {
    if (-not $server.HasExited) {
        Stop-Process -Id $server.Id -Force
        $server.WaitForExit()
    }
}

if ((Test-Path -LiteralPath $stderr) -and
    -not [string]::IsNullOrWhiteSpace((Get-Content -LiteralPath $stderr -Raw))) {
    throw "Installed Needle server wrote unexpected diagnostics: $(Get-Content -LiteralPath $stderr -Raw)"
}

Write-Host "Needle Windows package verification passed."
