# Tests for install.ps1 (docs/SPEC-M2.2.md section 6.7, SPEC-M2.3 section 6), the Windows side of
# scripts/test-install.sh.
#
#   pwsh -NoProfile -File scripts/test-install.ps1                         # install.ps1 under pwsh
#   $env:KIOKU_TEST_PS = 'powershell'; powershell -File scripts/test-install.ps1   # under 5.1
#   $env:KIOKU_TEST_REAL_BIN = 'target\debug\kioku.exe'                    # also the real binary
#
# Builds fixture releases (stub kioku.exe programs compiled with the .NET Framework's csc.exe
# that echo their argv, packaged exactly like release.yml does), serves them with a small
# python http.server that also answers GitHub's `releases/latest` redirect, and runs
# install.ps1 against them with KIOKU_DOWNLOAD_BASE, a temp install dir and a file standing in
# for the user PATH. With KIOKU_TEST_REAL_BIN it also installs the real kioku.exe and runs its
# Windows-specific paths: `setup` without --client-only, UTF-8 hook stdin, and `kioku update`
# replacing the running exe (SPEC-M2.2 section 4.4, section 4.7, section 5).

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$Root = Split-Path -Parent $PSScriptRoot
$InstallPs1 = Join-Path $Root 'install.ps1'
$Shell = $env:KIOKU_TEST_PS
if (-not $Shell) { $Shell = 'pwsh' }
$Target = 'x86_64-pc-windows-msvc'
$Tar = Join-Path $env:SystemRoot 'System32\tar.exe'
$Work = Join-Path ([System.IO.Path]::GetTempPath()) ('kioku-test-' + [System.Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $Work | Out-Null
$Server = $null
$Sleeper = $null

# The fixture server is on 127.0.0.1: keep any proxy out of the way.
foreach ($v in 'http_proxy', 'HTTP_PROXY', 'https_proxy', 'HTTPS_PROXY', 'all_proxy', 'ALL_PROXY') {
    Remove-Item -LiteralPath "Env:$v" -ErrorAction SilentlyContinue
}
$env:no_proxy = '127.0.0.1,localhost'

$script:Passed = 0
$script:Failed = 0
$script:Out = ''
$script:Rc = 0

function Pass([string]$Name) {
    $script:Passed++
    Write-Host "ok   $Name"
}

function Fail([string]$Name) {
    $script:Failed++
    Write-Host "FAIL $Name"
    foreach ($l in ($script:Out -split "`r?`n")) { Write-Host "     | $l" }
}

function Check([string]$Name, [bool]$Cond) {
    if ($Cond) { Pass $Name } else { Fail $Name }
}

function Has([string]$Needle) {
    return $script:Out.Contains($Needle)
}

# ---------------------------------------------------------------- fixtures

$Fix = Join-Path $Work 'fix'
$Rel = Join-Path $Fix 'releases\download'
New-Item -ItemType Directory -Path $Rel -Force | Out-Null
$Csc = Join-Path $env:WINDIR 'Microsoft.NET\Framework64\v4.0.30319\csc.exe'

# A stub kioku.exe: `--version` prints the version, `sleep` waits (a running exe to replace),
# anything else echoes its argv. `broken` exits 1 like a binary that does not run here.
function New-Stub([string]$Out, [string]$Version, [string]$Kind) {
    $src = Join-Path $Work ("stub-" + [System.Guid]::NewGuid().ToString('N') + '.cs')
    if ($Kind -eq 'broken') {
        $body = 'System.Console.Error.WriteLine("kioku: the procedure entry point could not be located"); return 1;'
    } else {
        $body = @"
if (a.Length > 0 && a[0] == "--version") { System.Console.WriteLine("kioku $Version ($Target)"); return 0; }
if (a.Length > 0 && a[0] == "sleep") { System.Threading.Thread.Sleep(120000); return 0; }
System.Console.Write("STUB-ARGV:");
foreach (var s in a) { System.Console.Write(" [" + s + "]"); }
System.Console.WriteLine();
return 0;
"@
    }
    Set-Content -LiteralPath $src -Value "class P { static int Main(string[] a) { $body } }"
    & $Csc /nologo /out:$Out $src | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "csc failed for $Out" }
}

function Get-Sha([string]$Path) {
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

# New-Asset <tag> <ok|broken|path-of-a-real-exe>: package like release.yml (+ .sha256).
function New-Asset([string]$Tag, [string]$Kind) {
    $name = "kioku-$Tag-$Target"
    $pkg = Join-Path $Work 'pkg'
    $dir = Join-Path $pkg $name
    New-Item -ItemType Directory -Path $dir -Force | Out-Null
    New-Item -ItemType Directory -Path (Join-Path $Rel $Tag) -Force | Out-Null
    $exe = Join-Path $dir 'kioku.exe'
    if (Test-Path -LiteralPath $Kind) {
        Copy-Item -LiteralPath $Kind -Destination $exe
    } else {
        New-Stub $exe $Tag.TrimStart('v') $Kind
    }
    Set-Content -LiteralPath (Join-Path $dir 'README.md') -Value 'readme'
    $tgz = Join-Path (Join-Path $Rel $Tag) "$name.tar.gz"
    & $Tar -czf $tgz -C $pkg $name
    if ($LASTEXITCODE -ne 0) { throw "tar failed for $name" }
    [System.IO.File]::WriteAllText("$tgz.sha256", "$(Get-Sha $tgz)  $name.tar.gz`n")
    Remove-Item -LiteralPath $dir -Recurse -Force
}

function New-Sums([string]$Tag) {
    $d = Join-Path $Rel $Tag
    $lines = Get-ChildItem -LiteralPath $d -Filter '*.sha256' | Sort-Object Name | ForEach-Object {
        (Get-Content -LiteralPath $_.FullName -Raw).Trim()
    }
    [System.IO.File]::WriteAllText((Join-Path $d 'SHA256SUMS'), (($lines -join "`n") + "`n"))
}

New-Asset 'v9.9.9' 'ok'; New-Sums 'v9.9.9'
New-Asset 'v9.9.8' 'ok'; New-Sums 'v9.9.8'
# v9.9.7: SHA256SUMS lies.
New-Asset 'v9.9.7' 'ok'
[System.IO.File]::WriteAllText((Join-Path $Rel 'v9.9.7\SHA256SUMS'), ('0' * 64) + "  kioku-v9.9.7-$Target.tar.gz`n")
# v9.9.6: predates SHA256SUMS (only <asset>.sha256).
New-Asset 'v9.9.6' 'ok'
# v9.9.5: no checksum at all.
New-Asset 'v9.9.5' 'ok'
Remove-Item -Path (Join-Path $Rel 'v9.9.5\*.sha256')
# v9.9.4: the binary does not run here.
New-Asset 'v9.9.4' 'broken'; New-Sums 'v9.9.4'
$RealBin = $env:KIOKU_TEST_REAL_BIN
if ($RealBin) {
    $RealBin = (Resolve-Path -LiteralPath $RealBin).Path
    New-Asset 'v9.9.3' $RealBin; New-Sums 'v9.9.3'
    New-Asset 'v9.9.2' $RealBin; New-Sums 'v9.9.2'
}

# ---------------------------------------------------------------- server

$ServerPy = Join-Path $Work 'server.py'
Set-Content -LiteralPath $ServerPy -Value @'
import functools, http.server, os, socketserver, sys

root, portfile = sys.argv[1], sys.argv[2]

class Handler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def route(self):
        path = self.path.split("?")[0]
        if path.startswith("/broken/"):
            self.send_response(500)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return True
        if path.endswith("/releases/latest"):
            prefix = path[: -len("/releases/latest")]
            loc = prefix + ("/releases" if prefix == "/empty" else "/releases/tag/v9.9.9")
            self.send_response(302)
            self.send_header("Location", loc)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return True
        if "/releases/tag/" in path or path == "/empty/releases":
            body = b"release page\n"
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            if self.command == "GET":
                self.wfile.write(body)
            return True
        return False

    def do_GET(self):
        if not self.route():
            super().do_GET()

    def do_HEAD(self):
        if not self.route():
            super().do_HEAD()

class Server(http.server.ThreadingHTTPServer):
    def server_bind(self):
        socketserver.TCPServer.server_bind(self)
        self.server_name = "127.0.0.1"
        self.server_port = self.server_address[1]

srv = Server(("127.0.0.1", 0), functools.partial(Handler, directory=root))
with open(portfile + ".tmp", "w") as f:
    f.write(str(srv.server_address[1]))
os.rename(portfile + ".tmp", portfile)
srv.serve_forever()
'@

$Python = 'python'
if (Get-Command python3 -ErrorAction SilentlyContinue) { $Python = 'python3' }
$PortFile = Join-Path $Work 'port'

try {
    $Server = Start-Process -FilePath $Python -ArgumentList @("`"$ServerPy`"", "`"$Fix`"", "`"$PortFile`"") -PassThru -WindowStyle Hidden
    $n = 0
    while (-not (Test-Path -LiteralPath $PortFile)) {
        $n++
        if ($n -gt 200) { throw 'fixture server did not start' }
        Start-Sleep -Milliseconds 100
    }
    $Port = (Get-Content -LiteralPath $PortFile -Raw).Trim()
    $Base = "http://127.0.0.1:$Port/releases"

    # ------------------------------------------------------------ runner

    $Home0 = Join-Path $Work 'home'
    New-Item -ItemType Directory -Path $Home0 | Out-Null
    $Dir = Join-Path $Work 'Programs\kioku dir'
    $Exe = Join-Path $Dir 'kioku.exe'
    $PathFile = Join-Path $Work 'user-path.txt'
    Set-Content -LiteralPath $PathFile -Value 'C:\Windows\system32;C:\Tools' -NoNewline

    # Runs install.ps1 with $InstallArgs in a fresh $Shell; sets $script:Out and $script:Rc.
    # $Mode 'file' = `-File install.ps1`, 'block' = the documented `& ([scriptblock]::Create(...))`,
    # 'iex' = what `irm http://<server>/i/<code>.ps1 | iex` runs: the server's script (install.ps1
    # in `& { ... }` with $KiokuJoinUrl / $KiokuJoinCode first) piped to Invoke-Expression, then
    # prints this window's $env:Path and whether a join variable leaked into it.
    function Invoke-Install([string[]]$InstallArgs, [string]$Mode = 'file', [hashtable]$ExtraEnv = @{}) {
        $saved = @{}
        $vars = @{
            KIOKU_DOWNLOAD_BASE = $Base
            KIOKU_USER_PATH_FILE = $PathFile
            KIOKU_ARCH = $null
            KIOKU_VERSION = $null
            KIOKU_INSTALL_DIR = $null
        }
        foreach ($k in $ExtraEnv.Keys) { $vars[$k] = $ExtraEnv[$k] }
        foreach ($k in $vars.Keys) {
            $saved[$k] = [Environment]::GetEnvironmentVariable($k, 'Process')
            [Environment]::SetEnvironmentVariable($k, $vars[$k], 'Process')
        }
        $eap = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        try {
            if ($Mode -eq 'rawiex') {
                # What `$env:KIOKU_JOIN='<server>/<code>'; irm <github>/install.ps1 | iex` runs.
                $cmd = "[System.IO.File]::ReadAllText('$InstallPs1') | Invoke-Expression; Write-Host ('ENVLEFT=' + [string](Test-Path Env:KIOKU_JOIN)); Write-Host ('FNLEFT=' + [string](Test-Path function:Invoke-KiokuInstall)); Write-Host ('EAP=' + `$ErrorActionPreference)"
                $script:Out = (& $Shell -NoProfile -Command $cmd 2>&1 | Out-String)
            } elseif ($Mode -eq 'iex') {
                $served = Join-Path $Work 'served.ps1'
                $text = "& {`n`$KiokuJoinUrl = '$($InstallArgs[0])'`n`$KiokuJoinCode = '$($InstallArgs[1])'`n" + [System.IO.File]::ReadAllText($InstallPs1) + "`n}`n"
                [System.IO.File]::WriteAllText($served, $text)
                $cmd = "[System.IO.File]::ReadAllText('$served') | Invoke-Expression; Write-Host ('SESSION-PATH=' + `$env:Path); Write-Host ('LEAK=' + [string](Test-Path variable:KiokuJoinUrl))"
                $script:Out = (& $Shell -NoProfile -ExecutionPolicy Bypass -Command $cmd 2>&1 | Out-String)
            } elseif ($Mode -eq 'block') {
                $quoted = ($InstallArgs | ForEach-Object { "'" + ($_ -replace "'", "''") + "'" }) -join ' '
                $cmd = "& ([scriptblock]::Create((Get-Content -Raw -LiteralPath '$InstallPs1'))) $quoted"
                $script:Out = (& $Shell -NoProfile -ExecutionPolicy Bypass -Command $cmd 2>&1 | Out-String)
            } else {
                $script:Out = (& $Shell -NoProfile -ExecutionPolicy Bypass -File $InstallPs1 @InstallArgs 2>&1 | Out-String)
            }
            $script:Rc = $LASTEXITCODE
        } finally {
            $ErrorActionPreference = $eap
            foreach ($k in $saved.Keys) { [Environment]::SetEnvironmentVariable($k, $saved[$k], 'Process') }
        }
    }

    function Installed-Version {
        if (-not (Test-Path -LiteralPath $Exe)) { return '' }
        return (& $Exe --version 2>&1 | Out-String).Trim()
    }

    function Run-Exe([string[]]$ExeArgs) {
        $eap = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        try {
            $script:Out = (& $Exe @ExeArgs 2>&1 | Out-String)
            $script:Rc = $LASTEXITCODE
        } finally {
            $ErrorActionPreference = $eap
        }
    }

    $psVersion = (& $Shell -NoProfile -Command '$PSVersionTable.PSVersion.ToString()' | Out-String).Trim()
    Write-Host "install.ps1 under $Shell ($psVersion)"

    # ------------------------------------------------------------ cases

    Invoke-Install @('-InstallDir', $Dir, '-ClientOnly', 'http://h.lan:7391', 'tok-SECRET-1', '--agents', 'codex,claude-code')
    Check 'latest resolves via redirect; x64 -> x86_64-pc-windows-msvc' ($script:Rc -eq 0 -and (Has "installing kioku v9.9.9 ($Target)") -and (Installed-Version) -eq "kioku 9.9.9 ($Target)")
    Check '-ClientOnly <url> <token> and other args reach kioku setup' (Has 'STUB-ARGV: [setup] [--client-only] [http://h.lan:7391] [tok-SECRET-1] [--agents] [codex,claude-code]')
    $installerLines = ($script:Out -split "`r?`n" | Where-Object { $_ -like 'kioku-install:*' }) -join "`n"
    Check 'the token is never echoed by the installer' (-not $installerLines.Contains('tok-SECRET-1'))
    Check 'the install dir is added to the user PATH by default' ((Has "added $Dir to your user PATH") -and (Get-Content -LiteralPath $PathFile -Raw) -eq "C:\Windows\system32;C:\Tools;$Dir")
    $elevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    if ($elevated) {
        Check 'elevated PowerShell: mentioned, not an error' ($script:Rc -eq 0 -and (Has 'runs as administrator, which is not needed'))
    } else {
        Check 'normal PowerShell: no elevation note' (-not (Has 'runs as administrator'))
    }
    Check 'no kioku.exe.new left behind' (-not (Test-Path -LiteralPath "$Exe.new"))

    # Replace a running kioku.exe (SPEC-M2.2 section 5), through the documented scriptblock form.
    $Sleeper = Start-Process -FilePath $Exe -ArgumentList 'sleep' -PassThru -WindowStyle Hidden
    Start-Sleep -Milliseconds 300
    Invoke-Install @('-InstallDir', $Dir, '-Version', '9.9.8', '-NoSetup') 'block'
    Check 'scriptblock form; -Version without v; replaces the running exe by renaming it' ($script:Rc -eq 0 -and (Installed-Version) -eq "kioku 9.9.8 ($Target)" -and (Test-Path -LiteralPath "$Exe.old") -and -not (Has 'STUB-ARGV') -and (Has 'done (-NoSetup)'))
    Stop-Process -Id $Sleeper.Id -Force -ErrorAction SilentlyContinue
    $Sleeper = $null
    Start-Sleep -Milliseconds 300

    Invoke-Install @('-InstallDir', $Dir, '-Version', 'v9.9.8', '-NoSetup', '-AddToPath')
    Check '-AddToPath (old flag) is accepted; the dir is on the user PATH once' ($script:Rc -eq 0 -and (Get-Content -LiteralPath $PathFile -Raw) -eq "C:\Windows\system32;C:\Tools;$Dir" -and -not (Has 'added'))
    Check 'a stale kioku.exe.old is removed, and a replaced exe nothing runs leaves none' (-not (Test-Path -LiteralPath "$Exe.old"))
    Invoke-Install @('-InstallDir', $Dir, '-Version', 'v9.9.8', '-NoSetup')
    Check 'no PATH hint when the dir is on the user PATH' ($script:Rc -eq 0 -and -not (Has 'not on your user PATH'))

    Set-Content -LiteralPath $PathFile -Value 'C:\Windows\system32;C:\Tools' -NoNewline
    Invoke-Install @('-InstallDir', $Dir, '-Version', 'v9.9.8', '-NoSetup', '-NoPath')
    Check '-NoPath: the user PATH is not edited, the exact command is shown' ($script:Rc -eq 0 -and (Has 'is not on your user PATH') -and (Has "SetEnvironmentVariable('Path'") -and (Get-Content -LiteralPath $PathFile -Raw) -eq 'C:\Windows\system32;C:\Tools')

    # Join mode (SPEC-M2.3 section 4.1): the pasted `irm .../i/<code>.ps1 | iex`.
    Invoke-Install @('http://192.168.1.240:7391', 'K7Q2M9XD') 'iex' @{ KIOKU_VERSION = 'v9.9.8'; KIOKU_INSTALL_DIR = $Dir }
    Check 'iex join mode: kioku.exe join <url> <code>, no setup' ($script:Rc -eq 0 -and (Has 'STUB-ARGV: [join] [http://192.168.1.240:7391] [K7Q2M9XD]') -and -not (Has '[setup]'))
    Check 'iex join mode: user PATH and this window''s $env:Path both get the dir' ((Get-Content -LiteralPath $PathFile -Raw) -eq "C:\Windows\system32;C:\Tools;$Dir" -and ($script:Out -split "`r?`n" | Where-Object { $_ -like 'SESSION-PATH=*' -and $_.Contains($Dir) }))
    Check 'iex join mode: bilingual end message, no variables left in the window' ((Has 'The kioku command works in new PowerShell windows') -and (Has 'LEAK=False'))
    # Regression (real Windows 11, 2026-09-29): a kioku.exe.old still in use (a directory with a
    # file inside stands in for a locked copy) must not block the update.
    $locked = Join-Path $Dir 'kioku.exe.old'
    New-Item -ItemType Directory -Force -Path $locked | Out-Null
    Set-Content -LiteralPath (Join-Path $locked 'in-use') -Value 'x'
    Invoke-Install @('-InstallDir', $Dir, '-Version', 'v9.9.8', '-NoSetup')
    Check 'a locked kioku.exe.old does not block the update' ($script:Rc -eq 0 -and (Test-Path -LiteralPath (Join-Path $Dir 'kioku.exe')) -and (Test-Path -LiteralPath $locked))
    Remove-Item -LiteralPath $locked -Recurse -Force -ErrorAction SilentlyContinue

    # SPEC-M2.3 section 9: the Windows line kioku invite prints since v0.6.1.
    Invoke-Install @() 'rawiex' @{ KIOKU_VERSION = 'v9.9.8'; KIOKU_INSTALL_DIR = $Dir; KIOKU_JOIN = '192.168.1.240:7391/K7Q2M9XD' }
    Check 'KIOKU_JOIN + irm | iex: kioku.exe join <url> <code>' ($script:Rc -eq 0 -and (Has 'STUB-ARGV: [join] [http://192.168.1.240:7391] [K7Q2M9XD]'))
    Check 'KIOKU_JOIN + irm | iex: nothing left in the window (env var, functions, ErrorActionPreference)' ((Has 'ENVLEFT=False') -and (Has 'FNLEFT=False') -and -not (Has 'EAP=Stop'))
    Invoke-Install @('-InstallDir', $Dir, '-Version', 'v9.9.8', '-Join', 'http://h:7391', 'CODE2345', '--agents', 'codex')
    Check '-Join <url> <code> plus args for kioku join' ($script:Rc -eq 0 -and (Has 'STUB-ARGV: [join] [http://h:7391] [CODE2345] [--agents] [codex]'))
    Invoke-Install @('-InstallDir', $Dir, '-Version', 'v9.9.8', '-Join', 'http://h:7391')
    Check '-Join without a code is refused' ($script:Rc -ne 0 -and (Has '-Join needs <url> <code>'))

    Invoke-Install @('-InstallDir', $Dir, '-Version', 'v9.9.7', '-NoSetup')
    Check 'checksum mismatch aborts, nothing installed' ($script:Rc -ne 0 -and (Has 'checksum mismatch') -and (Installed-Version) -eq "kioku 9.9.8 ($Target)")
    Invoke-Install @('-InstallDir', $Dir, '-Version', 'v9.9.6', '-NoSetup')
    Check 'missing SHA256SUMS falls back to <asset>.sha256' ($script:Rc -eq 0 -and (Installed-Version) -eq "kioku 9.9.6 ($Target)")
    Invoke-Install @('-InstallDir', $Dir, '-Version', 'v9.9.5', '-NoSetup')
    Check 'no checksum at all aborts, nothing installed' ($script:Rc -ne 0 -and (Has 'refusing to install an unverified binary') -and (Installed-Version) -eq "kioku 9.9.6 ($Target)")
    Invoke-Install @('-InstallDir', $Dir, '-Version', 'v9.9.4', '-NoSetup')
    Check 'a binary that does not run is not installed' ($script:Rc -ne 0 -and (Has 'does not run here') -and (Installed-Version) -eq "kioku 9.9.6 ($Target)")
    Invoke-Install @('-InstallDir', $Dir, '-Version', 'v9.0.0', '-NoSetup')
    Check 'a release without the Windows asset fails clearly' ($script:Rc -ne 0 -and (Has "release v9.0.0 has no kioku-v9.0.0-$Target.tar.gz"))

    Invoke-Install @('-InstallDir', $Dir, '-NoSetup') 'file' @{ KIOKU_ARCH = 'ARM64' }
    Check 'arm64 -> clear error' ($script:Rc -ne 0 -and (Has 'no prebuilt kioku for Windows on ARM64'))
    Invoke-Install @('-InstallDir', $Dir, '-NoSetup') 'file' @{ KIOKU_DOWNLOAD_BASE = "http://127.0.0.1:$Port/broken/releases" }
    Check 'HTTP 500 on releases/latest -> network error' ($script:Rc -ne 0 -and (Has 'could not look up the latest release'))
    Invoke-Install @('-InstallDir', $Dir, '-NoSetup') 'file' @{ KIOKU_DOWNLOAD_BASE = "http://127.0.0.1:$Port/empty/releases" }
    Check 'no release published -> clear error' ($script:Rc -ne 0 -and (Has 'no published release found for misorafa/kioku'))

    Invoke-Install @('--dry-run', "it's") 'file' @{ KIOKU_VERSION = 'v9.9.9'; KIOKU_INSTALL_DIR = $Dir }
    Check 'KIOKU_VERSION / KIOKU_INSTALL_DIR env; unknown args go to setup verbatim' ($script:Rc -eq 0 -and (Installed-Version) -eq "kioku 9.9.9 ($Target)" -and (Has "STUB-ARGV: [setup] [--dry-run] [it's]"))

    $DefaultDir = Join-Path $Work 'localappdata\Programs\kioku'
    Invoke-Install @('-NoSetup') 'file' @{ LOCALAPPDATA = (Join-Path $Work 'localappdata') }
    Check 'default dir is %LOCALAPPDATA%\Programs\kioku' ($script:Rc -eq 0 -and (Test-Path -LiteralPath (Join-Path $DefaultDir 'kioku.exe')))

    # ------------------------------------------------------------ the real kioku.exe

    if ($RealBin) {
        $RealDir = Join-Path $Work 'real bin'
        $Exe = Join-Path $RealDir 'kioku.exe'
        Invoke-Install @('-InstallDir', $RealDir, '-Version', 'v9.9.3', '-NoSetup')
        Check 'real kioku.exe installed and runs' ($script:Rc -eq 0 -and (Installed-Version).StartsWith('kioku '))

        $saved = @{}
        $vars = @{
            HOME = $Home0
            USERPROFILE = $Home0
            KIOKU_DATA_DIR = $null
            KIOKU_SERVER_URL = 'http://127.0.0.1:9'
            KIOKU_DOWNLOAD_BASE = $Base
        }
        foreach ($k in $vars.Keys) {
            $saved[$k] = [Environment]::GetEnvironmentVariable($k, 'Process')
            [Environment]::SetEnvironmentVariable($k, $vars[$k], 'Process')
        }
        try {
            Run-Exe @('setup', '--dry-run')
            Check 'real: setup without --client-only is refused on Windows' ($script:Rc -ne 0 -and (Has 'Windows runs kioku as a client: kioku setup --client-only <url> <token>'))

            # Hook stdin as Claude Code writes it on Windows: raw UTF-8, here with a BOM and CRLF.
            # Built from code points: Windows PowerShell 5.1 reads this file as ANSI.
            $yamada = -join [char[]](0x5C71, 0x7530)
            $nihongo = -join [char[]](0x65E5, 0x672C, 0x8A9E, 0x306E, 0x30D7, 0x30ED, 0x30F3, 0x30D7, 0x30C8)
            $payload = "{`r`n `"session_id`": `"win-1`", `"cwd`": `"C:\\Users\\$yamada\\repo`", `"hook_event_name`": `"UserPromptSubmit`", `"prompt`": `"$nihongo`"`r`n}`r`n"
            $bytes = [byte[]](0xEF, 0xBB, 0xBF) + [System.Text.Encoding]::UTF8.GetBytes($payload)
            $psi = New-Object System.Diagnostics.ProcessStartInfo
            $psi.FileName = $Exe
            $psi.Arguments = 'hook user-prompt-submit'
            $psi.UseShellExecute = $false
            $psi.RedirectStandardInput = $true
            $psi.RedirectStandardOutput = $true
            $psi.RedirectStandardError = $true
            $p = [System.Diagnostics.Process]::Start($psi)
            $p.StandardInput.BaseStream.Write($bytes, 0, $bytes.Length)
            $p.StandardInput.Close()
            $stdout = $p.StandardOutput.ReadToEnd()
            $null = $p.StandardError.ReadToEnd()
            $p.WaitForExit()
            $log = Join-Path $Home0 '.kioku\logs\hook.log'
            $logText = ''
            if (Test-Path -LiteralPath $log) { $logText = Get-Content -LiteralPath $log -Raw -Encoding UTF8 }
            $script:Out = "stdout: $stdout`nhook.log: $logText"
            Check 'real: hook reads BOM + CRLF + Japanese stdin as JSON, fails open' ($p.ExitCode -eq 0 -and -not $logText.Contains('not JSON'))

            Run-Exe @('update', '--version', 'v9.9.2')
            $updated = ($script:Rc -eq 0 -and (Has 'updated') -and (Test-Path -LiteralPath "$Exe.old") -and -not (Test-Path -LiteralPath "$Exe.new"))
            Check 'real: kioku update renames the running kioku.exe aside' $updated
            Run-Exe @('--version')
            Check 'real: the next run removes kioku.exe.old' ($script:Rc -eq 0 -and -not (Test-Path -LiteralPath "$Exe.old"))
        } finally {
            foreach ($k in $saved.Keys) { [Environment]::SetEnvironmentVariable($k, $saved[$k], 'Process') }
        }
    }
} finally {
    if ($Sleeper) { Stop-Process -Id $Sleeper.Id -Force -ErrorAction SilentlyContinue }
    if ($Server) { Stop-Process -Id $Server.Id -Force -ErrorAction SilentlyContinue }
    Start-Sleep -Milliseconds 200
    Remove-Item -LiteralPath $Work -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host ''
Write-Host "passed: $script:Passed, failed: $script:Failed"
if ($script:Failed -gt 0) { exit 1 }
