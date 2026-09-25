# kioku (記憶)

日本語 | [English](README.md)

**ステータス: M1 — 初期段階です。粗い部分があることを前提に使ってください。**

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
                    <kioku> project id、未受領の引き継ぎ、STATE.md 抜粋 </kioku>
UserPromptSubmit  \
PostToolUse        > サニタイズ済みの観測 --> POST /api/v1/observations
PreCompact        /
Stop              引き継ぎ未記録 かつ ツール実行 3 回以上？
                    はい   -> exit 2 + 催促:「kioku_handoff_write で引き継ぎを書くこと」
                    いいえ -> finalize: セッションページ + STATE.md（+ ルール生成の引き継ぎ）
SessionEnd        finalize（冪等）
```

- **SessionStart での注入**: フックが `<kioku>` ブロックを出力します。中身はプロジェクト id、
  未受領の引き継ぎ（あれば）、プロジェクトの `STATE.md` の抜粋です。
- **Stop 時の催促**: エージェントがツールを 3 回以上使い、まだ `kioku_handoff_write` を呼んでいなければ、
  Stop フックは終了コード 2 で終わり、要約・次にやること・未解決の質問・決定事項を記録するよう求めます。
  `stop_hook_active` によりループはしません。`[client] stop_nudge = false` または
  `KIOKU_STOP_NUDGE=0` で無効にできます。
- **finalize** はセッションページを書き、`STATE.md` を書き直し、エージェントが引き継ぎを書かなかった
  場合はルールで生成します（最後の指示、触ったファイル、コマンド、コミット、エラー件数）。
  ページの書き込みはすべて git コミットになります。
- **引き継ぎは一度きり**: 同じプロジェクトの次の SessionStart が最新の未受領の引き継ぎを受け取ります
  （それより古い未受領のものは置き換え済みとして受領扱いになります）。`kioku_handoff_pending` を
  `accept=false` で呼ぶと、消費せずに覗くだけです。

## クイックスタート（1 台で使う）

必要なもの: ビルドに Rust 1.95 以上、`git`（任意。無い場合 wiki はバージョン管理されません）。
ビルド時に IPADIC 辞書をダウンロードするため、ネットワーク接続が必要です。Linux（x86_64、aarch64）と
macOS（arm64、x86_64）のビルド済みバイナリは、タグ付きの GitHub リリースに添付されます。

```sh
cargo install --locked --path crates/kioku-cli   # `kioku` をインストール

kioku init                    # ~/.kioku: 新しいトークン入りの config.toml、wiki の git リポジトリ、db、index
kioku serve                   # 起動したままにする（127.0.0.1:7391）
kioku install claude-code     # 別のターミナルで: フック + MCP の登録
```

`kioku install claude-code` はフックの設定を `~/.claude/settings.json` にマージし
（`--project` なら `./.claude/settings.json`。最初の変更の前に `settings.json.kioku-bak` へ
バックアップします）、`claude mcp add --transport http kioku http://127.0.0.1:7391/mcp
--header "Authorization: Bearer <token>" --scope user` を実行します。`claude` が PATH に無ければ、
代わりに `~/.claude.json` 用の設定断片を表示します。`kioku uninstall claude-code` は追加したものだけを
正確に取り除きます。

試してみる:

1. git リポジトリで Claude Code を開き、タスクを与えます。そのディレクトリに kioku が使う
   プロジェクト id は `kioku project id` で確認できます。
2. ツールを何回か使ったターンが終わると、Stop の催促が `kioku_handoff_write` を呼ぶよう求めます。
3. 同じリポジトリで新しいセッションを始めると（または `/clear`）、SessionStart で引き継ぎが注入されます。
4. ターミナルから検索します:

```sh
kioku search 引き継ぎ
kioku search --project <id> --limit 5 設計 判断
kioku status
```

## 自宅サーバーで使う

すべてのマシンで 1 台のサーバーを共有します。

サーバー側:

```sh
kioku init
kioku serve --bind 0.0.0.0     # または config.toml の [server] bind = "0.0.0.0"、または KIOKU_BIND
```

ほかのすべてのマシン（ノート PC、デスクトップ）:

```sh
kioku init --client-only https://kioku.example.com <サーバーの config.toml にある auth_token>
kioku install claude-code
```

`--client-only` は `[client]` セクションだけを書き、認証付きのリクエストで URL とトークンを確認します。

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
`kioku init --client-only` に必要です）。ホストのディレクトリをバインドマウントする場合は、
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
| `kioku_handoff_write` | `project`、`session?`、`summary`、`next_steps`、`open_questions`、`decisions` | そのプロジェクトの次のセッションが受け取る引き継ぎを記録する |
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
  logs/hook.log                 # クライアント側フックの失敗ログ
```

ページは YAML frontmatter 付きの Markdown なので、どのエディタでも読み書きできます（手で編集したら
`kioku reindex` を実行すると検索に反映されます）。kioku はコミットはしますが push はしません。
バックアップ（たとえば `wiki/` をプライベートなリモートに push し、引き継ぎが入っている `db/` を
コピーする）はご自身で行ってください。

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
- `config.toml` にはトークンが平文で入っています。`chmod 600` してください。
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
- **フックは fail-open**: ネットワークやサーバーのエラー時、フックは `logs/hook.log` に 1 行記録し、
  何も出力せず、`timeout_ms` 以内に終了コード 0 で終わります。サーバーが落ちていてもエージェントは
  止まりません。0 以外で終わるのは、意図的な Stop の催促（2）だけです。

## ほかのツールとの比較

コーディングエージェントに永続的な記憶を与えるプロジェクトはほかにもあり（ai-memory、claude-mem、
memorix など）、一見の価値があります。kioku の違いは、日本語の形態素解析（lindera/IPADIC）により
日本語が実際に検索できること、Markdown-in-git が唯一の正本であること、既定で LLM を呼ばないこと、
そしてすべてのマシンが 1 台のセルフホストサーバーを共有することです。ロードマップには、生活全体の
取り込み（メール、カレンダー）と bi-temporal な事実管理があります。

## ロードマップ

- **M1**（このリリース）: サーバー、Markdown/git ストア、日本語検索、MCP ツール、Claude Code の
  フックと引き継ぎ、ルールベースの要約。
- **M2**: Web UI + ほかのエージェント用インストーラ。
- **M3**: 埋め込み（embeddings）+ bi-temporal な事実管理。
- **M4**: 取り込みアダプタ。
- **M5**: 評価ハーネス。

## ライセンス

MIT OR Apache-2.0 のいずれかを選択できます（[LICENSE-MIT](LICENSE-MIT)、
[LICENSE-APACHE](LICENSE-APACHE)）。
