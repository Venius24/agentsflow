# Keeps the gateway running on the owner's PC. Started by the Windows task
# "agentsflow" at logon, so it does not depend on the Claude app or a terminal.
# If the gateway exits, it starts again after a pause.
#
# Rebuild:  Stop-ScheduledTask agentsflow; Stop-Process -Name agentsflow -Force
#           cargo build -p agentsflow-gateway; Start-ScheduledTask agentsflow
$log = 'C:/Users/Wonde/agentsflow-home/.agentsflow/autostart.log'
while ($true) {
    Add-Content $log "$(Get-Date -Format s) starting"
    & (Join-Path $PSScriptRoot 'local.ps1') *> $null
    Add-Content $log "$(Get-Date -Format s) gateway exited, restarting in 15 s"
    Start-Sleep -Seconds 15
}
