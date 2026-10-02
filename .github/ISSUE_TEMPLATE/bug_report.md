---
name: 不具合の報告 / Bug report
about: kioku が期待どおりに動かない / kioku does not work as expected
title: ""
labels: bug
---

<!--
セキュリティ上の問題は公開 issue にせず、SECURITY.md の手順で非公開で報告してください。
Security problems: do not open a public issue — see SECURITY.md.
-->

## 何が起きたか / What happened

<!-- 期待した動作と実際の動作 / what you expected and what happened instead -->

## 再現手順 / Steps to reproduce

1.
2.
3.

## 環境 / Environment

- kioku のバージョン / kioku version (`kioku --version`):
- サーバーのバージョン / server version (`kioku status`):
- OS (macOS / Linux / Windows + version):
- エージェントとそのバージョン / agent and its version (Claude Code / Codex / Cursor / Antigravity / Gemini CLI …):
- サーバーかクライアントか / this machine is: server / client only

## `kioku doctor` の出力 / `kioku doctor` output

<!--
`kioku doctor`（または `kioku doctor --json`）の出力を貼ってください。
**トークンを含む行があれば必ず削除してください**（kioku 自身はトークンを表示しませんが、
自分で追記したログや設定ファイルの抜粋には含まれることがあります）。
Paste the output of `kioku doctor` (or `kioku doctor --json`).
**Remove every line that contains a token** (kioku never prints it, but config excerpts
or logs you add might).
-->

```
```

## ログ / Logs (任意 / optional)

<!--
`~/.kioku/logs/hook.log`、`serve.log`、`update.log` の該当部分。トークンや秘密情報は削除してください。
Relevant lines of `~/.kioku/logs/hook.log`, `serve.log`, `update.log` — remove tokens and secrets.
-->
