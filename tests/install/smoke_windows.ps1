$ErrorActionPreference = 'Stop'
$repo = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent
. (Join-Path $repo 'install.ps1')
. (Join-Path $PSScriptRoot 'transport.ps1')
function Get-ZevriaUserPath { throw 'Smoke must not read persistent PATH' }
function Set-ZevriaUserPath { throw 'Smoke must not write persistent PATH' }
function Get-ZevriaMachinePath { return '' }
Install-Zevria -RequestedVersion $env:FIXTURE_VERSION -SkipPath
