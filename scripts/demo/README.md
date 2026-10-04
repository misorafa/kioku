# Demo recordings

`sh scripts/demo/make-demo.sh` renders `docs/media/kioku-demo-{ja,en}.{gif,mp4}` with
[VHS](https://github.com/charmbracelet/vhs) against a throwaway kioku server in a temp HOME
(it never touches `~/.kioku`). Needs `kioku` on PATH plus `brew install vhs ffmpeg`.
Edit `demo-ja.tape` / `demo-en.tape` (the `@SANDBOX@` / `@CARGOBIN@` placeholders are filled
in by the script) and `seed-ja.sh` / `seed-en.sh` (the fake history) to change the story.
Re-run after a release so the recording shows the current output.
