# The watcher on the owner's PC: once a minute it asks the gateway "are you alive",
# tells you in Telegram when it stops answering, and ends a hung gateway process so
# that autostart.ps1 starts a new one. Started by the Windows task "agentsflow-watch"
# at logon (see docs/departments.md, "Сторож шлюза").
#
# It runs from its own copy of the program (agentsflow-watch.exe), so rebuilding the
# gateway is not blocked by it and Stop-Process -Name agentsflow never touches it.
$repo = Split-Path $PSScriptRoot -Parent
$home_dir = 'C:/Users/Wonde/agentsflow-home'
$logs = Join-Path $home_dir '.agentsflow'
$log = Join-Path $logs 'watch.log'
Set-Location $repo
foreach ($line in Get-Content (Join-Path $repo '.env')) {
    $text = $line.Trim()
    if ($text -eq '' -or $text.StartsWith('#')) { continue }
    $text = $text -replace '^export\s+', ''
    $at = $text.IndexOf('=')
    if ($at -lt 1) { continue }
    $value = $text.Substring($at + 1).Trim().Trim('"').Trim("'")
    [Environment]::SetEnvironmentVariable($text.Substring(0, $at).Trim(), $value, 'Process')
}
while ($true) {
    $copy = Join-Path $logs 'agentsflow-watch.exe'
    try {
        Copy-Item (Join-Path $repo 'target\debug\agentsflow.exe') $copy -Force
    } catch {
        Add-Content $log "$(Get-Date -Format s) could not refresh the copy, using the old one: $_"
    }
    Add-Content $log "$(Get-Date -Format s) watch starting"
    & $copy watch --home $home_dir *>> $log
    Add-Content $log "$(Get-Date -Format s) watch exited, restarting in 15 s"
    Start-Sleep -Seconds 15
}
