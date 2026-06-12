param(
    [string]$ServiceName = "NasFastCache",
    [string]$NssmPath = "nssm.exe",
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"
$commands = @(
    @("stop", $ServiceName),
    @("remove", $ServiceName, "confirm")
)

foreach ($command in $commands) {
    $line = @($NssmPath) + $command
    if ($DryRun) {
        $line -join " "
    } else {
        & $NssmPath @command
    }
}

if (-not $DryRun) {
    "removed service=$ServiceName"
}
