#Requires -RunAsAdministrator
[CmdletBinding()]
param(
    [ValidateRange(1, 65535)]
    [int]$ListenPort = 8631
)

$ErrorActionPreference = 'Stop'
$listener = Get-NetTCPConnection -State Listen -LocalPort $ListenPort |
    Select-Object -First 1
if ($null -eq $listener) {
    throw "Start Print Bridge on TCP port $ListenPort before running this script."
}
$program = (Get-Process -Id $listener.OwningProcess).Path
if ([string]::IsNullOrWhiteSpace($program) -or
    [IO.Path]::GetFileNameWithoutExtension($program) -notmatch '^print[- ]?bridge(?:-desktop|-windows-x64)?$') {
    throw "TCP port $ListenPort does not belong to a recognized Print Bridge executable."
}

$rule = @{
    Direction = 'Inbound'
    Action = 'Allow'
    Enabled = 'True'
    Program = $program
    Protocol = 'UDP'
    LocalPort = 5353
    Profile = 'Private'
    RemoteAddress = 'LocalSubnet'
}
$name = 'PrintBridge-AirPrint-mDNS'
$displayName = 'Print Bridge - AirPrint discovery (Private LAN)'
$existing = Get-NetFirewallRule -PolicyStore PersistentStore -Name $name -ErrorAction SilentlyContinue
if ($null -eq $existing) {
    New-NetFirewallRule -PolicyStore PersistentStore -Name $name -DisplayName $displayName @rule | Out-Null
} else {
    Set-NetFirewallRule -PolicyStore PersistentStore -Name $name -NewDisplayName $displayName @rule
}

Write-Output "AirPrint UDP 5353 inbound rule enabled for: $program"
Write-Output 'Scope: Private network, LocalSubnet. The service does not need to restart.'
$profiles = @(Get-NetConnectionProfile)
if (-not ($profiles | Where-Object NetworkCategory -eq 'Private')) {
    Write-Warning 'No Private network is active. This rule does not apply to Public or Domain networks.'
}
