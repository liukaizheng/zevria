# Platform-independent PowerShell parsing, SemVer, ZIP, and HTTP-policy fixtures.
# The Windows-only suite separately exercises real locks, paths, ACLs and editions.
$ErrorActionPreference = 'Stop'
$repo = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent
. (Join-Path $repo 'install.ps1')
function Assert($Condition, [string] $Message) { if (-not $Condition) { throw "Assertion failed: $Message" } }
function Expect-Failure([scriptblock] $Action) {
    $failed = $false
    try { & $Action } catch { $failed = $true }
    Assert $failed 'expected failure'
}
foreach ($file in @('install.ps1', 'tests/install/test_windows.ps1', 'tests/install/transport.ps1', 'tests/install/smoke_windows.ps1')) {
    $tokens = $null; $errors = $null
    [void] [Management.Automation.Language.Parser]::ParseFile((Join-Path $repo $file), [ref] $tokens, [ref] $errors)
    Assert (-not $errors) "parser errors in $file : $errors"
}
foreach ($version in @('1.2.3', 'v1.2.3', '1.2.3-alpha.01a', '1.2.3-alpha.1+build.00')) {
    Assert ((Get-ZevriaVersion $version) -ceq ($version -creplace '^v', '')) 'version normalization'
}
foreach ($version in @('', 'v01.2.3', '1.2.3-alpha.01', '1.2.3-alpha.1.00', '1.2.3-01', "1.2.3`n", '1.2.3/extra', '1.2.3+')) {
    Expect-Failure { Get-ZevriaVersion $version }
}
Install-Zevria -ShowHelp
Assert ((Add-ZevriaPath '' 'C:\fixture\bin') -ceq 'C:\fixture\bin') 'absent PATH does not introduce a current-directory entry'
Assert ((Add-ZevriaPath 'C:\other;;c:\fixture\bin\;C:\last' 'C:\fixture\bin') -ceq 'C:\fixture\bin;C:\other;;C:\last') 'PATH deduplication preserves unrelated entries'
$request = New-ZevriaHttpRequest ([Uri] 'https://github.com') $true
Assert (-not $request.AllowAutoRedirect -and $request.Timeout -eq 15000 -and $request.ReadWriteTimeout -eq 30000 -and $request.Method -eq 'HEAD') 'bounded manual redirect request'
$request.Abort()
# Replace only the low-level transport, so the real retry/redirect/length logic runs.
function Start-Sleep { }
function New-ZevriaHttpRequest([Uri] $Uri, [bool] $Head) {
    $script:requests++
    Assert ($Uri.Scheme -ceq 'https') 'HTTP must fail before opening a request'
    $response = [pscustomobject] @{ StatusCode = 200; Headers = @{}; ContentLength = 3L; Bytes = [byte[]] @(65, 66, 67) }
    if ($script:scenario -eq 'redirect' -and $Uri.Host -eq 'github.com') {
        $response.StatusCode = 302; $response.Headers['Location'] = 'https://release-assets.githubusercontent.com/fixture'
    }
    if ($script:scenario -eq 'downgrade') { $response.StatusCode = 302; $response.Headers['Location'] = 'http://insecure.invalid/fixture' }
    if ($script:scenario -eq 'loop') { $response.StatusCode = 302; $response.Headers['Location'] = 'https://github.com/again' }
    if ($script:scenario -eq 'interrupted') { $response.ContentLength = 4L }
    $response | Add-Member ScriptMethod GetResponseStream { return [IO.MemoryStream]::new($this.Bytes, $false) }
    $response | Add-Member ScriptMethod Dispose { }
    $request = [pscustomobject] @{ Response = $response }
    $request | Add-Member ScriptMethod GetResponse { return $this.Response }
    return $request
}
$temp = Join-Path ([IO.Path]::GetTempPath()) ('zevria-helper-tests-' + [Guid]::NewGuid().ToString('N'))
[void] [IO.Directory]::CreateDirectory($temp)
try {
    $destination = Join-Path $temp 'download'
    $script:requests = 0; $script:scenario = 'redirect'
    Invoke-ZevriaRequest 'https://github.com/fixture' $destination
    Assert ([IO.File]::ReadAllText($destination) -ceq 'ABC') 'HTTPS redirected content'
    Assert ($script:requests -eq 2) 'redirect followed once'
    foreach ($scenario in @('downgrade', 'loop', 'interrupted')) {
        $script:requests = 0; $script:scenario = $scenario
        Expect-Failure { Invoke-ZevriaRequest 'https://github.com/fixture' $destination }
        Assert ($script:requests -le 18) 'retries/redirects are bounded'
    }
    $script:requests = 0
    Expect-Failure { Invoke-ZevriaRequest 'http://insecure.invalid/fixture' $destination }
    Assert ($script:requests -eq 0) 'HTTP rejected before transport'

    Add-Type -AssemblyName System.IO.Compression
    $archive = Join-Path $temp 'fixture.zip'
    $file = [IO.File]::Open($archive, [IO.FileMode]::Create)
    $zip = [IO.Compression.ZipArchive]::new($file, [IO.Compression.ZipArchiveMode]::Create)
    try {
        $entry = $zip.CreateEntry('zevria.exe')
        $stream = $entry.Open()
        try { $stream.Write([byte[]] @(65, 66, 67), 0, 3) } finally { $stream.Dispose() }
    } finally { $zip.Dispose(); $file.Dispose() }
    $manifest = Join-Path $temp 'SHA256SUMS'
    $name = 'zevria-v1.2.3-x86_64-pc-windows-msvc.zip'
    $line = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash + "  $name`n"
    [IO.File]::WriteAllText($manifest, $line)
    Test-ZevriaChecksum $manifest $name $archive
    Expand-ZevriaArchive $archive $temp
    Assert ([IO.File]::ReadAllText((Join-Path $temp 'zevria.exe')) -ceq 'ABC') 'regular ZIP extraction'
    foreach ($text in @('', ($line + $line), ('0' * 64 + "  $name"), 'malformed')) {
        [IO.File]::WriteAllText($manifest, $text)
        Expect-Failure { Test-ZevriaChecksum $manifest $name $archive }
    }
    # Forge a smaller central-directory size while keeping the compressed stream.
    $bytes = [IO.File]::ReadAllBytes($archive)
    for ($i = 0; $i -lt $bytes.Length - 28; $i++) {
        if ($bytes[$i] -eq 0x50 -and $bytes[$i+1] -eq 0x4b -and $bytes[$i+2] -eq 1 -and $bytes[$i+3] -eq 2) {
            [Array]::Copy([BitConverter]::GetBytes([int] 1), 0, $bytes, $i + 24, 4)
            break
        }
    }
    [IO.File]::WriteAllBytes($archive, $bytes)
    [IO.File]::Delete((Join-Path $temp 'zevria.exe'))
    Expect-Failure { Expand-ZevriaArchive $archive $temp }
    Write-Host 'PowerShell helper fixtures passed.'
} finally { [IO.Directory]::Delete($temp, $true) }
