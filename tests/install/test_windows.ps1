# Native offline suite. Run separately in Windows PowerShell 5.1 and PowerShell 7.
$ErrorActionPreference = 'Stop'
$repo = Split-Path (Split-Path $PSScriptRoot -Parent) -Parent
. (Join-Path $repo 'install.ps1')
. (Join-Path $PSScriptRoot 'transport.ps1')
Add-Type -AssemblyName System.IO.Compression

function Assert($Condition, [string] $Message) { if (-not $Condition) { throw "Assertion failed: $Message" } }
function Expect-Failure([scriptblock] $Action, [string] $Pattern = '.') {
    try { & $Action } catch {
        Assert ("$_" -match $Pattern) "expected /$Pattern/, got $_"
        return
    }
    throw 'Expected failure, but operation succeeded.'
}
function Write-FixtureManifest([string] $Text = '') {
    if (-not $Text) { $Text = (Get-FileHash -LiteralPath $env:FIXTURE_ARCHIVE -Algorithm SHA256).Hash + '  ' + $env:FIXTURE_ASSET + "`n" }
    [IO.File]::WriteAllText($env:FIXTURE_MANIFEST, $Text)
}
function New-FixtureArchive([string] $FixtureVersion = '1.2.3', [string[]] $Names = @('zevria.exe'), [int] $Attributes = 0) {
    $env:FIXTURE_VERSION = $FixtureVersion
    $env:FIXTURE_BINARY_VERSION = $FixtureVersion
    $env:FIXTURE_ASSET = "zevria-v$FixtureVersion-x86_64-pc-windows-msvc.zip"
    $file = [IO.File]::Open($env:FIXTURE_ARCHIVE, [IO.FileMode]::Create)
    $zip = [IO.Compression.ZipArchive]::new($file, [IO.Compression.ZipArchiveMode]::Create)
    try {
        foreach ($name in $Names) {
            $entry = $zip.CreateEntry($name)
            $entry.ExternalAttributes = $Attributes
            $output = $entry.Open()
            $inputStream = [IO.File]::OpenRead($script:fixtureExe)
            try { $inputStream.CopyTo($output) } finally { $inputStream.Dispose(); $output.Dispose() }
        }
    } finally { $zip.Dispose(); $file.Dispose() }
    Write-FixtureManifest
}
# Deliberate interception: no test path may touch the real persistent user PATH.
function Get-ZevriaMachinePath { return $script:machinePath }
function Get-ZevriaUserPath { return @{ Value = $script:savedPath; Kind = $script:savedKind } }
function Set-ZevriaUserPath([string] $Value, $Kind) { $script:savedPath = $Value; $script:savedKind = $Kind; $script:writes++ }

$temp = Join-Path ([IO.Path]::GetTempPath()) ('zevria-fixtures-' + [Guid]::NewGuid().ToString('N'))
[void] [IO.Directory]::CreateDirectory($temp)
$oldEnvironment = @{}
foreach ($key in @('HOME', 'USERPROFILE', 'HOMEDRIVE', 'HOMEPATH', 'ZEVRIA_INSTALL', 'PATH', 'PROCESSOR_ARCHITECTURE', 'PROCESSOR_ARCHITEW6432', 'FIXTURE_LATEST')) {
    $oldEnvironment[$key] = [Environment]::GetEnvironmentVariable($key)
}
try {
    $env:HOME = Join-Path $temp 'home'
    [void] [IO.Directory]::CreateDirectory($env:HOME)
    # ASCII source file remains readable by PS 5.1; generate Unicode at runtime.
    $env:ZEVRIA_INSTALL = Join-Path $temp ("root $([char]0x96ea) ' & [literal] `$()")
    $env:FIXTURE_ARCHIVE = Join-Path $temp 'fixture.zip'
    $env:FIXTURE_MANIFEST = Join-Path $temp 'SHA256SUMS'
    $env:FIXTURE_CALLS = Join-Path $temp 'calls'
    $env:FIXTURE_LATEST = $null
    $script:fixtureExe = Join-Path $temp 'fixture.exe'
    $source = Join-Path $temp 'fixture.cs'
    [IO.File]::WriteAllText($source, @'
using System;
class Fixture {
    static int Main(string[] args) {
        if (args.Length == 1 && args[0] == "--version") {
            Console.WriteLine("zevria " + Environment.GetEnvironmentVariable("FIXTURE_BINARY_VERSION")); return 0;
        }
        return 7;
    }
}
'@)
    $csc = Join-Path $env:WINDIR 'Microsoft.NET\Framework64\v4.0.30319\csc.exe'
    & $csc /nologo /target:exe "/out:$script:fixtureExe" $source
    Assert ($LASTEXITCODE -eq 0) 'compile native fixture with .NET Framework compiler'
    $script:savedPath = '%USERPROFILE%\tools;C:\unrelated;;%SystemRoot%\other'
    $script:savedKind = [Microsoft.Win32.RegistryValueKind]::ExpandString
    $script:writes = 0
    $script:machinePath = ''

    Write-Host 'SemVer, argument binding, help, home resolution, platform and path validation'
    foreach ($value in @('1.2.3', 'v1.2.3', '1.2.3-beta.1+build.00')) { Assert ((Get-ZevriaVersion $value) -ceq ($value -creplace '^v', '')) 'SemVer preserved' }
    foreach ($value in @('', 'v01.2.3', '1.2', '1.2.3-01', '1.2.3-alpha.01', "1.2.3`n", '1.2.3+x/y', '1.2.3?bad')) { Expect-Failure { Get-ZevriaVersion $value } }
    Install-Zevria -ShowHelp
    Assert ((Get-ChildItem -LiteralPath $env:HOME -Force).Count -eq 0) 'help has no home side effects'
    $shellExe = (Get-Process -Id $PID).Path
    foreach ($arguments in @(@('-Unknown'), @('-Version', '1.2.3', '-Version', '2.0.0'), @('-NoPathUpdate', '-NoPathUpdate'), @('-Version', '1.2'))) {
        $ErrorActionPreference = 'Continue'
        $output = & $shellExe -NoProfile -NonInteractive -File (Join-Path $repo 'install.ps1') @arguments 2>&1
        $status = $LASTEXITCODE
        $ErrorActionPreference = 'Stop'
        Assert ($status -ne 0) "invalid arguments rejected: $arguments / $output"
    }
    Assert (-not [IO.File]::Exists($env:FIXTURE_CALLS)) 'invalid arguments did not download'
    # These expected failures leave a nonzero native exit code that GitHub's
    # PowerShell 5.1 wrapper otherwise mistakes for a failed fixture suite.
    $global:LASTEXITCODE = 0
    $realHome = $env:HOME
    $env:USERPROFILE = Join-Path $temp 'profile'
    Assert ((Get-ZevriaHome) -eq $realHome) 'HOME wins'
    $env:HOME = 'relative'
    Assert ((Get-ZevriaHome) -eq $env:USERPROFILE) 'USERPROFILE fallback'
    $env:USERPROFILE = 'relative'
    $env:HOMEDRIVE = [IO.Path]::GetPathRoot($realHome).TrimEnd('\')
    $env:HOMEPATH = $realHome.Substring($env:HOMEDRIVE.Length)
    Assert ((Get-ZevriaHome) -eq $realHome) 'drive/path fallback'
    $env:HOME = $realHome
    Assert ((Get-ZevriaTarget) -eq 'x86_64-pc-windows-msvc') 'native x64 target'
    $nativeMachine = (Get-Item Function:Get-ZevriaNativeMachine).ScriptBlock
    function Get-ZevriaNativeMachine { return $script:machine }
    $script:machine = 0xaa64
    Expect-Failure { Get-ZevriaTarget } 'ARM64'
    $script:machine = 0x14c
    Expect-Failure { Get-ZevriaTarget } 'x64'
    Set-Item Function:Get-ZevriaNativeMachine $nativeMachine
    foreach ($path in @('relative', 'C:relative', 'C:\bad;entry', "C:\bad`nentry", '\\?\C:\device', 'C:\bad:stream')) { Expect-Failure { Resolve-ZevriaPath $path } }

    Write-Host 'Latest resolves once, explicit assets, clean install, opt-out, repair/downgrade'
    New-FixtureArchive
    $oldProcessPath = $env:PATH
    Install-Zevria -SkipPath
    $exe = Join-Path $env:ZEVRIA_INSTALL 'bin\zevria.exe'
    Assert ([IO.File]::Exists($exe)) 'installed executable'
    $calls = [IO.File]::ReadAllLines($env:FIXTURE_CALLS)
    Assert (($calls | Where-Object { $_ -eq 'latest' }).Count -eq 1) 'latest resolved once'
    Assert ($calls.Count -eq 3) 'two downloads after one resolution'
    Assert ($env:PATH -ceq $oldProcessPath -and $script:writes -eq 0) 'opt-out skips process and persistent PATH'
    [IO.File]::WriteAllText($exe, 'damaged installation')
    Install-Zevria -RequestedVersion '1.2.3' -SkipPath
    Assert ((Get-FileHash -LiteralPath $exe).Hash -eq (Get-FileHash -LiteralPath $script:fixtureExe).Hash) 'same-version repair replaced damaged bytes'
    $other = Join-Path $temp 'other-installation'
    [void] [IO.Directory]::CreateDirectory($other)
    [IO.File]::WriteAllText((Join-Path $other 'keep'), 'unrelated')
    foreach ($version in @('1.2.3', '2.0.0', '1.0.0', '1.0.0-beta.1+build.00')) {
        New-FixtureArchive $version
        Install-Zevria -RequestedVersion "v$version" -SkipPath
    }
    foreach ($latest in @('http://github.com/liukaizheng/zevria/releases/tag/v1.2.3', 'https://github.com/other/repo/releases/tag/v1.2.3', 'https://github.com/liukaizheng/zevria/releases/tag/v1.2.3-beta')) {
        $env:FIXTURE_LATEST = $latest
        Expect-Failure { Install-Zevria -SkipPath }
    }
    $env:FIXTURE_LATEST = $null

    Write-Host 'Checksums, archive inventory, corrupt downloads and staged versions'
    New-FixtureArchive
    $before = (Get-FileHash -LiteralPath $exe).Hash
    $goodManifest = [IO.File]::ReadAllText($env:FIXTURE_MANIFEST)
    foreach ($text in @('', ($goodManifest + $goodManifest), ('0' * 64 + '  ' + $env:FIXTURE_ASSET), 'bad digest', ('a' * 64 + '  other.zip'))) {
        [IO.File]::WriteAllText($env:FIXTURE_MANIFEST, $text)
        Expect-Failure { Install-Zevria -RequestedVersion '1.2.3' -SkipPath }
        Assert ((Get-FileHash -LiteralPath $exe).Hash -eq $before) 'verification preserved old binary'
    }
    foreach ($names in @(@('../zevria.exe'), @('/zevria.exe'), @('nested/zevria.exe'), @('zevria.exe', 'extra'), @('zevria.exe', 'zevria.exe'))) {
        New-FixtureArchive -Names $names
        Expect-Failure { Install-Zevria -RequestedVersion '1.2.3' -SkipPath } 'Archive|ZIP'
    }
    foreach ($attributes in @(0x10, 0x400, -1577058304, 0x40000000, 0x10000000)) {
        New-FixtureArchive -Attributes $attributes
        Expect-Failure { Install-Zevria -RequestedVersion '1.2.3' -SkipPath } 'ZIP'
    }
    [IO.File]::WriteAllText($env:FIXTURE_ARCHIVE, 'corrupt zip')
    Write-FixtureManifest
    Expect-Failure { Install-Zevria -RequestedVersion '1.2.3' -SkipPath }
    New-FixtureArchive
    $env:FIXTURE_BINARY_VERSION = '9.9.9'
    Expect-Failure { Install-Zevria -RequestedVersion '1.2.3' -SkipPath } 'version'
    $env:FIXTURE_BINARY_VERSION = '1.2.3'
    [IO.File]::Delete($env:FIXTURE_ARCHIVE)
    Expect-Failure { Install-Zevria -RequestedVersion '1.2.3' -SkipPath }
    Assert ((Get-FileHash -LiteralPath $exe).Hash -eq $before) 'missing asset preserved binary'
    New-FixtureArchive

    Write-Host 'Windows locks and unsafe destinations'
    $lock = [IO.File]::Open($exe, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try { Expect-Failure { Install-Zevria -RequestedVersion '1.2.3' -SkipPath } 'Close running Zevria' } finally { $lock.Dispose() }
    Assert ((Get-FileHash -LiteralPath $exe).Hash -eq $before) 'locked binary preserved'
    [IO.File]::Delete($exe)
    [void] [IO.Directory]::CreateDirectory($exe)
    Expect-Failure { Install-Zevria -RequestedVersion '1.2.3' -SkipPath } 'destination'
    [IO.Directory]::Delete($exe)
    Install-Zevria -RequestedVersion '1.2.3' -SkipPath
    $junction = Join-Path $temp 'junction'
    $junctionTarget = Join-Path $temp 'junction-target'
    [void] [IO.Directory]::CreateDirectory($junctionTarget)
    New-Item -ItemType Junction -Path $junction -Target $junctionTarget | Out-Null
    Expect-Failure { Assert-ZevriaParents (Join-Path $junction 'bin') } 'reparse'
    [IO.Directory]::Delete($junction)

    Write-Host 'Mocked persistent PATH preserves references/type, deduplicates, process priority'
    $bin = Join-Path $env:ZEVRIA_INSTALL 'bin'
    $script:savedPath = "$bin;%USERPROFILE%\tools;$($bin.ToUpperInvariant());;C:\unrelated"
    $env:PATH = "$oldProcessPath;$bin;$bin"
    Install-Zevria -RequestedVersion '1.2.3'
    Assert ($script:writes -eq 1) 'one mocked persistent write'
    Assert ($script:savedKind -eq [Microsoft.Win32.RegistryValueKind]::ExpandString) 'registry type preserved'
    Assert ($script:savedPath -ceq "$bin;%USERPROFILE%\tools;;C:\unrelated") 'unexpanded references and unrelated entries preserved'
    Assert ($env:PATH.StartsWith("$bin;")) 'process bin first'
    $script:savedKind = [Microsoft.Win32.RegistryValueKind]::String
    Update-ZevriaPath $bin
    Assert ($script:savedKind -eq [Microsoft.Win32.RegistryValueKind]::String) 'REG_SZ preserved'
    $script:machinePath = Join-Path $temp 'machine bin'
    [void] [IO.Directory]::CreateDirectory($script:machinePath)
    [IO.File]::Copy($script:fixtureExe, (Join-Path $script:machinePath 'zevria.exe'))
    $diagnostic = Write-ZevriaNextSteps $bin $true 3>&1 | Out-String
    Assert ($diagnostic -match 'Machine PATH may shadow') 'machine shadowing diagnosed without machine writes'
    Assert ([IO.File]::ReadAllText((Join-Path $other 'keep')) -ceq 'unrelated') 'other installation untouched'
    Assert ((Get-ChildItem -LiteralPath $env:HOME -Force).Count -eq 0) 'no provider configuration created'
    Write-Host 'Windows installer fixtures passed (no real registry writes).'
} finally {
    foreach ($key in $oldEnvironment.Keys) { [Environment]::SetEnvironmentVariable($key, $oldEnvironment[$key]) }
    [IO.Directory]::Delete($temp, $true)
}
