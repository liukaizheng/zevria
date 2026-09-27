# Standalone Windows installer: Windows PowerShell 5.1 and PowerShell 7.
# Dot-sourcing defines helpers only; fixtures override transport/registry helpers.
[CmdletBinding(PositionalBinding = $false)]
param(
    [string] $Version = 'latest',
    [switch] $NoPathUpdate,
    [switch] $Help
)

function Get-ZevriaVersion([string] $Value) {
    $value = $Value -creplace '^v', ''
    if ($value -cnotmatch '\A(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)(?:-(?<pre>[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?\z') {
        throw 'Expected a SemVer version, optionally prefixed with v, or latest.'
    }
    $pre = $Matches['pre']
    if ($pre) {
        foreach ($part in $pre.Split('.')) {
            if ($part -match '^0[0-9]+$') { throw 'Numeric prerelease identifiers cannot have leading zeroes.' }
        }
    }
    return $value
}
function Test-ZevriaAbsolute([string] $Path) {
    return ($Path -match '^[A-Za-z]:[\\/]' -or $Path -match '^\\\\[^\\/]+[\\/][^\\/]+([\\/]|$)') -and $Path -notmatch '^\\\\[?.]'
}
function Resolve-ZevriaPath([string] $Path) {
    if (-not (Test-ZevriaAbsolute $Path) -or $Path -match '[\p{Cc};"<>|?*]' -or $Path.Substring(2).Contains(':')) {
        throw "Unsafe path '$Path': use an absolute Windows path without controls, PATH delimiter (;), device paths or invalid filename characters."
    }
    foreach ($part in ($Path -split '[\\/]')) {
        if ($part -match '[ .]$' -and $part -notin @('.', '..')) { throw 'Path components must not end in spaces or dots.' }
        if ($part -match '^(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(\.|$)') { throw 'Reserved Windows device names are not installation paths.' }
    }
    $full = [IO.Path]::GetFullPath($Path)
    if ($full -eq [IO.Path]::GetPathRoot($full)) { return $full }
    return $full.TrimEnd('\', '/')
}
function Get-ZevriaHome {
    foreach ($candidate in @($env:HOME, $env:USERPROFILE, ($env:HOMEDRIVE + $env:HOMEPATH))) {
        if ($candidate -and (Test-ZevriaAbsolute $candidate)) { return Resolve-ZevriaPath $candidate }
    }
    throw 'Cannot locate user home: set an absolute HOME or USERPROFILE.'
}
function Get-ZevriaNativeMachine {
    # Environment architecture can describe an emulated process, not its host.
    if (-not ('ZevriaInstallerPlatform' -as [type])) {
        Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class ZevriaInstallerPlatform {
    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool IsWow64Process2(IntPtr process, out ushort guest, out ushort host);
}
'@
    }
    $guest = [UInt16] 0; $hostMachine = [UInt16] 0
    if (-not [ZevriaInstallerPlatform]::IsWow64Process2([IntPtr] (-1), [ref] $guest, [ref] $hostMachine)) {
        throw 'Cannot detect native Windows architecture. Use a supported Windows x64 host or a source build.'
    }
    return $hostMachine
}
function Get-ZevriaTarget {
    if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) { throw 'Use install.sh on supported Linux or Apple Silicon macOS hosts.' }
    if ((Get-ZevriaNativeMachine) -ne 0x8664) { throw 'Windows x64 is required; Windows ARM64/x86 are not packaged. Use a source build.' }
    return 'x86_64-pc-windows-msvc'
}
function New-ZevriaHttpRequest([Uri] $Uri, [bool] $Head) {
    $request = [Net.HttpWebRequest]::Create($Uri)
    $request.AllowAutoRedirect = $false
    $request.Timeout = 15000
    $request.ReadWriteTimeout = 30000
    $request.UserAgent = 'zevria-installer'
    if ($Head) { $request.Method = 'HEAD' }
    return $request
}
function Invoke-ZevriaRequest([string] $Url, [string] $Destination = '') {
    # Manual redirects also forbid HTTPS -> HTTP under .NET Framework / PS 5.1.
    $tls = [Net.ServicePointManager]::SecurityProtocol
    [Net.ServicePointManager]::SecurityProtocol = $tls -bor [Net.SecurityProtocolType]::Tls12
    try {
        for ($attempt = 0; $attempt -lt 3; $attempt++) {
            try {
                $uri = [Uri] $Url
                $clock = [Diagnostics.Stopwatch]::StartNew()
                for ($redirect = 0; $redirect -le 5; $redirect++) {
                    if ($uri.Scheme -cne 'https' -or $uri.UserInfo -or $clock.Elapsed.TotalSeconds -gt 180) { throw 'Only bounded HTTPS requests and redirects are allowed.' }
                    $request = New-ZevriaHttpRequest $uri (-not $Destination)
                    $response = $null
                    try {
                        $response = $request.GetResponse()
                        $status = [int] $response.StatusCode
                        if ($status -in @(301, 302, 303, 307, 308)) {
                            if (-not $response.Headers['Location']) { throw 'Redirect is missing Location.' }
                            $uri = [Uri]::new($uri, $response.Headers['Location'])
                            continue
                        }
                        if ($status -ne 200) { throw "Unusable HTTP response: $status" }
                        if (-not $Destination) { return $uri.AbsoluteUri }
                        if ($response.ContentLength -gt 536870912) { throw 'Download exceeds size limit.' }
                        $inputStream = $response.GetResponseStream()
                        $outputStream = [IO.File]::Open($Destination, [IO.FileMode]::Create, [IO.FileAccess]::Write, [IO.FileShare]::None)
                        try {
                            $buffer = New-Object byte[] 65536
                            $total = 0L
                            while (($read = $inputStream.Read($buffer, 0, $buffer.Length)) -gt 0) {
                                $total += $read
                                if ($total -gt 536870912 -or $clock.Elapsed.TotalSeconds -gt 180) { throw 'Download exceeded size/time limit.' }
                                $outputStream.Write($buffer, 0, $read)
                            }
                            if ($response.ContentLength -ge 0 -and $total -ne $response.ContentLength) { throw 'Interrupted download.' }
                        } finally { $outputStream.Dispose(); $inputStream.Dispose() }
                        return
                    } finally { if ($response) { $response.Dispose() } }
                }
                throw 'Too many HTTPS redirects.'
            } catch {
                if ($attempt -eq 2) { throw "Cannot download $Url (release/asset unavailable or network failure): $_" }
                Start-Sleep -Seconds ($attempt + 1)
            }
        }
    } finally { [Net.ServicePointManager]::SecurityProtocol = $tls }
}
function Get-ZevriaLatest { Invoke-ZevriaRequest 'https://github.com/liukaizheng/zevria/releases/latest' }
function Save-ZevriaDownload([string] $Url, [string] $Destination) { Invoke-ZevriaRequest $Url $Destination }
function Test-ZevriaChecksum([string] $Manifest, [string] $ArchiveName, [string] $Archive) {
    if ((Get-Item -LiteralPath $Manifest).Length -gt 65536) { throw 'SHA256SUMS is too large.' }
    $count = 0
    $expected = ''
    foreach ($line in [IO.File]::ReadAllLines($Manifest)) {
        if ($line -cnotmatch '^([0-9a-fA-F]{64}) [ *]([^ /\\]+)$') { throw 'Malformed SHA256SUMS entry.' }
        if ($Matches[2] -ceq $ArchiveName) { $expected = $Matches[1]; $count++ }
    }
    if ($count -ne 1) { throw 'SHA256SUMS must contain exactly one entry for the selected archive.' }
    if ((Get-FileHash -LiteralPath $Archive -Algorithm SHA256).Hash -ine $expected) { throw 'Archive checksum mismatch.' }
}
function Expand-ZevriaArchive([string] $Archive, [string] $Stage) {
    Add-Type -AssemblyName System.IO.Compression
    # ZipArchive on older .NET does not verify CRC and may silently truncate to a
    # forged central-directory length. Validate the workflow's single-file,
    # non-ZIP64 ZIP independently after copying, before executing any bytes.
    if (-not ('ZevriaInstallerZip' -as [type])) {
        Add-Type @'
using System;
using System.IO;
public static class ZevriaInstallerZip {
    public static void Verify(string archive, string extracted, long length) {
        uint expected;
        using (var file = File.OpenRead(archive))
        using (var reader = new BinaryReader(file)) {
            long end = -1;
            for (long p = file.Length - 22; p >= Math.Max(0, file.Length - 65557); --p) {
                file.Position = p;
                if (reader.ReadUInt32() != 0x06054b50) continue;
                file.Position = p + 20;
                if (p + 22 + reader.ReadUInt16() == file.Length) { end = p; break; }
            }
            if (end < 0) throw new InvalidDataException("Missing ZIP end record");
            file.Position = end + 4;
            if (reader.ReadUInt16() != 0 || reader.ReadUInt16() != 0 ||
                reader.ReadUInt16() != 1 || reader.ReadUInt16() != 1)
                throw new InvalidDataException("Expected a single-disk single-file ZIP");
            uint size = reader.ReadUInt32(), offset = reader.ReadUInt32();
            if ((long)offset + size != end || size < 46)
                throw new InvalidDataException("Invalid ZIP central directory");
            file.Position = offset;
            if (reader.ReadUInt32() != 0x02014b50) throw new InvalidDataException("Invalid ZIP member");
            file.Position = offset + 16;
            expected = reader.ReadUInt32();
            reader.ReadUInt32();
            if (reader.ReadUInt32() != length) throw new InvalidDataException("ZIP length mismatch");
            ushort nameLength = reader.ReadUInt16(), extraLength = reader.ReadUInt16(), commentLength = reader.ReadUInt16();
            // Workflow zipfile output needs no extra fields (including Unix link
            // metadata or ZIP64). Never interpret extra fields as filesystem links.
            if (extraLength != 0 || size != 46 + nameLength + commentLength)
                throw new InvalidDataException("Unexpected ZIP metadata");
            file.Position = offset + 42;
            if (reader.ReadUInt32() != 0) throw new InvalidDataException("Unexpected ZIP prefix");
            file.Position = 0;
            if (reader.ReadUInt32() != 0x04034b50) throw new InvalidDataException("Invalid ZIP local header");
            file.Position = 26;
            if (reader.ReadUInt16() != nameLength || reader.ReadUInt16() != 0)
                throw new InvalidDataException("Unexpected ZIP local metadata");
        }
        var table = new uint[256];
        for (uint i = 0; i < 256; i++) {
            uint value = i;
            for (int bit = 0; bit < 8; bit++) value = (value >> 1) ^ ((value & 1) != 0 ? 0xedb88320U : 0U);
            table[i] = value;
        }
        uint crc = 0xffffffffU;
        long total = 0;
        using (var file = File.OpenRead(extracted)) {
            var buffer = new byte[65536];
            int count;
            while ((count = file.Read(buffer, 0, buffer.Length)) != 0) {
                total += count;
                for (int i = 0; i < count; i++) crc = (crc >> 8) ^ table[(crc ^ buffer[i]) & 255];
            }
        }
        if (total != length || (crc ^ 0xffffffffU) != expected)
            throw new InvalidDataException("Corrupt ZIP member (length/CRC mismatch)");
    }
}
'@
    }
    $stream = [IO.File]::OpenRead($Archive)
    $zip = $null
    try {
        $zip = [IO.Compression.ZipArchive]::new($stream, [IO.Compression.ZipArchiveMode]::Read)
        if ($zip.Entries.Count -ne 1) { throw 'Archive must contain exactly one root-level regular zevria.exe.' }
        $entry = $zip.Entries[0]
        $type = ($entry.ExternalAttributes -shr 16) -band 0xF000
        # Python zipfile on Windows can omit Unix type bits. Reject Unix links,
        # special files, DOS directories/reparse points; copy only regular bytes.
        if ($entry.FullName -cne 'zevria.exe' -or $type -notin @(0, 0x8000) -or ($entry.ExternalAttributes -band 0x410) -ne 0 -or $entry.Length -gt 536870912) {
            throw 'Unsafe ZIP member name, type or size.'
        }
        $source = $entry.Open()
        $destination = [IO.File]::Open((Join-Path $Stage 'zevria.exe'), [IO.FileMode]::CreateNew)
        try {
            $buffer = New-Object byte[] 65536
            $total = 0L
            while (($read = $source.Read($buffer, 0, $buffer.Length)) -gt 0) {
                $total += $read
                if ($total -gt 536870912 -or $total -gt $entry.Length) { throw 'Expanded ZIP member exceeds declared size or limit.' }
                $destination.Write($buffer, 0, $read)
            }
            if ($total -ne $entry.Length) { throw 'Truncated ZIP member.' }
        } finally { $destination.Dispose(); $source.Dispose() }
        [ZevriaInstallerZip]::Verify($Archive, (Join-Path $Stage 'zevria.exe'), $entry.Length)
    } finally { if ($zip) { $zip.Dispose() }; $stream.Dispose() }
}
function New-ZevriaPrivateDirectory([string] $Parent) {
    $path = Join-Path $Parent ('.zevria-install-' + [Guid]::NewGuid().ToString('N'))
    [void] [IO.Directory]::CreateDirectory($path)
    try {
        $acl = New-Object Security.AccessControl.DirectorySecurity
        $acl.SetAccessRuleProtection($true, $false)
        $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User
        $rule = [Security.AccessControl.FileSystemAccessRule]::new($sid, 'FullControl', 'ContainerInherit, ObjectInherit', 'None', 'Allow')
        $acl.AddAccessRule($rule)
        Set-Acl -LiteralPath $path -AclObject $acl
        return $path
    } catch { [IO.Directory]::Delete($path); throw }
}
function Assert-ZevriaDestination([string] $Path, [bool] $Directory) {
    try { $attributes = [IO.File]::GetAttributes($Path) }
    catch [IO.FileNotFoundException] { return }
    catch [IO.DirectoryNotFoundException] { return }
    $isDirectory = ($attributes -band [IO.FileAttributes]::Directory) -ne 0
    if (($attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or $isDirectory -ne $Directory) {
        throw "Refusing unexpected directory/file or reparse destination: $Path"
    }
}
function Assert-ZevriaParents([string] $Path) {
    $current = $Path
    while ($current) {
        Assert-ZevriaDestination $current $true
        $parent = [IO.Directory]::GetParent($current)
        if (-not $parent) { break }
        $current = $parent.FullName
    }
}
function Test-ZevriaExecutable([string] $Executable, [string] $ExpectedVersion) {
    $info = New-Object Diagnostics.ProcessStartInfo
    $info.FileName = $Executable
    $info.Arguments = '--version'
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $process = [Diagnostics.Process]::Start($info)
    try {
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit(15000)) { $process.Kill(); throw 'Staged --version timed out.' }
        $text = $stdout.GetAwaiter().GetResult()
        $errorText = $stderr.GetAwaiter().GetResult()
        if ($process.ExitCode -ne 0 -or $text -cnotin @("zevria $ExpectedVersion`r`n", "zevria $ExpectedVersion`n")) {
            throw "Staged executable version did not exactly match the selected release: $text $errorText"
        }
    } finally { $process.Dispose() }
}
function Remove-ZevriaOwnedDirectory([string] $Path) {
    try { [IO.Directory]::Delete($Path, $true) }
    catch { Write-Warning "Could not clean installer-owned staging $Path. Retained files can be inspected and removed after closing any locks: $_" }
}
function Install-ZevriaExecutable([string] $Source, [string] $Bin) {
    Assert-ZevriaParents $Bin
    [void] [IO.Directory]::CreateDirectory($Bin)
    $destination = Join-Path $Bin 'zevria.exe'
    Assert-ZevriaDestination $destination $false
    $stage = New-ZevriaPrivateDirectory $Bin
    $new = Join-Path $stage 'new.exe'
    $backup = Join-Path $stage 'previous.exe'
    $keep = $false
    try {
        [IO.File]::Copy($Source, $new)
        if ([IO.File]::Exists($destination)) {
            # Atomic same-volume replacement, with the old executable retained.
            [IO.File]::Replace($new, $destination, $backup)
        } else { [IO.File]::Move($new, $destination) }
    } catch {
        $failure = $_
        # ReplaceFile can report a partial failure after creating the backup.
        # Restore it if possible; otherwise keep every recoverable staged file.
        if ([IO.File]::Exists($backup)) {
            try {
                Assert-ZevriaDestination $destination $false
                if ([IO.File]::Exists($destination)) {
                    [IO.File]::Replace($backup, $destination, (Join-Path $stage 'rejected.exe'))
                } else { [IO.File]::Move($backup, $destination) }
            } catch { $keep = $true }
        }
        $recovery = if ($keep) { " Recovery failed; restore previous.exe from $stage. Recovery files were retained." } else { ' The previous installation was preserved/restored.' }
        throw "Cannot replace $destination. Close running Zevria processes and rerun; no process was terminated.$recovery $failure"
    } finally { if (-not $keep) { Remove-ZevriaOwnedDirectory $stage } }
}
function Get-ZevriaUserPath {
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment')
    try {
        if (-not $key -or $key.GetValueNames() -notcontains 'Path') {
            return @{ Value = ''; Kind = [Microsoft.Win32.RegistryValueKind]::ExpandString }
        }
        $kind = $key.GetValueKind('Path')
        if ($kind -notin @([Microsoft.Win32.RegistryValueKind]::String, [Microsoft.Win32.RegistryValueKind]::ExpandString)) { throw 'User PATH has an unsupported registry type.' }
        return @{ Value = $key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames); Kind = $kind }
    } finally { if ($key) { $key.Dispose() } }
}
function Set-ZevriaUserPath([string] $Value, $Kind) {
    $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
    try { $key.SetValue('Path', $Value, $Kind) } finally { $key.Dispose() }
    # Notify Explorer/new terminals without expanding or rewriting the value.
    try {
        if (-not ('ZevriaInstallerEnvironment' -as [type])) {
            Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class ZevriaInstallerEnvironment {
    [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    public static extern IntPtr SendMessageTimeout(IntPtr window, uint message, UIntPtr wparam,
        string lparam, uint flags, uint timeout, out UIntPtr result);
}
'@
        }
        $result = [UIntPtr]::Zero
        [void] [ZevriaInstallerEnvironment]::SendMessageTimeout([IntPtr] 0xffff, 0x1a, [UIntPtr]::Zero, 'Environment', 2, 3000, [ref] $result)
    } catch { Write-Warning 'User PATH saved, but environment notification failed. Sign out/in if new terminals still see the old PATH.' }
}
function Add-ZevriaPath([string] $Value, [string] $Bin) {
    if (-not $Value) { return $Bin }
    $rest = @($Value.Split(';') | Where-Object {
        $entry = [Environment]::ExpandEnvironmentVariables($_.Trim('"')).Replace('/', '\').TrimEnd('\')
        -not $entry.Equals($Bin.TrimEnd('\'), [StringComparison]::OrdinalIgnoreCase)
    })
    return (@($Bin) + $rest) -join ';'
}
function Update-ZevriaPath([string] $Bin) {
    $saved = Get-ZevriaUserPath
    if ($saved.Kind -eq [Microsoft.Win32.RegistryValueKind]::ExpandString -and $Bin.Contains('%')) {
        throw 'A literal % in this root cannot safely be added to REG_EXPAND_SZ PATH; use the absolute executable path or choose another root.'
    }
    Set-ZevriaUserPath (Add-ZevriaPath $saved.Value $Bin) $saved.Kind
    $env:PATH = Add-ZevriaPath $env:PATH $Bin
}
function Get-ZevriaMachinePath { return [Environment]::GetEnvironmentVariable('Path', 'Machine') }
function Write-ZevriaNextSteps([string] $Bin, [bool] $PathUpdated) {
    $exe = Join-Path $Bin 'zevria.exe'
    $quoted = "'" + $exe.Replace("'", "''") + "'"
    Write-Host "Run directly: & $quoted --help"
    if ($PathUpdated) {
        Write-Host 'User PATH and this PowerShell process PATH were updated. Restart terminals for persistent changes.'
        Write-Host 'If run in a child PowerShell process, the parent PATH is unchanged. Machine PATH may still take priority in new terminals.'
    }
    $command = Get-Command zevria -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($command -and $command.Source -ine $exe) { Write-Warning "Another command shadows this installation: $($command.Definition). Use the absolute path above; remove/reorder the competing entry yourself." }
    # User PATH cannot override machine PATH in newly started processes.
    $machine = Get-ZevriaMachinePath
    foreach ($entry in ($machine -split ';')) {
        if (-not $entry) { continue }
        $candidate = Join-Path ([Environment]::ExpandEnvironmentVariables($entry.Trim('"'))) 'zevria.exe'
        if ([IO.File]::Exists($candidate) -and $candidate -ine $exe) { Write-Warning "Machine PATH may shadow Zevria in new terminals: $candidate. Use the absolute path or repair that competing entry."; break }
    }
    if (-not (Get-Command rtk -ErrorAction SilentlyContinue)) { Write-Warning 'Install RTK separately from rtk-ai/rtk (not the unrelated crates.io package) for command tools.' }
    $gitBash = @($env:ZEVRIA_GIT_BASH, "$env:ProgramFiles\Git\bin\bash.exe", "${env:ProgramFiles(x86)}\Git\bin\bash.exe", "$env:LOCALAPPDATA\Programs\Git\bin\bash.exe")
    if (-not ($gitBash | Where-Object { $_ -and [IO.File]::Exists($_) })) { Write-Warning 'Native Windows command tools need Git for Windows Bash. Install it separately; WSL bash.exe is not Git Bash.' }
    Write-Host 'Provider/model configuration is separate first-run setup. No credentials or configuration were changed.'
}
function Install-Zevria([string] $RequestedVersion = 'latest', [switch] $SkipPath, [switch] $ShowHelp) {
    $ErrorActionPreference = 'Stop'
    if ($RequestedVersion -cne 'latest') { $RequestedVersion = Get-ZevriaVersion $RequestedVersion }
    if ($ShowHelp) {
        Write-Output @'
Usage: install.ps1 [-Version VERSION] [-NoPathUpdate] [-Help]
Install latest stable or a SemVer version, optionally prefixed with v.
ZEVRIA_INSTALL: absolute installation root; default: resolved home\.zevria.
Installs only Zevria, not RTK, Git Bash, WSL, Rust, or provider configuration.
'@
        return
    }
    $target = Get-ZevriaTarget
    $homePath = Get-ZevriaHome
    $root = if (Test-Path Env:ZEVRIA_INSTALL) { Resolve-ZevriaPath $env:ZEVRIA_INSTALL } else { Join-Path $homePath '.zevria' }
    $bin = Join-Path $root 'bin'
    Assert-ZevriaParents $bin
    Assert-ZevriaDestination (Join-Path $bin 'zevria.exe') $false
    if ($RequestedVersion -ceq 'latest') {
        $url = Get-ZevriaLatest
        if ($url -cnotmatch '\Ahttps://github\.com/liukaizheng/zevria/releases/tag/(v[^/?#]+)\z') { throw 'Unexpected latest-release redirect identity.' }
        $RequestedVersion = Get-ZevriaVersion ($Matches[1].Replace('%2B', '+').Replace('%2b', '+'))
        if (($RequestedVersion -split '\+')[0].Contains('-')) { throw 'Latest redirect did not select a stable release.' }
    }
    $archiveName = "zevria-v$RequestedVersion-$target.zip"
    $stage = New-ZevriaPrivateDirectory ([IO.Path]::GetTempPath())
    try {
        $archive = Join-Path $stage 'archive.zip'
        $manifest = Join-Path $stage 'SHA256SUMS'
        $base = "https://github.com/liukaizheng/zevria/releases/download/v$RequestedVersion"
        Save-ZevriaDownload "$base/$archiveName" $archive
        Save-ZevriaDownload "$base/SHA256SUMS" $manifest
        Test-ZevriaChecksum $manifest $archiveName $archive
        Expand-ZevriaArchive $archive $stage
        Test-ZevriaExecutable (Join-Path $stage 'zevria.exe') $RequestedVersion
        Install-ZevriaExecutable (Join-Path $stage 'zevria.exe') $bin
    } finally { Remove-ZevriaOwnedDirectory $stage }
    $updated = $false
    if (-not $SkipPath) {
        try { Update-ZevriaPath $bin; $updated = $true } catch { Write-Warning "Installed successfully, but PATH setup failed: $_. Add '$bin' to your user PATH manually, or use the absolute executable path." }
    }
    Write-Host "Installed Zevria $RequestedVersion at $(Join-Path $bin 'zevria.exe')"
    if ($SkipPath) { Write-Host 'PATH was not changed (-NoPathUpdate).' }
    try { Write-ZevriaNextSteps $bin $updated }
    catch { Write-Warning "Installed successfully; optional prerequisite/PATH diagnostics failed: $_. Run $(Join-Path $bin 'zevria.exe') directly." }
}

if ($MyInvocation.InvocationName -ne '.') {
    try { Install-Zevria -RequestedVersion $Version -SkipPath:$NoPathUpdate -ShowHelp:$Help }
    catch { Write-Error "zevria installer: $_"; exit 1 }
}
