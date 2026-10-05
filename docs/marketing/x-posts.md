# X 投稿文（下書き） / X post drafts

素材: `docs/media/kioku-short-ja.mp4`（68 秒、縦）, `kioku-short-en.mp4`（54 秒）, `docs/media/kioku-flow-{ja,en}.png`（1 枚図）,
`docs/media/kioku-demo-{ja,en}.gif`（ターミナル実演）。動画は MP4 を直接添付（GIF より画質がよい）。

---

## 日本語

### A. 単発（動画 1 本を添付）

AI コーディングエージェントを乗り換えるたびに、記憶がゼロに戻るのが嫌で、自分用の記憶サーバーを作りました。

kioku（記憶）
・Claude Code / Codex / Cursor / Antigravity で共有
・Mac / Windows / Linux のどのマシンからでも続きから
・日本語のために作った（日本語の要約・引き継ぎ・全文検索）
・Rust 製シングルバイナリ、LLM は呼ばない、データは自宅サーバーに
・無料・オープンソース

導入は 1 行。
https://github.com/misorafa/kioku

### B. 3 連投（1 本目に動画、2 本目に 1 枚図、3 本目にコマンド）

**1/3**
きのう Claude Code で直した続きを Codex に頼むと「どのログイン画面のことですか？」。
エージェントを変えると記憶はゼロから。マシンを変えてもゼロから。

これを直すために、エージェント横断の記憶サーバー kioku（記憶）を作りました。
https://github.com/misorafa/kioku

**2/3**
しくみは 3 つだけ。
① 記録：フックが指示・編集・最後の回答を自動で送る
② 要約：サーバーがルールで引き継ぎ・決定事項にまとめる（LLM は使わない）
③ 注入：次のセッションが、どのエージェント・どのマシンでも続きから始まる

保存先は git 管理の Markdown + SQLite。検索は tantivy + lindera で日本語がちゃんと引ける。Rust 製の 1 バイナリ。

**3/3**
導入は 1 行。サーバーは Mac / Linux / Docker、クライアントは Windows も。
ほかのマシンは招待コードを 1 行貼るだけで参加できます。

curl -fsSL https://raw.githubusercontent.com/misorafa/kioku/main/install.sh | sh

無料・オープンソース。感想や要望、歓迎です。
https://github.com/misorafa/kioku

### 返信欄に添える補足（任意）
- 「LLM を呼ばない」のは、要約を決定的で速く、無料に保つため。要約の質はエージェント自身が書く引き継ぎで補います（セッション終了時に自動で促します）。
- 1 サーバー = 1 人の設計です。チーム共有は対象外。
- Windows のバイナリは現在無署名（SmartScreen の警告が出ます）。署名は申請中です。

---

## English

### A. Single post (attach the video)

I got tired of my memory resetting to zero every time I switched coding agents, so I built a memory server for myself.

kioku (記憶)
• Shared by Claude Code / Codex / Cursor / Antigravity
• Pick up where you left off from any Mac / Windows / Linux machine
• Built Japanese-first (summaries, handoffs and full-text search that actually work in Japanese)
• Single Rust binary, no LLM calls, your data on your own server
• Free and open source

One-line install.
https://github.com/misorafa/kioku

### B. Thread (video on 1, diagram on 2, command on 3)

**1/3**
Yesterday Claude Code fixed my login screen. Today I ask Codex to continue and it says "which login screen?"
Switch agents: memory starts at zero. Switch machines: zero again.

So I built kioku — a shared memory server for coding agents.
https://github.com/misorafa/kioku

**2/3**
Three steps, nothing else:
1 Capture — hooks send prompts, edits and the final reply automatically
2 Summarize — the server writes handoffs and decisions by rule (no LLM calls)
3 Inject — the next session, on any agent and any machine, starts where you left off

Storage is Markdown in git + SQLite; search is tantivy + lindera, so Japanese works as well as English. One Rust binary.

**3/3**
Install in one line. The server runs on a Mac, Linux or Docker; Windows joins as a client. Other machines paste an invite line.

curl -fsSL https://raw.githubusercontent.com/misorafa/kioku/main/install.sh | sh

Free, open source. Feedback welcome.
https://github.com/misorafa/kioku

### Optional follow-up replies
- Why no LLM calls: summaries stay deterministic, fast and free; the agent's own handoff (requested automatically at the end of a session) carries the judgement.
- One server = one person by design; team sharing is out of scope.
- The Windows binary is unsigned for now (SmartScreen warns); code signing is in progress.
