<#
.SYNOPSIS
    Runs build commands pinned to specific CPU cores and BelowNormal priority.
.DESCRIPTION
    Restricts MSBuild / Cargo to a subset of CPU cores (e.g. 4 out of 8 threads)
    and BelowNormal priority so background builds never cause mouse/UI stutter or
    slow down user apps (Premiere Pro, browser, games, etc.).
.EXAMPLE
    .\tools\build-sandboxed.ps1 -Command "cmd /c _build_msix_x64.cmd"
    .\tools\build-sandboxed.ps1 -Command "cargo build --release --manifest-path tools/wta/Cargo.toml"
#>
param(
    [string]$Command = "cmd /c _build_msix_x64.cmd",
    [int]$CoresToUse = 4
)

$cs = Get-CimInstance Win32_ComputerSystem
$totalCores = $cs.NumberOfLogicalProcessors
$useCores = [Math]::Min($CoresToUse, [Math]::Max(1, [int]($totalCores / 2)))
$mask = (1 -shl $useCores) - 1

Write-Host ("[SANDBOX] Pinning build to $useCores of $totalCores threads (Affinity: 0x{0:X}, Priority: BelowNormal)" -f $mask) -ForegroundColor Cyan
Write-Host ("[SANDBOX] Running: $Command") -ForegroundColor DarkGray

$psi = New-Object System.Diagnostics.ProcessStartInfo
$parts = $Command -split ' ', 2
$psi.FileName = $parts[0]
if ($parts.Length -gt 1) {
    $psi.Arguments = $parts[1]
}
$psi.UseShellExecute = $false

$proc = [System.Diagnostics.Process]::Start($psi)
if ($proc) {
    try {
        $proc.ProcessorAffinity = [IntPtr]$mask
        $proc.PriorityClass = [System.Diagnostics.ProcessPriorityClass]::BelowNormal
    } catch {
        Write-Warning "Could not adjust process priority/affinity: $_"
    }
    $proc.WaitForExit()
    Write-Host ("[SANDBOX] Exit code: " + $proc.ExitCode) -ForegroundColor ($proc.ExitCode -eq 0 ? 'Green' : 'Red')
    exit $proc.ExitCode
}
