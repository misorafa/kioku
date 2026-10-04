#!/bin/sh
# Seed the throwaway demo server with a plausible history (Japanese), never printing the token.
D=$1; P=$2; HOME_DIR=$3
export HOME=$HOME_DIR PATH=$HOME/.cargo/bin:$PATH
TOK=$(awk -F'"' '/^\[client\]/{c=1} c&&/^auth_token/{print $2; exit}' "$HOME/.kioku/config.toml")
[ -z "$TOK" ] && TOK=$(awk -F'"' '/^auth_token/{print $2; exit}' "$HOME/.kioku/config.toml")
U=http://127.0.0.1:47391/api/v1
post() { curl -s -m10 -X POST "$U/$1" -H "Authorization: Bearer $TOK" -H 'Content-Type: application/json' --data-binary "$2" >/dev/null; }
put() { curl -s -m10 -X PUT "$U/$1" -H "Authorization: Bearer $TOK" -H 'Content-Type: application/json' --data-binary "$2" >/dev/null; }
start() { post sessions/start "{\"session_id\":\"$1\",\"agent\":\"$2\",\"cwd\":\"$D/repo\",\"source\":\"startup\",\"machine\":\"$3\",\"project\":{\"id\":\"$P\",\"name\":\"demo-app\",\"root\":\"$D/repo\",\"remote\":\"https://github.com/misorafa/demo-app.git\"}}"; }
obs() { post observations "{\"session_id\":\"$1\",\"kind\":\"$2\",\"payload\":$3}"; }
# Session 1: Claude Code on the Mac
start s1 claude-code macbook
obs s1 prompt '{"prompt":"ログイン画面のバリデーションを直して"}'
obs s1 tool_use '{"tool_name":"Edit","tool_input":{"file_path":"src/auth/login.rs"}}'
obs s1 tool_use '{"tool_name":"Bash","tool_input":{"command":"cargo test -p auth"}}'
obs s1 tool_use '{"tool_name":"Bash","tool_input":{"command":"git commit -am \"fix(auth): validate email before password\""}}'
post handoffs "{\"project\":\"$P\",\"session\":\"s1\",\"summary\":\"ログイン画面のメール検証をパスワード検証より先に行うよう修正し、auth のテストを追加した\",\"next_steps\":[\"パスワード再設定画面にも同じ検証を入れる\",\"エラーメッセージを日本語化する\"],\"open_questions\":[\"メールの正規化（大文字小文字）はサーバー側でやるか\"],\"decisions\":[\"バリデーションは serde の前段で行う\",\"エラーは i18n キーで返す\"],\"gotchas\":[\"cargo test -p auth は DB のマイグレーションを先に流す必要がある\"]}"
post sessions/s1/finalize '{"reason":"demo"}'
# A pinned page and a design note
put pages "{\"project\":\"$P\",\"title\":\"作業ルール\",\"slug\":\"rules\",\"content\":\"- main に直接 push しない\\n- テストは cargo test --workspace\\n- 日本語のコメント OK\",\"tags\":[\"pinned\"]}"
put pages "{\"project\":\"$P\",\"title\":\"認証まわりの設計メモ\",\"slug\":\"auth-design\",\"content\":\"セッションは JWT ではなく DB セッションにする。理由：失効を即時にしたいため。\\nログインの検証順序：メール → パスワード → 2FA。\",\"tags\":[\"design\"]}"
# Session 2: Codex on Windows continues, writes a handoff for the next one
start s2 codex win-pc
obs s2 prompt '{"prompt":"パスワード再設定画面に同じ検証を入れて"}'
obs s2 tool_use '{"tool_name":"Edit","tool_input":{"file_path":"src/auth/reset.rs"}}'
post handoffs "{\"project\":\"$P\",\"session\":\"s2\",\"summary\":\"パスワード再設定画面にもメール検証を追加。i18n キーの一覧を docs/i18n.md に起こした\",\"next_steps\":[\"エラーメッセージの日本語訳を埋める\",\"2FA の画面にも同じ順序を適用\"],\"open_questions\":[],\"decisions\":[\"i18n のキーは auth.error.<name> で統一\"],\"verified\":[\"cargo test --workspace は全件通る\"]}"
post sessions/s2/finalize '{"reason":"demo"}'
echo seeded
