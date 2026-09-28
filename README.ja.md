# kioku (記憶)

日本語 | [English](README.md)

**ステータス: 開発中（v0.x、M2.1）。** 作者のマシンでは日常的に動いていますが、粗い部分があり、
マイナーバージョンの間でも設定の形式や API が変わることがあります。

kioku は、あなたのすべてのマシンで動くすべての AI コーディングエージェントが共有する、
セルフホスト型の記憶サーバーです。Rust 製のシングルバイナリで動きます。記憶する内容は
すべて git リポジトリ内のプレーンな Markdown（これが唯一の正本）で、SQLite がメタデータを、
tantivy のインデックスが検索を受け持ち、日本語は lindera（IPADIC）で正しく分かち書きされます。
主な対象は日本語ですが、英語も使えます。エージェントとは MCP（streamable HTTP）とライフサイクル
フックでつながり、セッションは自動で記録され、次のセッションは（別のエージェントでも別のマシンでも）
前回の引き継ぎを受け取った状態で始まります。既定では LLM を一切呼びません。要約と引き継ぎは
ルールベースで生成します。

## 解決する問題

- エージェントはそれぞれ自分だけの記憶を持つ（あるいは持たない）。デスクトップの Claude Code が
  覚えたことを、ノート PC のセッションも、次に試すツールも知らない。
- 作業を続けるには、セッションの終わりに毎回手で引き継ぎ書を書き、次のセッションに貼り付ける必要がある。
- 多くの記憶ツールは日本語を分かち書きできないトークナイザで索引を作るため、日本語のメモは
  事実上検索できない。

kioku は自分で管理するサーバーに記憶を一つだけ持ち、フックでセッションを記録し、同じプロジェクトの
次のセッションへ引き継ぎを自動で渡します。

## 仕組み

```
 各マシン                                      kioku サーバー（1 人に 1 台）
+----------------------------------+          +---------------------------------------+
| Claude Code                      |          | kioku serve  (:7391)                  |
|  hooks --> kioku hook <event> ---+-- HTTP ->|  /api/v1  --+                         |
|  MCP client (kioku_* tools) -----+-- HTTP ->|  /mcp     --+--> kioku-core           |
+----------------------------------+  Bearer  |                 |- wiki/   Markdown + git|
                                       token  |                 |          (source of truth)
                                              |                 |- db/     SQLite metadata|
                                              |                 '- index/  tantivy + lindera|
                                              +---------------------------------------+
```

セッションのライフサイクル（Claude Code）:

```
SessionStart      kioku hook session-start --> POST /api/v1/sessions/start
                  stdout（エージェントのコンテキストに追加される）:
                    <kioku> project id、session id、未受領の引き継ぎ、STATE.md 抜粋 </kioku>
UserPromptSubmit  \
PostToolUse        > サニタイズ済みの観測 --> POST /api/v1/observations
PreCompact        /
Stop              最後の引き継ぎ以降（無ければ開始以降）のツール実行 3 回以上？
                    はい   -> exit 2 + 催促:「kioku_handoff_write で引き継ぎを書くこと」
                    いいえ -> finalize: セッションページ + STATE.md（+ ルール生成の引き継ぎ）
SessionEnd        finalize（冪等）
```

- **SessionStart での注入**: フックが `<kioku>` ブロックを出力します。中身はプロジェクト id と
  セッション id（どちらも `kioku_handoff_write` に渡す）、未受領の引き継ぎ（あれば）、
  プロジェクトの `STATE.md` の抜粋です。
- **Stop 時の催促**: このセッションで最後に `kioku_handoff_write` を呼んでから（一度も呼んでいなければ
  セッション開始から）ツールを 3 回以上使っていれば、Stop フックは終了コード 2 で終わり、要約・次にやること・
  未解決の質問・決定事項を記録するよう求めます。`stop_hook_active` によりループはしません。
  `[client] stop_nudge = false` または `KIOKU_STOP_NUDGE=0` で無効にできます。
- **finalize** はセッションページを書き、`STATE.md` を書き直し、エージェントが引き継ぎを書かなかった
  場合はルールで生成します（最後の指示、触ったファイル、コマンド、コミット、エラー件数）。
  エージェントの引き継ぎの後にツールを 3 回以上使っていた場合は、その後の作業分をルールで生成した追記
  （「引き継ぎ（自動生成・追記）」）を付けて両方を引き継ぎます。ページの書き込みはすべて git コミットになります。
- **引き継ぎは一度きり**: 同じプロジェクトの次の SessionStart が最新の未受領の引き継ぎを受け取ります
  （それより古い未受領のものは置き換え済みとして受領扱いになります）。`kioku_handoff_pending` を
  `accept=false` で呼ぶと、消費せずに覗くだけです。

## インストール

1 行で、sudo 不要です（macOS と Linux、x86_64 と arm64）:

```sh
curl -fsSL https://raw.githubusercontent.com/misorafa/kioku/main/install.sh | sh
```

`install.sh` はこのマシン用のリリースバイナリをダウンロードし（Linux ではまず静的リンクの musl 版、
次に glibc 版）、リリースの `SHA256SUMS` で検証し（検証できないものは決してインストールしません）、
`~/.local/bin/kioku` にインストールして（アトミックな rename）、`kioku setup`（次の節）を実行します。
`~/.local/bin` が `PATH` に無ければ、使っているシェル（zsh / bash / fish）用に追加する行を表示します。
シェルの設定ファイルを書き換えることはありません。オプション:

| オプション | 環境変数 | 既定値 | |
|------------|----------|--------|-|
| `--version <tag>` | `KIOKU_VERSION` | `latest` | インストールするリリース |
| `--install-dir <dir>` | `KIOKU_INSTALL_DIR` | `~/.local/bin` | インストール先 |
| `--repo <owner/name>` | `KIOKU_REPO` | `misorafa/kioku` | GitHub リポジトリ |
| `--from-source` | | | ダウンロードせず cargo でビルドする |
| `--no-setup` | | | バイナリのインストールだけ行う |

それ以外の引数（と `--` 以降のすべて）は `kioku setup` に渡されます:
`curl -fsSL …/install.sh | sh -s -- --version v0.2.0 --no-setup`。kioku はユーザー単位のインストールなので、
`--install-dir` を指定しない限り root では実行を拒否します。このプラットフォーム用のビルド済みバイナリが
無い場合はソースからビルドします。これには Rust 1.91 以上（古い場合は `rustup update`）と `git` が必要で、
初回のビルドには数分かかります（IPADIC 辞書をダウンロードするため）。GitHub に到達できない場合
（ネットワークや HTTP のエラー）はソースビルドに切り替えず、エラーで終了します。手動なら
`cargo install --locked --git https://github.com/misorafa/kioku kioku-cli`、チェックアウト内なら
`cargo install --locked --path crates/kioku-cli` です。実行時の `git` は任意です（無い場合 wiki は
バージョン管理されません）。

**ほかのマシン**（ノート PC、デスクトップ）は 1 台のサーバーに接続します。サーバーで
`kioku setup --print-client-command` を実行すると、トークン入りの正確なコマンドが表示されます:

```sh
curl -fsSL https://raw.githubusercontent.com/misorafa/kioku/main/install.sh | sh -s -- --client-only http://<server>:7391 <token>
```

あとからの更新は `kioku update`（同じダウンロードとチェックサム検証。バイナリをその場で置き換え、
サービスを再起動します。動いているものより新しいリリースだけをインストールし、`--version <tag>` なら
古いものも含めて任意のタグを入れられます。`kioku update --check` は新しいリリースがあれば終了コード 10
で終わります）、または 1 行のコマンドをもう一度実行します（古いバージョンのまま動いているサービスは、
次の `kioku setup` が再起動します）。

macOS では、インストール済みのバイナリに `cp` で上書きしないでください。カーネルが古いコード署名を
キャッシュしているため、新しいバイナリが SIGKILL で落ちます（"zsh: killed"）。先に消してからコピーし
（`rm ~/.cargo/bin/kioku && cp target/release/kioku ~/.cargo/bin/`）、`kioku service stop && kioku service start`
で再起動します。`kioku update` と `cargo install` は最初からこの方法で置き換えます。

### Windows（クライアント専用）

Windows 11（x64）では、kioku は WSL を使わずネイティブの**クライアント**として動きます。対象は
Claude Code（CLI と Claude デスクトップアプリの Code タブ）と Codex デスクトップアプリ / CLI のフック、
それに `kioku mcp` ブリッジです。サーバーは Mac か Linux のマシンに置いたままにします。サーバーで
`kioku setup --print-client-command` を実行すると、`install.sh` の行に続いて PowerShell 用の行が
表示されます。PowerShell（5.1 または 7、管理者権限は不要）で実行します:

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/misorafa/kioku/main/install.ps1))) -ClientOnly http://<server>:7391 <token>
```

`install.ps1` は `kioku-<tag>-x86_64-pc-windows-msvc.tar.gz` をダウンロードして `SHA256SUMS` で
検証し（`Get-FileHash`）、`kioku.exe` を `%LOCALAPPDATA%\Programs\kioku` にインストールして
`kioku setup --client-only <url> <token>` を実行します。オプションは `-Version <tag>`、
`-InstallDir <dir>`、`-Repo <owner/name>`（環境変数は `install.sh` と同じ）、`-NoSetup`、`-AddToPath`
です。`-AddToPath` が無ければ、ユーザーの `PATH` にディレクトリを追加するコマンドを表示するだけです
（フックは絶対パスを使うので、どちらでも動きます）。それ以外の引数は `kioku setup` に渡されます。
Windows では `--client-only` なしの `kioku setup` は実行を拒否し、`kioku service` も使えません。

- **フック:** Claude Code には exec 形式（`"command": "C:\\…\\kioku.exe", "args": ["hook", "stop"]`）
  を登録するので、シェルを経由しません。Codex には `command` と PowerShell 用の `commandWindows`
  （`& "C:\…\kioku.exe" hook stop --agent codex`）、Cursor（ベストエフォート）には引用符付きの
  コマンド文字列を登録します。Gemini CLI と Antigravity は Windows では設定しません。フックの入力は
  UTF-8 のバイト列として読む（BOM は無視）ので、日本語ロケールの Windows でも日本語のプロンプトが
  化けません。
- **更新:** `kioku update` はほかの OS と同じように使えます。実行中の `kioku.exe` は上書きできないため
  `kioku.exe.old` に名前を変え、次の実行時に削除します。
- **SmartScreen:** Windows 版のバイナリはまだコード署名していません。ダウンロードした `kioku.exe` の
  初回実行時に Windows が警告を出すことがあります。
- **アクセス権:** `%USERPROFILE%\.kioku` 以下のファイルはユーザープロファイルの ACL（本人と管理者のみ）
  を引き継ぎます。unix の 0600 / 0700 は適用されません。
- **WSL と Orca:** WSL の中で動くエージェント（Codex の「Agent environment = WSL」、Orca の WSL
  ターミナル）は Linux のプロセスです。WSL の中に Linux 版の kioku を `install.sh --client-only …` で
  入れてください。Orca は WSL 上の Codex に専用のホームを与えるため、`~/.codex/hooks.json` が
  見えないことがあります（既知の制限）。ネイティブの Windows エージェントが
  `\\wsl.localhost\<distro>\…` のリポジトリで作業した場合も git のリモートから同じプロジェクト ID に
  なるはずですが、未検証です。

## `kioku setup`

```
kioku setup [--client-only <url> <token>] [--no-service] [--no-agents] [--agents a,b]
            [--bind <addr>] [--no-instructions] [--dry-run] [--print-client-command]
```

冪等で非対話の 1 ステップです（`curl … | sh` の下でも安全。何度でも再実行できます）:

1. **config** — 新しいトークン入りの `~/.kioku/config.toml` を作る（`kioku init` と同じ）か、既存のものを
   そのまま使います（トークンは決して置き換えません）。ほかのマシンから接続されるサーバーでは
   `--bind 0.0.0.0`。`--client-only` では、何かを書く*前に* URL とトークンをサーバーで確認し、
   `[client]` セクションだけを書きます。
2. **service** — バックグラウンドサービスをインストールし（`kioku service install`、後述）、サーバーが
   応答するまで待ちます。そのポートですでに kioku サーバーが応答している場合（Docker など）は
   サービスをインストールしません。インストール済みのサービスがこのバイナリと違うバージョンで応答した
   場合（バイナリを置き換えた直後など）は再起動します: `restarted (v<old> -> v<new>)`。
   `--no-service` で省略します。
3. **auth** — 認証付きのリクエストでトークンを確認します。
4. **agents** — 検出したすべてのエージェント（`~/.claude`、`~/.codex` または `$CODEX_HOME`、
   `~/.cursor`、`~/.gemini`）にフック + MCP をインストールします。`--agents` で対象を絞り、
   `--no-agents` で省略し、`--no-instructions` で指示スニペットを省略します。
5. **summary** — ステップごとに 1 行（`ok`、`--` 省略、`!!` 警告、`xx` 失敗）。失敗したステップが
   あれば終了コード 1 です。

`--dry-run` は計画を表示するだけで何も書きません。実行後は、起動中のエージェントを再起動して
新しいフックと MCP サーバーを読み込ませてください。

## `kioku doctor`

```sh
kioku doctor                  # チェックごとに [ OK ] / [WARN] / [FAIL] と `fix:` のヒント
kioku doctor --agent codex    # 1 つのエージェントだけ（検出されていなくても）
kioku doctor --json           # {"checks":[{id, status, message, fix?}]}
```

バイナリと `PATH`、`config.toml`（とそのモード 0600）、データディレクトリ、`git`、サーバー（到達できるか、
バージョンが同じか）、トークン、インデックスのバージョン、MCP、サービス、そして検出した各エージェントに
ついて、フックがあり既存のバイナリを指しているか、MCP エントリ（stdio の中継なら実行ファイル、URL 形式なら URL とトークン。トークンは比較する
だけで表示しません）、エージェント固有のスイッチ、指示スニペットを確認します。`hook.log` の最近のフックエラーと、
ペイロードダンプが有効になっていることも指摘します。FAIL が 1 つでもあれば終了コード 1 です。

## `kioku service`

```sh
kioku service install      # 定義を書き、有効化して起動（冪等）
kioku service status       # インストール済み? 動作中? pid、サーバーの health
kioku service start|stop    # start は動作中の launchd ジョブを再起動（kickstart -k）
kioku service logs [-f] [-n 200]   # ~/.kioku/logs/serve.log の末尾
kioku service uninstall
```

`kioku serve --log-file ~/.kioku/logs/serve.log` を動かすユーザー単位のサービスで、sudo は不要です。
macOS では LaunchAgent `~/Library/LaunchAgents/dev.kioku.serve.plist`（launchd）、Linux では
`systemd --user` のユニット `~/.config/systemd/user/kioku.service` です。Linux ではログアウト後も
サーバーが動き続けるよう lingering を有効にします（`loginctl enable-linger`）。許可されない場合は
自分で実行するコマンドを表示します。launchd も動作する `systemctl --user` も無い環境（systemd の無い
WSL、コンテナ）では、`kioku serve` を自分で動かす方法（または Docker）を表示し、`setup` は警告付きで
続行します。クライアント専用のマシンにサービスはありません。
サービスはクラッシュ後には再起動しますが、正常終了後には再起動せず、クラッシュの繰り返しは抑制されます
（launchd: 起動間隔 10 秒、systemd: 60 秒に 5 回まで）。`serve.log` は 10 MiB でローテートします
（`.1`〜`.3` を保持）。

## エージェント

`kioku setup` は検出したすべてのエージェントにインストールします。`kioku install <agent>` は 1 つだけ
（`claude-code`、`codex`、`cursor`、`gemini-cli`、`antigravity`、または `all`）、`kioku uninstall <agent>` は kioku が
追加したものだけを正確に取り除きます。共通事項:

- フックは `<kioku の絶対パス> hook <event> --agent <agent>` を実行するので、`PATH` に関係なく動きます
  （kioku は `target/` ではなく `~/.local/bin` のような動かない場所に置いてください）。フックは
  fail-open です（「セキュリティ」参照）;
- エントリはマージされます。同じファイル内のほかのフックやサーバーはそのまま残り、2 回目の実行では
  何も変わらず、既存のファイルは kioku が最初に変更する前に 1 回だけ `<file>.kioku-bak` にバックアップ
  されます。正しい JSON でないファイルは触らずに、追加すべき設定断片を表示します;
- シンボリックリンクの設定ファイル（dotfiles リポジトリなど）はリンク越しに編集します。リンクは残り、
  リンク先が更新されます（バックアップはリンク先の隣）。ほかのユーザーが読めるファイルに kioku が
  トークンを追加するときは、モードを 0600 にしてその旨を表示します;
- MCP サーバーは **`kioku mcp`（stdio の中継）** として登録します（`{"command": "<kioku>", "args": ["mcp"]}`）。
  エージェントが kioku を起動し、kioku が `~/.kioku/config.toml` のサーバーに各ツールの呼び出しを中継します。
  エージェントの設定ファイルにはサーバーの URL もトークンも入らず、フックと同じ粘り強い接続（覚えたアドレス、
  IPv6/IPv4）が使われ、サーバーを変えるときも `kioku setup --client-only …` だけで済みます。`--mcp-http` を
  付けると、従来の URL + トークンの形で登録します;
- `--project` はフック（と指示）を現在のリポジトリに書きます。MCP のエントリは常に
  ユーザー設定に置かれ、リポジトリには決して入りません。プロジェクトのフックファイルにはマシン固有の
  パスが入るので、コミットしないでください。プロジェクトのディレクトリがホームディレクトリそのもの
  の場合、`--project` は実行を拒否します;
- `--dry-run` は変更内容を表示します。

### Claude Code

| 内容 | 場所 |
|------|------|
| フック（SessionStart、UserPromptSubmit、PostToolUse、PreCompact、Stop、SessionEnd） | `~/.claude/settings.json`（`--project`: `./.claude/settings.json`） |
| MCP | `~/.claude.json` の `mcpServers.kioku`: `{"type": "stdio", "command": "<kioku>", "args": ["mcp"]}` |
| 指示 | 既定では無し（フックがコンテキストを注入します）。`--instructions` で `~/.claude/CLAUDE.md` にブロックを追加 |

kioku は `claude mcp add` を実行しないので、トークンがコマンドラインに現れることはありません。

### Codex CLI

| 内容 | 場所 |
|------|------|
| フック | `~/.codex/hooks.json`（`$CODEX_HOME`。`--project`: `<repo>/.codex/hooks.json`） |
| MCP | `~/.codex/config.toml` 内の管理ブロック `[mcp_servers.kioku]`（`command` + `args = ["mcp"]`）。ブロック外のバイトは決して変更せず、Codex があとからブロック内に追加したテーブル（プロジェクトやフックの信頼）はブロックの外へ移し、決して消しません |
| 指示 | `~/.codex/AGENTS.md` の区切られた kioku ブロック（`--project`: `<repo>/AGENTS.md`） |

**信頼の手順:** Codex は、新しいフックや変更されたフックを、信頼されるまで実行しません。Codex を一度
開いて `/hooks` を実行し、kioku のフックを信頼してください（setup と doctor が知らせます）。kioku が
信頼の状態を代わりに書くことはありません。`kioku update` の前後でパスは変わらないので、再び必要になるのは
バイナリを移動したときだけです。それまでの間は AGENTS.md のブロックが Codex に kioku の MCP ツールを
使うよう伝えます。現在の Codex ではフックは既定で有効です。まだ機能フラグが必要な古いビルドでは、
`kioku install codex --enable-hooks-feature` が `[features]` に `hooks = true` を追加します。

### Cursor

| 内容 | 場所 |
|------|------|
| フック | `~/.cursor/hooks.json`（`--project`: `<repo>/.cursor/hooks.json`）— エディタと `agent` CLI の両方が使います |
| MCP | `~/.cursor/mcp.json` の `mcpServers.kioku` |
| 指示 | `--project` のときだけ: `<repo>/.cursor/rules/kioku.mdc`（Cursor にはユーザー単位のルールファイルがありません） |

**フックの重複:** Cursor は `~/.claude/settings.json` にある Claude Code のフックも実行します
（「Include Third-Party Plugins, Skills, and Other Configs」、既定で有効）。kioku のネイティブな Cursor
フックがインストールされていれば、取り込まれた Claude Code 側の呼び出しは Cursor 内で動いていることを
認識して何もしないので、二重に記録されることはありません。ネイティブのフックが無い場合は Cursor の
イベントとして処理されます。両方をインストールし（setup はそうします）、`kioku doctor` で確認して
ください（`agent.cursor.duplicate`）。Cursor の sessionStart のコンテキストは常に届くとは限らないため、
kioku は各セッションの最初のツール使用時（`postToolUse`。ファイル編集と失敗したツールでは渡せません）
にもコンテキストを追加します（`[client] cursor_late_context = false` で無効）。

### Gemini CLI

Gemini CLI は 2026-06-18 に個人アカウント向けの提供を終了しました（Code Assist Standard/Enterprise と
有料 API キーでは引き続き使えます）。後継は下の Antigravity CLI です。Gemini CLI の検出には `~/.gemini`
ではなく `~/.gemini/tmp` を使います（`~/.gemini` は Antigravity も作るため）。

| 内容 | 場所 |
|------|------|
| フック（名前は `kioku-*`） | `~/.gemini/settings.json` の `hooks`（`--project`: `<repo>/.gemini/settings.json`） |
| MCP | `~/.gemini/settings.json` の `mcpServers.kioku`（`--trust-mcp` で `"trust": true` を追加） |
| 指示 | `~/.gemini/GEMINI.md` の区切られた kioku ブロック（`--project`: `<repo>/GEMINI.md`） |

フックが無効になっていないこと（`hooksConfig.enabled: false`、または `hooksConfig.disabled` に
`kioku-*` の名前がある）が必要です。doctor が両方を確認します。プロジェクトのフックは、初回の実行前に
Gemini の警告が一度表示されます。`context.fileName` で Gemini が `AGENTS.md`（Codex と共有）を読む
場合、片方をアンインストールしても、もう片方がインストールされている間は kioku のブロックを残します。

### Antigravity CLI

`agy` です（`~/.gemini/antigravity-cli` か `~/.local/bin/agy` があれば検出。デスクトップアプリだけでは
検出しません）。詳細と出典は `docs/SPEC-M2.1.md` にあります。

| 内容 | 場所 |
|------|------|
| フック（名前付きグループ `kioku`） | `~/.gemini/config/hooks.json`（`--project`: `<repo>/.agents/hooks.json`） |
| MCP | `~/.gemini/config/mcp_config.json` の `mcpServers.kioku`（`serverUrl`） |
| 指示 | `~/.gemini/GEMINI.md` の区切られた kioku ブロック（`--project`: `<repo>/AGENTS.md`） |

agy が読み込むのは SessionStart・PreInvocation・PostInvocation・Stop だけで、ペイロードにはプロンプトも
cwd もありません。そのため kioku は、新しいプロンプトを会話の transcript から読み取り、会話の最初の
モデル呼び出しで引き継ぎを渡し（`injectSteps`）、同じターンの 2 回目以降のモデル呼び出しを Stop の催促用に
ツール実行として数え、プロジェクトは `workspacePaths` から決めます。`--add-dir` なしの `agy -p` は
ワークスペースを送らないため記録されません。`hooks.json` にあるほかのツールのグループはそのまま残ります。
agy が実際に読み込んだフックは `agy -p "/hooks" --output-format json` で確認できます。

## 試してみる

1. git リポジトリでこれらのエージェントのどれかを開き、タスクを与えます。そのディレクトリに kioku が使う
   プロジェクト id は `kioku project id` で確認できます。
2. （最後の引き継ぎ以降に）ツールを何回か使ったターンが終わると、Stop の催促が `kioku_handoff_write` を
   呼ぶよう求めます。
3. 同じリポジトリで新しいセッションを始めると（または `/clear`）— 同じエージェントでも別のエージェントでも、
   このマシンでも別のマシンでも — SessionStart で引き継ぎが注入されます。
4. ターミナルから検索します:

```sh
kioku search 引き継ぎ
kioku search --project <id> --limit 5 設計 判断
kioku status
```

## 自宅サーバーで使う

すべてのマシンで 1 台のサーバーを共有します。新しいサーバーでは
`curl -fsSL …/install.sh | sh -s -- --bind 0.0.0.0` が新しい `config.toml` に `[server] bind =
"0.0.0.0"` を書きます（既存の設定はそのまま残るので、そこで `bind` を編集してから
`kioku service stop && kioku service start`）。続いて `kioku setup --print-client-command` を実行し、
表示されたコマンドをほかのすべてのマシンで実行します。

**接続先の選び方。** `--print-client-command` はサーバーの LAN の IP を表示します。
- IP は、LAN を経由させる VPN（WireGuard など）の先からも使えます。ただしサーバーの IP が変わると、つながらなくなります。
- `<ホスト名>.local` は、IP が変わっても使えます。ただし mDNS なので、同じ LAN の中でしか名前が引けません。
- kioku のサーバーは `bind = "0.0.0.0"` のとき IPv4 と IPv6 の両方で待ち受けます。そのため、名前が IPv6 に解決されても届きます。
- フックと CLI は、名前で接続して成功したアドレスを `~/.kioku/state/server-addrs.json` に覚えます。次からはそのアドレスを先に試すので、VPN で外にいて名前が引けないときもつながります。
- エージェントの MCP は `kioku mcp` の中継を通るので、フックと同じく覚えたアドレスが使われます（`--mcp-http` で登録したエージェントは設定の URL に直接つなぐので、IP か DNS の名前を使ってください）。

**macOS のサーバーでは、ファイアウォールの許可を確認してください。** Little Snitch や LuLu などが入っていると、
新しい kioku への LAN からの接続が、許可の画面（サーバー機の画面にだけ出ます）で止まります。症状は
「接続はできるのに応答が返らない」です。`kioku doctor` の `server.lan` が、LAN のアドレスから届くかを
確認します。許可したら `kioku service stop && kioku service start` を実行してください。

フックは、このマシン上またはプライベートネットワーク上のサーバーへのリクエストを、環境変数の
`HTTP(S)_PROXY` 経由では送りません（ループバック、10/8、172.16/12、192.168/16、fc00::/7、fe80::/10、
`*.local` / `*.lan` / `*.internal` の名前）。それ以外のホスト（例: `kioku.tailnet.ts.net` のような
VPN の名前）をプロキシに通したくない場合は `NO_PROXY` に追加してください。

**TLS とトークンなしで kioku をインターネットに公開しないでください。** kioku は平文の HTTP を話し、
1 つの Bearer トークンで認証します。次のいずれかの背後に置いてください:

- **Caddy**（HTTPS 自動化）を同じホストで動かし、kioku は `127.0.0.1` にバインドする:

  ```
  kioku.example.com {
      reverse_proxy 127.0.0.1:7391
  }
  ```

- **Cloudflare Tunnel**（`cloudflared`）で `http://127.0.0.1:7391` を指す — ポートを開ける必要がありません。
- **WireGuard**（または Tailscale などの VPN）: kioku を VPN のアドレスにバインドし、URL には
  `http://<vpn-ip>:7391` を使う。暗号化はトンネルが担います。

## Docker

```sh
docker build -t kioku .
docker run -d --name kioku -p 7391:7391 \
  -e KIOKU_AUTH_TOKEN="$(openssl rand -hex 32)" \
  -v kioku-data:/data kioku
```

イメージは非 root ユーザー（uid 10001）で `kioku serve` を実行し、`KIOKU_DATA_DIR=/data`、
`KIOKU_BIND=0.0.0.0` が設定されています。`config.toml` は不要です。`KIOKU_AUTH_TOKEN` が設定されていれば、
初回起動時にデータディレクトリが作られます。トークンは控えておいてください（クライアントの
`install.sh … --client-only <url> <token>`（または `kioku setup --client-only`）に必要です）。
Docker ホスト自身で `kioku setup` を実行すると、動いているサーバーを検出してサービスはインストールしません。ホストのディレクトリをバインドマウントする場合は、
uid 10001 が書き込めるようにしてください。追加の引数は `kioku serve` に渡されます（例: `--port 8000`）。
コンテナ内では `docker exec kioku kioku status` が使えます。

Compose の場合（`docker-compose.yml` 参照）は、同じ場所の `.env` ファイルに `KIOKU_AUTH_TOKEN=…` を書き、
`docker compose up -d` を実行します。TLS についての注意は同じです。コンテナは平文の HTTP を話します。

## MCP ツール

| ツール | 入力 | 内容 |
|--------|------|------|
| `kioku_query` | `query`、`project?`、`scope?`（`project`/`global`/`all`）、`limit?`（既定 8） | 全文検索（日本語・英語）。`project` を渡すとそのプロジェクトとグローバルのページに絞る |
| `kioku_read` | `path` | wiki 内の相対パス（検索結果に表示されるもの）でページを読む |
| `kioku_write_page` | `title`、`content`、`project?`、`scope?`（`project`/`global`）、`tags?`、`path?` | 検索可能な Markdown ページを保存する。同じ title/path なら置き換える |
| `kioku_handoff_write` | `project`、`session?`（SessionStart のブロックにある id）、`summary`、`next_steps`、`open_questions`、`decisions` | そのプロジェクトの次のセッションが受け取る引き継ぎを記録する |
| `kioku_handoff_pending` | `project`、`accept?`（既定 false） | 未受領の引き継ぎを覗く（または受領する） |
| `kioku_status` | — | 件数、データディレクトリ、登録済みプロジェクト id |

サーバーが MCP の `instructions` で、探索の前に検索し、終了の前に引き継ぎを書くようエージェントに伝えます。

## データの配置

```
~/.kioku/                       # $KIOKU_DATA_DIR
  config.toml
  wiki/                         # git リポジトリ — 唯一の正本
    _global/<slug>.md           # プロジェクト横断のページ（scope = global）
    <project_id>/
      STATE.md                  # 現在の状態。finalize のたびに書き直される
      sessions/YYYY-MM-DD-<session>.md
      pages/<slug>.md           # kioku_write_page で書いたページ
  raw/<project_id>/<session_id>.jsonl   # 追記のみ・サニタイズ済みの観測
  db/kioku.sqlite               # メタデータ、セッション、観測、引き継ぎ
  index/tantivy/                # 派生データ。`kioku reindex` で wiki/ から再構築
  index/schema-version          # 索引の形式。古ければ `kioku reindex` を実行
  logs/hook.log                 # クライアント側フックの失敗ログ
```

ページは YAML frontmatter 付きの Markdown なので、どのエディタでも読み書きできます（手で編集したら
`kioku reindex` を実行すると検索に反映されます）。kioku はコミットはしますが push はしません。
バックアップ（たとえば `wiki/` をプライベートなリモートに push し、引き継ぎが入っている `db/` を
コピーする）はご自身で行ってください。

ページのファイル名はタイトルの ASCII slug です。slug 化で何かが落ちる場合（非 ASCII、記号、連続した区切り —
`C++ tips` と `C tips` など）は、タイトルの 6 桁のハッシュを付けるので、別のタイトルが同じファイルになることは
ありません。検索は NFKC で正規化するので、全角の `Ｆｌｕｔｔｅｒ` や半角の `ｱﾌﾟﾘ` も `flutter` / `アプリ` で
見つかります。**kioku を更新した後、索引が古いバージョンで作られたという警告がサーバーのログに出たら
`kioku reindex` を実行してください。**

## 設定

`$KIOKU_DATA_DIR/config.toml`（既定は `~/.kioku/config.toml`）:

```toml
[server]
bind = "127.0.0.1"      # 自宅サーバーでは 0.0.0.0
port = 7391
auth_token = "…"        # `kioku init` が生成。無いと serve は起動を拒否する
# data_dir = "~/.kioku" # データの置き場所を変える場合（任意）
summary_lang = "ja"     # ja | en — セッションページ、STATE.md、生成される引き継ぎ

[client]                # `kioku hook`、`search`、`status`、`reindex`、`install` が使う
server_url = "http://127.0.0.1:7391"
auth_token = "…"
timeout_ms = 3000       # フック 1 回あたりの上限時間
stop_nudge = true
lang = "ja"             # ja | en — SessionStart のブロックと Stop の催促
```

環境変数（ファイルより優先）:

| 変数 | 効果 |
|------|------|
| `KIOKU_DATA_DIR` | データディレクトリ。`config.toml` もここから読む |
| `KIOKU_BIND` | `[server] bind` |
| `KIOKU_PORT` | `[server] port` |
| `KIOKU_AUTH_TOKEN` | `[server]` と `[client]` 両方の `auth_token` |
| `KIOKU_SERVER_URL` | `[client] server_url` |
| `KIOKU_STOP_NUDGE` | `0` / `false` / `off` / `no` で Stop の催促を無効化 |
| `RUST_LOG` | サーバーのログフィルタ（既定 `info,tantivy=warn`） |

`kioku serve --bind <addr> --port <port>` はそのどちらよりも優先されます。

## プロジェクトの識別

プロジェクト id は作業ディレクトリから決まります（`kioku project id [path]` で表示）:

1. そのディレクトリか祖先にある `.kioku.toml` が最優先:

   ```toml
   project = "my-project"   # id
   name = "マイプロジェクト"   # 表示名（任意）
   ```

2. `origin` リモートのある git リポジトリ: `<repo>-<sha256(正規化したリモート) の先頭 8 桁>`。
   例: フォルダ `proj`、リモート `git@github.com:me/chord-life.git` → id `chord-life-…`、
   名前 `chord-life`。リモートは正規化される（スキーム、認証情報、ポート、`.git` を除去し、
   ホスト名を小文字化）ので、あるマシンの SSH クローンと、別のマシンの別名フォルダにある
   HTTPS クローンが同じ記憶を共有します。
3. それ以外（リモートの無い git、または git 以外）: `<フォルダ名>-<sha256(正規化パス) の先頭 8 桁>` —
   そのマシンのそのパスに結びつきます。

slug 部分では非 ASCII 文字が落とされます（日本語だけの名前は `proj` になります）。
プロジェクトをまとめたり名前を変えたりするには `.kioku.toml` を使ってください。

## セキュリティ

- **認証**: トークン 1 つ、ユーザー 1 人。トークンが無いと、バインドアドレスに関係なく
  `kioku serve` は起動しません。`GET /api/v1/health` 以外のすべてのルートで
  `Authorization: Bearer <token>` が必要です。`/mcp` も同様で、Host ヘッダの許可リストは無効に
  してあるため、トークンが唯一の防御です。既定のバインドは `127.0.0.1` です。
- **トークンの作り直し**: サーバーのマシンで `kioku rotate-token` を実行すると、新しいトークンを書き込み、
  サービスを再起動します（以後、古いトークンは拒否されます）。ほかのマシンで実行する
  `kioku setup --client-only <url> <新しいトークン>` が表示されるので、それを各マシンで実行します。
  v0.4 からエージェントはトークンを持たない（`kioku mcp`）ので、作業はこれだけです。
- トークンを含むエージェントのファイル（`~/.claude.json`、`~/.codex/config.toml`、`~/.cursor/mcp.json`、
  `~/.gemini/settings.json`）は 0600 で作成します。ほかのユーザーが読める既存のファイルに kioku が
  トークンを追加するときは 0600 にします（1 行で報告し、トークン自体は表示しません）。
- `config.toml` にはトークンが平文で入っています。kioku はこれをモード 0600 で書き、データディレクトリ・
  `raw/`・`logs/` を 0700 で作成します（unix）。
- **サニタイザ** — フックのペイロードは送信前にクライアントで（サーバーでも再度）伏せ字にされます:
  - AWS アクセスキー ID（`AKIA…`）、`sk-…` 形式のキー、Stripe の `sk_live_…` / `sk_test_…` キー、
    GitHub の `ghp_` / `gho_` / `ghu_` / `ghs_` / `ghr_` / `github_pat_…` トークン、
    Slack の `xoxb-` / `xoxa-` / `xoxp-` トークン、Google の `AIza…` キー、JWT 形式の文字列
    （`eyJ….….…`）、PEM 形式の秘密鍵ブロック（`END` 行が無い場合はテキストの末尾まで）
  - URL 内のパスワード（`postgres://user:[REDACTED]@host`）
  - `secret`、`token`、`password`/`passwd`、`api_key`、`access_key`、`private_key`、`credential`、
    `authorization` を*含む*キーに `:` か `=` が続く場合の値全体（`AWS_SECRET_ACCESS_KEY=…`、
    `"access_token": "…"`、`password = "複数の 単語"`。引用符で囲まれた値はひとまとまり、`Bearer …` も含む）、
    `--password` / `--token` / `--api-key` の後ろの値、およびそれらの語を含む名前のキーを持つ JSON の値
    （`max_tokens` のような件数は除く）
  - `tool_input` は 4 000 文字、`tool_response` は 2 000 文字に切り詰め
- **伏せ字にならないもの**: 上記の形に一致しないもの全般 — 例: `-p secret` のように渡したパスワード、
  単独の高エントロピー文字列、個人情報。
  プロンプト、コマンド、ファイルパス、`Read`/`Bash`/編集系ツールの（切り詰められた）出力はサーバーに届き、
  `raw/`、SQLite、そして要約された形で `wiki/` の git 履歴に残ります。トランスクリプトは送信しません。
- **フックは fail-open**: ネットワークやサーバーのエラー時、フックは `logs/hook.log`（上限 1 MiB、
  `hook.log.1` に 1 世代だけローテート）に 1 行記録し、
  何も出力せず、`timeout_ms` 以内に終了コード 0 で終わります。サーバーが落ちていてもエージェントは
  止まりません。0 以外で終わるのは、意図的な Stop の催促（2）だけです。

## ほかのツールとの比較

コーディングエージェントに永続的な記憶を与えるプロジェクトはほかにもあり（ai-memory、claude-mem、
memorix など）、一見の価値があります。kioku の違いは、日本語の形態素解析（lindera/IPADIC）により
日本語が実際に検索できること、Markdown-in-git が唯一の正本であること、既定で LLM を呼ばないこと、
そしてすべてのマシンが 1 台のセルフホストサーバーを共有することです。ロードマップには、生活全体の
取り込み（メール、カレンダー）と bi-temporal な事実管理があります。

## ロードマップ

- **M1**: サーバー、Markdown/git ストア、日本語検索、MCP ツール、Claude Code のフックと引き継ぎ、
  ルールベースの要約。
- **M2**（このリリース）: Codex CLI、Cursor、Gemini CLI。`install.sh`、
  `kioku setup` / `doctor` / `service` / `update`。
- **今後**: Web UI。
- **M3**: 埋め込み（embeddings）+ bi-temporal な事実管理。
- **M4**: 取り込みアダプタ。
- **M5**: 評価ハーネス。

## ライセンス

MIT OR Apache-2.0 のいずれかを選択できます（[LICENSE-MIT](LICENSE-MIT)、
[LICENSE-APACHE](LICENSE-APACHE)）。
