# kioku demo (ja) — rendered by VHS against a throwaway local server
Output kioku-demo-ja.gif
Output kioku-demo-ja.mp4
Set Shell zsh
Set FontFamily "JetBrains Mono, Menlo, Hiragino Sans"
Set FontSize 15
Set Width 1320
Set Height 1040
Set Padding 20
Set Theme "Catppuccin Mocha"
Set TypingSpeed 35ms
Env HOME "@SANDBOX@/home"
Env KIOKU_DATA_DIR "@SANDBOX@/home/.kioku"
Env PATH "@SANDBOX@/bin:@CARGOBIN@:/opt/homebrew/bin:/usr/bin:/bin"
Hide
Type "cd @SANDBOX@/repo && setopt interactivecomments && export PS1='%F{cyan}demo-app%f %# ' && clear"
Enter
Show
Sleep 800ms
Type "# きのう Mac の Claude Code で直し、きょう Windows の Codex で続きをやった。"
Enter
Sleep 1800ms
Type 'kioku search "メール検証"'
Sleep 500ms
Enter
Sleep 5s
Type "clear"
Enter
Type "# 次のセッションを始めると、どのエージェントもこれを受け取る："
Enter
Sleep 1200ms
Type "next-session"
Sleep 400ms
Enter
Sleep 9s
Screenshot frame-ja-end.png
Sleep 1s
