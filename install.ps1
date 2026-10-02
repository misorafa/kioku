# kioku installer for Windows (docs/SPEC-M2.2.md section 6, docs/SPEC-M2.3.md section 4).
# Windows PowerShell 5.1 or PowerShell 7, normal or elevated.
#
#   $env:KIOKU_JOIN='192.168.1.240:7391/K7Q2M9XD'; irm https://raw.githubusercontent.com/misorafa/kioku/main/install.ps1 | iex   # the line `kioku invite` prints
#   & ([scriptblock]::Create((irm https://raw.githubusercontent.com/misorafa/kioku/main/install.ps1))) -ClientOnly http://192.168.1.240:7391 <token>
#   & ([scriptblock]::Create((irm .../install.ps1))) -Version v0.5.0 -NoSetup
#   $env:KIOKU_JOIN='home.lan:7391/<code>'; irm https://raw.githubusercontent.com/misorafa/kioku/main/install.ps1 | iex
#
# Downloads kioku.exe (x86_64-pc-windows-msvc) from the GitHub release, verifies its
# SHA-256 against the release's SHA256SUMS, installs it to
# %LOCALAPPDATA%\Programs\kioku\kioku.exe, adds that directory to the user PATH (and to this
# window's $env:Path), then runs `kioku join <url> <code>` (join mode: $KiokuJoinUrl and
# $KiokuJoinCode set, as the former `/i/<code>.ps1` script of a kioku server did, or
# $env:KIOKU_JOIN, or -Join) or
# `kioku setup --client-only <url> <token>`.
# Windows machines are kioku clients only: the server runs on macOS or Linux.
# Never needs administrator rights; it always installs for the user who runs it.
#
# Options (an option wins over its environment variable):
#   -Version <tag>        KIOKU_VERSION      release tag (default: latest)
#   -InstallDir <dir>     KIOKU_INSTALL_DIR  destination (default: %LOCALAPPDATA%\Programs\kioku)
#   -Repo <owner/name>    KIOKU_REPO         GitHub repository (default: misorafa/kioku)
#   -Join <url> <code>                       run `kioku join <url> <code>` (an invite code)
#   -ClientOnly <url> <token>                run `kioku setup --client-only <url> <token>`
#   -NoSetup                                 install only, do not run `kioku setup` / `join`
#   -NoPath                                  do not add the install directory to PATH
#   anything else is passed to `kioku setup` / `kioku join`, e.g. --agents codex,claude-code.
# Test-only overrides: KIOKU_DOWNLOAD_BASE (replaces https://github.com/<repo>/releases),
# KIOKU_ARCH (replaces PROCESSOR_ARCHITECTURE), KIOKU_USER_PATH_FILE (a file standing in
# for the user PATH in the registry).
#
# Errors are thrown (never `exit`), so running it in an open PowerShell window never
# closes the window. The body lives in functions called on the last line, so a truncated
# download runs nothing. This file stays ASCII (Windows PowerShell 5.1 reads a BOM-less
# -File script as ANSI): Japanese text is written with \u escapes (see Ja).

# Everything runs inside one script block, so `irm .../install.ps1 | iex` leaves no functions,
# variables or $ErrorActionPreference behind in the user's window (SPEC-M2.3 section 9).
& {
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

function Say([string]$Message) {
    Write-Host "kioku-install: $Message"
}

function Warn([string]$Message) {
    Write-Host "kioku-install: warning: $Message"
}

function Die([string]$Message) {
    throw "kioku-install: error: $Message"
}

# Japanese text from \u escapes (keeps this file ASCII).
function Ja([string]$Escaped) {
    return [regex]::Unescape($Escaped)
}

# True in an elevated ("Run as administrator") PowerShell.
function Test-Elevated {
    try {
        $id = [Security.Principal.WindowsIdentity]::GetCurrent()
        return ([Security.Principal.WindowsPrincipal]$id).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    } catch {
        return $false
    }
}

# The release target for this machine; dies on anything but x64.
function Get-KiokuTarget {
    $arch = $env:KIOKU_ARCH
    if (-not $arch) {
        # A 32-bit PowerShell on 64-bit Windows reports x86 here and AMD64 in PROCESSOR_ARCHITEW6432.
        $arch = $env:PROCESSOR_ARCHITEW6432
        if (-not $arch) { $arch = $env:PROCESSOR_ARCHITECTURE }
    }
    switch ($arch.ToUpperInvariant()) {
        'AMD64' { return 'x86_64-pc-windows-msvc' }
        'X64' { return 'x86_64-pc-windows-msvc' }
        default {
            Die "no prebuilt kioku for Windows on $arch (only x64 is released); build it with: cargo install --locked --git https://github.com/misorafa/kioku kioku-cli"
        }
    }
}

# Final URL of a GET after redirects; throws on a network or HTTP error.
function Get-FinalUrl([string]$Url) {
    $req = [System.Net.WebRequest]::Create($Url)
    $req.Method = 'HEAD'
    $req.AllowAutoRedirect = $true
    $req.UserAgent = 'kioku-install'
    $req.Timeout = 30000
    $resp = $req.GetResponse()
    try {
        return $resp.ResponseUri.AbsoluteUri
    } finally {
        $resp.Close()
    }
}

# The tag to install ('' when no release is published).
function Resolve-Tag([string]$Base, [string]$Version) {
    if ($Version -ne 'latest') { return $Version }
    try {
        $final = Get-FinalUrl "$Base/latest"
    } catch {
        Die "could not look up the latest release at $Base/latest (network or HTTP error): $($_.Exception.Message). Check the connection and retry, or pass -Version <tag>"
    }
    $final = ($final -split '[?#]')[0].TrimEnd('/')
    $marker = '/releases/tag/'
    $i = $final.LastIndexOf($marker)
    if ($i -lt 0) { return '' }
    $tag = $final.Substring($i + $marker.Length)
    if ($tag.Contains('/')) { return '' }
    return $tag
}

# Downloads $Url to $OutFile; $false on any HTTP or network error.
function Get-File([string]$Url, [string]$OutFile) {
    try {
        Invoke-WebRequest -Uri $Url -OutFile $OutFile -UseBasicParsing -UserAgent 'kioku-install' -TimeoutSec 300
        return $true
    } catch {
        Remove-Item -LiteralPath $OutFile -Force -ErrorAction SilentlyContinue
        return $false
    }
}

# The checksum of $Asset in `<hex>  [*]<file>` lines, lowercase, or ''.
function Find-Checksum([string]$Text, [string]$Asset) {
    foreach ($line in ($Text -split "`r?`n")) {
        $parts = $line.Trim() -split '\s+'
        if ($parts.Count -ge 2 -and $parts[1].TrimStart('*') -eq $Asset) {
            return $parts[0].ToLowerInvariant()
        }
    }
    return ''
}

# The expected SHA-256 of $Asset: SHA256SUMS, else <asset>.sha256; dies when neither has it.
function Get-ExpectedSum([string]$Base, [string]$Tag, [string]$Asset, [string]$Tmp) {
    foreach ($name in @('SHA256SUMS', "$Asset.sha256")) {
        $file = Join-Path $Tmp $name
        if (Get-File "$Base/download/$Tag/$name" $file) {
            $sum = Find-Checksum (Get-Content -LiteralPath $file -Raw) $Asset
            if ($sum) { return $sum }
        }
    }
    Die "no checksum for $Asset in the $Tag release (SHA256SUMS / $Asset.sha256); refusing to install an unverified binary"
}

# Windows' own bsdtar (Git's GNU tar on PATH reads `C:\...` as a remote host).
function Get-Tar {
    $system = Join-Path $env:SystemRoot 'System32\tar.exe'
    if (Test-Path -LiteralPath $system) { return $system }
    return 'tar.exe'
}

# Puts $New in the place of $Exe: a running kioku.exe cannot be overwritten, but it can be
# renamed. So every kioku.exe.old* that nothing runs any more is deleted, kioku.exe is moved
# aside to a free name (kioku.exe.old, or kioku.exe.old-<utc time> when an old copy is still
# in use -- a `kioku mcp` an app such as Orca keeps alive in the background; this used to
# need a reboot), and kioku.exe.new takes its place (SPEC-M2.2 section 5).
function Remove-OldCopies([string]$Exe) {
    $dir = Split-Path -Parent $Exe
    $leaf = Split-Path -Leaf $Exe
    Get-ChildItem -LiteralPath $dir -Filter "$leaf.old*" -File -ErrorAction SilentlyContinue |
        ForEach-Object { Remove-Item -LiteralPath $_.FullName -Force -ErrorAction SilentlyContinue }
}

function Move-IntoPlace([string]$New, [string]$Exe) {
    Remove-OldCopies $Exe
    $old = "$Exe.old"
    if (Test-Path -LiteralPath $old) {
        $old = "$Exe.old-" + [DateTime]::UtcNow.ToString('yyyyMMddHHmmssfff')
    }
    $moved = $false
    if (Test-Path -LiteralPath $Exe) {
        try {
            Move-Item -LiteralPath $Exe -Destination $old -Force
            $moved = $true
        } catch {
            Remove-Item -LiteralPath $New -Force -ErrorAction SilentlyContinue
            Die "cannot move $Exe out of the way: $($_.Exception.Message)"
        }
    }
    try {
        Move-Item -LiteralPath $New -Destination $Exe -Force
    } catch {
        if ($moved) { Move-Item -LiteralPath $old -Destination $Exe -Force -ErrorAction SilentlyContinue }
        Remove-Item -LiteralPath $New -Force -ErrorAction SilentlyContinue
        Die "cannot replace $($Exe): $($_.Exception.Message)"
    }
    # Not running any more (the usual case): the moved-aside copy can go right away.
    Remove-OldCopies $Exe
}

# Downloads, verifies, extracts and installs kioku.exe; returns its path.
function Install-Kioku([string]$Base, [string]$Tag, [string]$Target, [string]$Dir) {
    $asset = "kioku-$Tag-$Target.tar.gz"
    $tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("kioku-install-" + [System.Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        Say "downloading $asset"
        $tarball = Join-Path $tmp $asset
        if (-not (Get-File "$Base/download/$Tag/$asset" $tarball)) {
            Die "release $Tag has no $asset"
        }
        $expected = Get-ExpectedSum $Base $Tag $asset $tmp
        $actual = (Get-FileHash -LiteralPath $tarball -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actual -ne $expected) {
            Die "checksum mismatch for $asset (expected $expected, got $actual); nothing was installed"
        }
        Say "checksum ok ($actual)"
        $x = Join-Path $tmp 'x'
        New-Item -ItemType Directory -Path $x | Out-Null
        & (Get-Tar) -xzf $tarball -C $x
        if ($LASTEXITCODE -ne 0) { Die "cannot extract $asset" }
        $bin = Join-Path (Join-Path $x "kioku-$Tag-$Target") 'kioku.exe'
        if (-not (Test-Path -LiteralPath $bin)) { Die "$asset does not contain kioku.exe" }

        if (-not (Test-Path -LiteralPath $Dir)) {
            New-Item -ItemType Directory -Path $Dir -Force | Out-Null
        }
        $exe = Join-Path $Dir 'kioku.exe'
        if (Test-Path -LiteralPath $exe -PathType Container) {
            Die "$exe is a directory; move it away and re-run"
        }
        # Run the extracted copy (PowerShell would hand `kioku.exe.new` to a file association).
        $ver = ''
        $ok = $false
        try {
            $ver = (& $bin --version 2>&1 | Out-String).Trim()
            $ok = ($LASTEXITCODE -eq 0)
        } catch {
            $ver = $_.Exception.Message
        }
        if (-not $ok) {
            Die "the downloaded kioku.exe does not run here: $(($ver -split "`n")[0]); nothing was installed"
        }
        $new = "$exe.new"
        try {
            Copy-Item -LiteralPath $bin -Destination $new -Force
        } catch {
            Die "cannot write to $($Dir): $($_.Exception.Message)"
        }
        Move-IntoPlace $new $exe
        Say "installed $ver to $exe"
        return $exe
    } finally {
        Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function Get-UserPath {
    if ($env:KIOKU_USER_PATH_FILE) {
        if (Test-Path -LiteralPath $env:KIOKU_USER_PATH_FILE) {
            return (Get-Content -LiteralPath $env:KIOKU_USER_PATH_FILE -Raw).Trim()
        }
        return ''
    }
    $p = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ($null -eq $p) { return '' }
    return $p
}

function Set-UserPath([string]$Value) {
    if ($env:KIOKU_USER_PATH_FILE) {
        Set-Content -LiteralPath $env:KIOKU_USER_PATH_FILE -Value $Value -NoNewline
        return
    }
    [Environment]::SetEnvironmentVariable('Path', $Value, 'User')
}

# Puts $Dir on the user PATH (SPEC-M2.3 section 4.2) and on this window's $env:Path, or with
# -NoPath only tells how. Returns $true when $Dir is on the user PATH afterwards.
function Update-UserPath([string]$Dir, [bool]$NoPath) {
    $norm = { param($p) $p.Trim().TrimEnd('\').ToLowerInvariant() }
    $user = Get-UserPath
    $entries = @($user -split ';' | Where-Object { $_.Trim() -ne '' })
    $present = $false
    foreach ($e in $entries) {
        if ((& $norm $e) -eq (& $norm $Dir)) { $present = $true }
    }
    if ($NoPath) {
        if (-not $present) {
            Say "$Dir is not on your user PATH; to use the kioku command, add it:"
            Say "  [Environment]::SetEnvironmentVariable('Path', [Environment]::GetEnvironmentVariable('Path', 'User') + ';$Dir', 'User')"
            Say "(kioku setup and the agent hooks use the absolute path, so they work either way)"
        }
        return $present
    }
    if (-not $present) {
        $entries += $Dir
        Set-UserPath ($entries -join ';')
        # The marker `kioku uninstall` looks for before it removes this entry (SPEC-M3.3
        # section 3): only a PATH entry this installer added is ever removed.
        try {
            [System.IO.File]::WriteAllText((Join-Path $Dir 'kioku-path-entry.txt'), "$Dir`r`n")
        } catch {
            Warn "could not write $Dir\kioku-path-entry.txt (kioku uninstall will leave the PATH entry)"
        }
        Say "added $Dir to your user PATH / $(Ja '\u30e6\u30fc\u30b6\u30fc\u306e PATH \u306b\u8ffd\u52a0\u3057\u307e\u3057\u305f') (-NoPath skips this)"
    }
    # This window too: after `irm ... | iex` the kioku command works right away.
    $session = @($env:Path -split ';' | Where-Object { $_.Trim() -ne '' })
    $inSession = $false
    foreach ($e in $session) {
        if ((& $norm $e) -eq (& $norm $Dir)) { $inSession = $true }
    }
    if (-not $inSession) { $env:Path = (($session + $Dir) -join ';') }
    return $true
}

function Invoke-KiokuInstall([object[]]$Arguments) {
    $version = $env:KIOKU_VERSION
    if (-not $version) { $version = 'latest' }
    $dir = $env:KIOKU_INSTALL_DIR
    $repo = $env:KIOKU_REPO
    if (-not $repo) { $repo = 'misorafa/kioku' }
    $setup = $true
    $noPath = $false
    # Join mode (SPEC-M2.3 section 4.1): these two variables (the server's former /i/<code>.ps1
    # set them; SPEC-M2.7 section 4 removed that route), or KIOKU_JOIN below.
    $joinUrl = ''
    $joinCode = ''
    if ($KiokuJoinUrl) { $joinUrl = [string]$KiokuJoinUrl }
    if ($KiokuJoinCode) { $joinCode = [string]$KiokuJoinCode }
    # SPEC-M2.3 section 9: `$env:KIOKU_JOIN='<server>:<port>/<code>'; irm <github>/install.ps1 | iex`
    # -- the script comes from GitHub over https, not from a bare LAN IP, and no execution-policy
    # bypass is involved (Defender flagged `powershell -ExecutionPolicy Bypass -c irm http://<ip>/... | iex`).
    if (-not $joinUrl -and $env:KIOKU_JOIN) {
        $j = ([string]$env:KIOKU_JOIN).Trim()
        $scheme = 'http://'
        if ($j -match '^(https?://)') { $scheme = $Matches[1]; $j = $j.Substring($scheme.Length) }
        $slash = $j.LastIndexOf('/')
        if ($slash -lt 1 -or $slash -eq $j.Length - 1) {
            Die 'KIOKU_JOIN must look like <server>:<port>/<code>, as kioku invite prints it'
        }
        $joinUrl = $scheme + $j.Substring(0, $slash)
        $joinCode = $j.Substring($slash + 1)
        Remove-Item Env:KIOKU_JOIN -ErrorAction SilentlyContinue
    }
    $pass = New-Object System.Collections.Generic.List[string]

    $i = 0
    while ($i -lt $Arguments.Count) {
        $a = [string]$Arguments[$i]
        $needsValue = @('-Version', '-InstallDir', '-Repo')
        if ($needsValue -contains $a) {
            if ($i + 1 -ge $Arguments.Count) { Die "$a needs a value" }
            $value = [string]$Arguments[$i + 1]
            switch ($a) {
                '-Version' { $version = $value }
                '-InstallDir' { $dir = $value }
                '-Repo' { $repo = $value }
            }
            $i += 2
            continue
        }
        if ($a -eq '-ClientOnly' -or $a -eq '--client-only') {
            # URL and token go to `kioku setup` verbatim, whatever they look like.
            $pass.Add('--client-only')
            $i += 1
            for ($k = 0; $k -lt 2 -and $i -lt $Arguments.Count; $k++) {
                $pass.Add([string]$Arguments[$i])
                $i += 1
            }
            continue
        }
        if ($a -eq '-Join' -or $a -eq '--join') {
            if ($i + 2 -ge $Arguments.Count) { Die "$a needs <url> <code>" }
            $joinUrl = [string]$Arguments[$i + 1]
            $joinCode = [string]$Arguments[$i + 2]
            $i += 3
            continue
        }
        if ($a -eq '-NoSetup' -or $a -eq '--no-setup') { $setup = $false; $i += 1; continue }
        if ($a -eq '-NoPath' -or $a -eq '--no-modify-path') { $noPath = $true; $i += 1; continue }
        # -AddToPath (before v0.6) is the default now.
        if ($a -eq '-AddToPath') { $i += 1; continue }
        if ($a -eq '-Help' -or $a -eq '-h' -or $a -eq '--help') {
            Say 'usage: install.ps1 [-Version <tag>] [-InstallDir <dir>] [-Repo <owner/name>] [-Join <url> <code> | -ClientOnly <url> <token>] [-NoSetup] [-NoPath] [kioku setup options...]'
            return
        }
        if ($a -eq '--') {
            for ($k = $i + 1; $k -lt $Arguments.Count; $k++) { $pass.Add([string]$Arguments[$k]) }
            break
        }
        $pass.Add($a)
        $i += 1
    }

    if ($joinUrl -and -not $joinCode) { Die 'join mode needs both $KiokuJoinUrl and $KiokuJoinCode (or -Join <url> <code>)' }
    if (Test-Elevated) {
        Say "this PowerShell runs as administrator, which is not needed: kioku is installed for $env:USERNAME only / $(Ja '\u7ba1\u7406\u8005\u3068\u3057\u3066\u5b9f\u884c\u3059\u308b\u5fc5\u8981\u306f\u3042\u308a\u307e\u305b\u3093\u3002\u3053\u306e\u30e6\u30fc\u30b6\u30fc\u306b\u3060\u3051\u30a4\u30f3\u30b9\u30c8\u30fc\u30eb\u3057\u307e\u3059\u3002')"
    }
    if (-not $env:LOCALAPPDATA -and -not $dir) { Die 'LOCALAPPDATA is not set; pass -InstallDir' }
    if (-not $dir) { $dir = Join-Path $env:LOCALAPPDATA 'Programs\kioku' }
    $dir = [System.IO.Path]::GetFullPath($dir)
    if ($version -ne 'latest' -and -not $version.StartsWith('v')) { $version = "v$version" }
    $base = $env:KIOKU_DOWNLOAD_BASE
    if (-not $base) { $base = "https://github.com/$repo/releases" }
    $base = $base.TrimEnd('/')

    # Windows PowerShell 5.1 does not offer TLS 1.2 by default.
    try {
        [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    } catch {
        Warn 'could not enable TLS 1.2'
    }

    $target = Get-KiokuTarget
    $tag = Resolve-Tag $base $version
    if (-not $tag) { Die "no published release found for $repo" }
    Say "installing kioku $tag ($target)"
    $exe = Install-Kioku $base $tag $target $dir

    $onPath = Update-UserPath $dir $noPath

    if (-not $setup) {
        if ($joinUrl) {
            Say "done (-NoSetup). Next: & `"$exe`" join $joinUrl $joinCode"
        } else {
            Say "done (-NoSetup). Next: & `"$exe`" setup --client-only <url> <token>"
        }
        return
    }
    if ($joinUrl) {
        # `kioku join` fetches the token itself and never prints it.
        Say "running: $exe join $joinUrl"
        $joinArgs = @('join', $joinUrl, $joinCode) + $pass.ToArray()
        & $exe @joinArgs
        $code = $LASTEXITCODE
        if ($code -ne 0) { Die "kioku join did not finish (exit $code); see the message above" }
        if ($onPath) {
            Say "$(Ja 'kioku \u30b3\u30de\u30f3\u30c9\u306f\u65b0\u3057\u3044 PowerShell \u30a6\u30a3\u30f3\u30c9\u30a6\u3067\u4f7f\u3048\u307e\u3059\u3002') / The kioku command works in new PowerShell windows."
        }
        return
    }
    # The arguments may hold the token: never echo them.
    Say "running: $exe setup"
    $setupArgs = @('setup') + $pass.ToArray()
    & $exe @setupArgs
    $code = $LASTEXITCODE
    if ($code -ne 0) { Die "kioku setup failed (exit $code); fix the problem above and re-run: & `"$exe`" setup ..." }
}

Invoke-KiokuInstall $args
} @args
