# Virtual clean install on Windows (PowerShell 5.1 or 7; saved with a UTF-8 BOM so 5.1 reads
# the Japanese fixtures correctly on a Japanese code page) of the PUBLIC release path: the Windows
# counterpart of scripts/e2e/clean-install.sh, run by .github/workflows/clean-install.yml on a
# pristine windows-latest runner. Never run it on a machine you use: install.ps1 adds each
# throwaway install directory to the user PATH in the registry.
#
#   pwsh scripts/e2e/clean-install.ps1                    # latest release
#   $env:KIOKU_RELEASE_TAG='v0.9.4'; pwsh scripts/e2e/clean-install.ps1
#
# 1. "server": a profile directory (USERPROFILE / HOME / LOCALAPPDATA point into it) gets
#    kioku.exe with install.ps1 -NoSetup; `kioku init`, then `kioku serve` on 127.0.0.1:7391.
#    (A Windows server is unsupported, SPEC-M2.2; it stands in for the Mac/Linux server here
#    because a Windows runner cannot reach one. Its doctor is not judged.)
# 2. `kioku invite --host 127.0.0.1 --uses 2`; the "Windows (PowerShell)" line is captured.
# 3. Clients A and B: their own profile directories with ~/.claude; each pastes the line
#    verbatim (`$env:KIOKU_JOIN='…'; irm …/install.ps1 | iex`, run with Invoke-Expression).
# 4. A: SessionStart, UserPromptSubmit (Japanese), PostToolUse, kioku_handoff_write through the
#    registered `kioku mcp` bridge, Stop (Windows captured fixture shapes, bytes as UTF-8).
#    B: SessionStart must show A's handoff; Japanese `kioku search`; a short session; then A's
#    next SessionStart shows B's automatic handoff. `kioku doctor` exit 0 on A and B with only
#    the warnings in $AllowedWarnClient.
# Prints PASS / FAIL per step; exit 1 on any FAIL. Never prints a token or an invite code.

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
# No Set-StrictMode: it would leak into install.ps1 (run in this scope like `irm | iex`),
# which reads variables a user may not have set ($KiokuJoinUrl), as it always has.

$Repo = 'misorafa/kioku'
$InstallPs1Url = "https://raw.githubusercontent.com/$Repo/main/install.ps1"
$Root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$Fix = Join-Path $Root 'crates\kioku-cli\tests\fixtures\windows\claude-code'
$Port = 7391

# Doctor warnings expected on the Windows clients (no Claude Code binary, only ~/.claude).
# `binary`: the throwaway profiles live under the temp directory, which doctor rightly flags
# ("hooks break when it moves") on a real machine; on a CI runner RUNNER_TEMP is not under
# %TEMP%, so the warning only appears locally.
$AllowedWarnClient = @('binary')

$script:Passed = 0
$script:Failed = 0
$script:Out = ''
$script:Code = 0

function Mask([string]$Text) { return ($Text -replace ":$Port/[A-Za-z0-9]{4,}", ":$Port/<code>") }
function Pass([string]$Name) { $script:Passed++; Write-Host "PASS  [windows] $Name" }
function Fail([string]$Name) {
    $script:Failed++
    Write-Host "FAIL  [windows] $Name"
    $lines = @((Mask $script:Out) -split "`r?`n")
    $lines | Select-Object -Last 40 | ForEach-Object { Write-Host "      | $_" }
}
function Check([string]$Name, [bool]$Ok) { if ($Ok) { Pass $Name } else { Fail $Name } }
function Has([string]$Needle) { return $script:Out.Contains($Needle) }

# --------------------------------------------------------------------------- release

if ($env:KIOKU_RELEASE_TAG) {
    $Tag = $env:KIOKU_RELEASE_TAG
    if (-not $Tag.StartsWith('v')) { $Tag = "v$Tag" }
} else {
    # The releases/latest redirect, as install.ps1 resolves it (the REST API is rate-limited per
    # runner IP without a token).
    $r = Invoke-WebRequest -UseBasicParsing -Method Head -Uri "https://github.com/$Repo/releases/latest"
    $Tag = ($r.BaseResponse.RequestMessage.RequestUri.AbsoluteUri -split '/tag/')[-1]
}
$Version = $Tag.TrimStart('v')
$env:KIOKU_VERSION = $Tag

$Base = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [IO.Path]::GetTempPath() }
$Work = Join-Path $Base "kioku-e2e-$PID"
New-Item -ItemType Directory -Force -Path $Work | Out-Null
$BasePath = $env:Path
foreach ($v in 'KIOKU_DATA_DIR', 'KIOKU_SERVER_URL', 'KIOKU_AUTH_TOKEN', 'KIOKU_JOIN', 'CODEX_HOME') {
    Remove-Item "Env:$v" -ErrorAction SilentlyContinue
}
Write-Host "kioku clean-install e2e (Windows, PowerShell $($PSVersionTable.PSVersion)): release $Tag, profiles under $Work"
Write-Host ''

# --------------------------------------------------------------------------- machines

# Switches this process (and the processes it starts) to machine $Name's profile.
function Use-Machine([string]$Name) {
    $dir = Join-Path $Work $Name
    New-Item -ItemType Directory -Force -Path (Join-Path $dir 'AppData\Local'), (Join-Path $dir 'AppData\Roaming') | Out-Null
    $env:USERPROFILE = $dir
    $env:HOME = $dir
    $env:LOCALAPPDATA = Join-Path $dir 'AppData\Local'
    $env:APPDATA = Join-Path $dir 'AppData\Roaming'
    $env:KIOKU_MACHINE = $Name
    $env:Path = "$BasePath;" + (Join-Path $env:LOCALAPPDATA 'Programs\kioku')
    return $dir
}

function Exe([string]$Name) { return Join-Path $Work "$Name\AppData\Local\Programs\kioku\kioku.exe" }

# Runs kioku.exe like an agent does: stdin from a file (UTF-8 bytes), stdout / stderr captured
# as UTF-8. Sets $script:Out and $script:Code.
function Invoke-Kioku([string]$Exe, [string[]]$Arguments, [string]$Cwd, [string]$StdinFile) {
    if (-not $StdinFile) {
        $StdinFile = Join-Path $Work 'empty.txt'
        [IO.File]::WriteAllBytes($StdinFile, [byte[]]@())
    }
    $o = Join-Path $Work 'stdout.txt'
    $e = Join-Path $Work 'stderr.txt'
    $quoted = $Arguments | ForEach-Object { if ($_ -match '[\s"]') { '"' + ($_ -replace '"', '\"') + '"' } else { $_ } }
    $p = Start-Process -FilePath $Exe -ArgumentList $quoted -WorkingDirectory $Cwd -NoNewWindow -Wait -PassThru `
        -RedirectStandardInput $StdinFile -RedirectStandardOutput $o -RedirectStandardError $e
    $script:Code = $p.ExitCode
    $script:Out = [IO.File]::ReadAllText($o, [Text.Encoding]::UTF8) + [IO.File]::ReadAllText($e, [Text.Encoding]::UTF8)
}

# A Claude Code payload (Windows captured shape) for machine $Name, written as UTF-8 bytes.
function Payload([string]$Name, [string]$Fixture, [string]$Session, [hashtable]$Set) {
    $dir = Join-Path $Work $Name
    $proj = Join-Path $dir 'src\demo'
    $slug = ($proj -replace '[:\\]', '-')
    $j = Get-Content -Raw -Encoding UTF8 (Join-Path $Fix "$Fixture.captured.json") | ConvertFrom-Json
    $j.session_id = $Session
    $j.cwd = $proj
    $j.transcript_path = Join-Path $dir ".claude\projects\$slug\$Session.jsonl"
    $j.scratchpad_dir = Join-Path $dir "AppData\Local\Temp\claude\$slug\$Session\scratchpad"
    if ($Set) { foreach ($k in $Set.Keys) { $j | Add-Member -NotePropertyName $k -NotePropertyValue $Set[$k] -Force } }
    # A tool-use payload built on the prompt fixture's shape carries no prompt.
    if ($Set -and $Set.ContainsKey('tool_name')) { $j.PSObject.Properties.Remove('prompt') }
    $f = Join-Path $Work "$Session-$Fixture.json"
    [IO.File]::WriteAllText($f, ($j | ConvertTo-Json -Depth 20), [Text.UTF8Encoding]::new($false))
    return $f
}

function Hook([string]$Name, [string]$Event, [string]$PayloadFile) {
    Invoke-Kioku (Exe $Name) @('hook', $Event, '--agent', 'claude-code') (Join-Path $Work "$Name\src\demo") $PayloadFile
}

function Project-Id { if ($script:Out -match '(?m)^project: .*\(id: ([^)]*)\)') { return $Matches[1] } return '' }

# kioku_handoff_write through the MCP server registered in ~/.claude.json (`kioku mcp`).
function Mcp-Handoff([string]$Name, [string]$ProjectId, [string]$Session) {
    $cfg = Get-Content -Raw -Encoding UTF8 (Join-Path $Work "$Name\.claude.json") | ConvertFrom-Json
    $entry = $cfg.mcpServers.kioku
    $psi = [Diagnostics.ProcessStartInfo]::new($entry.command)
    # Windows PowerShell 5.1 (.NET Framework) has no ProcessStartInfo.ArgumentList: quote by hand.
    $psi.Arguments = (@($entry.args) | ForEach-Object { '"' + ([string]$_ -replace '(\\*)"', '$1$1\"') + '"' }) -join ' '
    $psi.WorkingDirectory = Join-Path $Work "$Name\src\demo"
    $psi.UseShellExecute = $false
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.StandardOutputEncoding = [Text.UTF8Encoding]::new($false)
    $p = [Diagnostics.Process]::Start($psi)
    $call = @{
        jsonrpc = '2.0'; id = 2; method = 'tools/call'
        params = @{ name = 'kioku_handoff_write'; arguments = @{
                project = $ProjectId; session = $Session
                summary = '認証ミドルウェアのトークン検証を auth.rs に集約した（E2E-HANDOFF-MARKER）'
                next_steps = @('auth.rs の単体テストを追加する'); open_questions = @(); decisions = @('トークン検証は auth.rs に一本化する')
            } }
    } | ConvertTo-Json -Depth 10 -Compress
    $lines = @(
        '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"claude-code-e2e","version":"0"}}}',
        '{"jsonrpc":"2.0","method":"notifications/initialized"}',
        $call
    )
    $utf8 = [Text.UTF8Encoding]::new($false)
    foreach ($l in $lines) {
        $b = $utf8.GetBytes($l + "`n")
        $p.StandardInput.BaseStream.Write($b, 0, $b.Length)
    }
    $p.StandardInput.BaseStream.Flush()
    $got = ''
    $deadline = (Get-Date).AddSeconds(30)
    while ((Get-Date) -lt $deadline -and -not $got.Contains('"id":2')) {
        $t = $p.StandardOutput.ReadLineAsync()
        if (-not $t.Wait(30000)) { break }
        if ($null -eq $t.Result) { break }
        $got += $t.Result + "`n"
    }
    $p.StandardInput.Close()
    if (-not $p.WaitForExit(10000)) { $p.Kill() }
    $script:Out = $got
    $script:Code = if ($got.Contains('"id":2')) { 0 } else { 1 }
}

function Doctor([string]$Name, [string[]]$Allowed) {
    Use-Machine $Name | Out-Null
    Invoke-Kioku (Exe $Name) @('doctor') (Join-Path $Work $Name)
    $warns = @([regex]::Matches($script:Out, '(?m)^\[WARN\] ([^:]+):') | ForEach-Object { $_.Groups[1].Value })
    $fails = @([regex]::Matches($script:Out, '(?m)^\[FAIL\] ([^:]+):') | ForEach-Object { $_.Groups[1].Value })
    $unexpected = @($warns | Where-Object { $Allowed -notcontains $_ })
    $summary = (@($script:Out.Trim() -split "`r?`n") | Select-Object -Last 1)
    Check "${Name}: kioku doctor exit 0 ($summary; warnings: $($warns -join ' '); allowed: $($Allowed -join ' '))" `
        ($script:Code -eq 0 -and $fails.Count -eq 0 -and $unexpected.Count -eq 0)
}

# --------------------------------------------------------------------------- server

$srvDir = Use-Machine 'server'
try {
    $script:Out = (& ([scriptblock]::Create((Invoke-RestMethod $InstallPs1Url))) -NoSetup *>&1 | Out-String)
    $ok = $true
} catch {
    $script:Out += "`n$_"
    $ok = $false
}
Check "server: install.ps1 -NoSetup installed kioku $Version" ($ok -and (Test-Path (Exe 'server')))
function Stop-Early {
    if ($script:serve -and -not $script:serve.HasExited) { Stop-Process -Id $script:serve.Id -Force }
    Write-Host ''
    Write-Host "release ${Tag}: $script:Passed passed, $script:Failed failed (stopped early)"
    exit 1
}
$serve = $null
if (-not (Test-Path (Exe 'server'))) { Stop-Early }
Invoke-Kioku (Exe 'server') @('--version') $srvDir
Check "server: kioku.exe --version = kioku $Version" ($script:Out.Trim() -eq "kioku $Version")
Invoke-Kioku (Exe 'server') @('init') $srvDir
Check 'server: kioku init (token generated into config.toml, not printed)' ($script:Code -eq 0)

$env:KIOKU_AUTO_UPDATE = '0'
$serveLog = Join-Path $Work 'serve.out'
$serve = Start-Process -FilePath (Exe 'server') -ArgumentList 'serve' -WorkingDirectory $srvDir -PassThru -NoNewWindow `
    -RedirectStandardOutput $serveLog -RedirectStandardError "$serveLog.err"
Remove-Item Env:KIOKU_AUTO_UPDATE
$up = $false
for ($i = 0; $i -lt 60 -and -not $up; $i++) {
    try { Invoke-WebRequest -UseBasicParsing "http://127.0.0.1:$Port/api/v1/health" -TimeoutSec 2 | Out-Null; $up = $true } catch { Start-Sleep 1 }
}
$script:Out = (Get-Content -Raw $serveLog -ErrorAction SilentlyContinue) + (Get-Content -Raw "$serveLog.err" -ErrorAction SilentlyContinue)
Check 'server: kioku serve answers /api/v1/health on 127.0.0.1' $up

Invoke-Kioku (Exe 'server') @('invite', '--host', '127.0.0.1', '--uses', '2') $srvDir
$line = ''
if ($script:Out -match '(?m)^  Windows \(PowerShell\):  (.+)$') { $line = $Matches[1].Trim() }
$shape = "^\`$env:KIOKU_JOIN='127\.0\.0\.1:$Port/[A-Za-z0-9]+'; irm $([regex]::Escape($InstallPs1Url)) \| iex$"
Check "server: kioku invite printed the Windows line: $(Mask $line)" ($script:Code -eq 0 -and $line -match $shape)
if (-not $up -or -not ($line -match $shape)) { Stop-Early }

# --------------------------------------------------------------------------- clients

foreach ($c in 'win-a', 'win-b') {
    $dir = Use-Machine $c
    New-Item -ItemType Directory -Force -Path (Join-Path $dir '.claude') | Out-Null
    Check "${c}: pristine profile (no kioku), Claude Code dir ~/.claude present" (-not (Test-Path (Exe $c)) -and -not (Test-Path (Join-Path $dir '.kioku')))
    try {
        # The line exactly as `kioku invite` printed it, pasted into PowerShell.
        $script:Out = (Invoke-Expression $line *>&1 | Out-String)
        $ok = $true
    } catch {
        $script:Out += "`n$_"
        $ok = $false
    }
    $ok = $ok -and (Has "installed kioku $Version") -and (Has 'kioku is ready - restart Claude Code.') -and (Test-Path (Exe $c))
    Check "${c}: pasted the Windows invite line -> kioku $Version installed, joined, Claude Code hooks + MCP" $ok
    $settings = Get-Content -Raw -Encoding UTF8 (Join-Path $dir '.claude\settings.json') -ErrorAction SilentlyContinue
    $script:Out = [string]$settings
    $ok = $true
    foreach ($ev in 'session-start', 'user-prompt-submit', 'post-tool-use', 'stop', 'pre-compact', 'session-end') {
        if (-not (Has "`"$ev`"")) { $ok = $false }
    }
    Check "${c}: ~/.claude/settings.json runs kioku.exe hook <event> (exec form) for all 6 events" ($ok -and (Has 'kioku.exe'))
    $proj = Join-Path $dir 'src\demo'
    New-Item -ItemType Directory -Force -Path $proj | Out-Null
    $remote = if ($c -eq 'win-a') { 'https://github.com/example/kioku-e2e-demo.git' } else { 'git@github.com:example/kioku-e2e-demo.git' }
    git -C $proj init -q -b main
    git -C $proj remote add origin $remote
    Set-Content -Path (Join-Path $proj 'README.md') -Value '# demo'
    git -C $proj add README.md
    git -C $proj -c user.name=e2e -c user.email=e2e@example.invalid commit -q -m init
    Check "${c}: test repository src\demo (origin $remote)" ($LASTEXITCODE -eq 0)
}

# --- A: a Claude Code session
Use-Machine 'win-a' | Out-Null
$sa = 'e2e-win-a-1'
Hook 'win-a' 'session-start' (Payload 'win-a' 'session_start' $sa @{})
$projId = Project-Id
Check "win-a: SessionStart -> <kioku> block (project $projId, session $sa)" ($script:Code -eq 0 -and (Has '<kioku>') -and (Has "session: $sa") -and $projId)
Hook 'win-a' 'user-prompt-submit' (Payload 'win-a' 'user_prompt_submit' $sa @{ prompt = '引き継ぎ書の自動生成を実装して。テスト用の api_key=sk-live_abcdefghijklmnop1234 は使っていい' })
Check 'win-a: UserPromptSubmit (Japanese prompt) exit 0' ($script:Code -eq 0)
$edit = @{
    hook_event_name = 'PostToolUse'; tool_name = 'Edit'; tool_use_id = 'toolu_e2e_edit'
    tool_input = @{ file_path = (Join-Path $Work 'win-a\src\demo\src\auth.rs'); old_string = 'fn a()'; new_string = "/// トークン検証`nfn a()"; replace_all = $false }
    tool_response = @{ filePath = (Join-Path $Work 'win-a\src\demo\src\auth.rs'); userModified = $false }
}
Hook 'win-a' 'post-tool-use' (Payload 'win-a' 'user_prompt_submit' $sa $edit)
Check 'win-a: PostToolUse (Edit) exit 0' ($script:Code -eq 0)
Mcp-Handoff 'win-a' $projId $sa
Check 'win-a: kioku_handoff_write through the registered `kioku mcp` bridge (Japanese summary)' ($script:Code -eq 0 -and (Has "handoff recorded for $projId") -and -not (Has '"isError":true'))
Hook 'win-a' 'stop' (Payload 'win-a' 'stop' $sa @{})
Check 'win-a: Stop exit 0' ($script:Code -eq 0)

# --- B: the next session, on the other machine
Use-Machine 'win-b' | Out-Null
$sb = 'e2e-win-b-1'
Hook 'win-b' 'session-start' (Payload 'win-b' 'session_start' $sb @{})
Check "win-b: SessionStart shows win-a's handoff (summary, next step, same project via SSH remote, @win-a)" (
    $script:Code -eq 0 -and (Has "(id: $projId)") -and (Has '## 前回からの引き継ぎ') -and
    (Has '認証ミドルウェアのトークン検証を auth.rs に集約した（E2E-HANDOFF-MARKER）') -and (Has 'auth.rs の単体テストを追加する') -and (Has '@win-a'))
Invoke-Kioku (Exe 'win-b') @('search', '引き継ぎ書', '--project', $projId) (Join-Path $Work 'win-b')
Check "win-b: Japanese kioku search (引き継ぎ書) finds win-a's session; the fake secret is redacted" (
    $script:Code -eq 0 -and (Has '引き継ぎ書') -and (Has '@win-a') -and -not (Has 'sk-live_abcdefghijklmnop1234'))
Hook 'win-b' 'user-prompt-submit' (Payload 'win-b' 'user_prompt_submit' $sb @{ prompt = '期限切れトークンのテストを追加して（E2E-RULES-MARKER）' })
Check 'win-b: UserPromptSubmit (Japanese) exit 0' ($script:Code -eq 0)
Hook 'win-b' 'stop' (Payload 'win-b' 'stop' $sb @{ last_assistant_message = '期限切れトークンのテストを追加しました。' })
Check 'win-b: Stop exit 0 (rules handoff)' ($script:Code -eq 0)

Use-Machine 'win-a' | Out-Null
Hook 'win-a' 'session-start' (Payload 'win-a' 'session_start' 'e2e-win-a-2' @{})
Check "win-a: next SessionStart shows win-b's automatic handoff (@win-b)" ($script:Code -eq 0 -and (Has '## 前回からの引き継ぎ') -and (Has 'E2E-RULES-MARKER') -and (Has '@win-b'))

Doctor 'win-a' $AllowedWarnClient
Doctor 'win-b' $AllowedWarnClient

if ($serve -and -not $serve.HasExited) { Stop-Process -Id $serve.Id -Force }
Write-Host ''
Write-Host "release ${Tag}: $script:Passed passed, $script:Failed failed"
if ($script:Failed -gt 0) { exit 1 }
