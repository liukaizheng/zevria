# Offline fixture transport. Dot-source after install.ps1; never used by users.
function Get-ZevriaLatest {
    [IO.File]::AppendAllText($env:FIXTURE_CALLS, "latest`n")
    if ($env:FIXTURE_LATEST) { return $env:FIXTURE_LATEST }
    return "https://github.com/liukaizheng/zevria/releases/tag/v$env:FIXTURE_VERSION"
}
function Save-ZevriaDownload([string] $Url, [string] $Destination) {
    [IO.File]::AppendAllText($env:FIXTURE_CALLS, "$Url`n")
    $base = "https://github.com/liukaizheng/zevria/releases/download/v$env:FIXTURE_VERSION"
    if ($Url -ceq "$base/$env:FIXTURE_ASSET") { [IO.File]::Copy($env:FIXTURE_ARCHIVE, $Destination) }
    elseif ($Url -ceq "$base/SHA256SUMS") { [IO.File]::Copy($env:FIXTURE_MANIFEST, $Destination) }
    else { throw "Unexpected fixture URL: $Url" }
}
