# Short video (9:16) for X

`short.html` is a 58-second motion-graphics story (problem → kioku → how it works → what the
next session sees → search → one-line install → end card), in Japanese by default and in
English with `?lang=en`. `record.mjs` renders it headless with Playwright at 1080×1920:

```sh
cd scripts/short
npm i playwright@1 && npx playwright install chromium        # once
python3 -m venv tts && ./tts/bin/pip install edge-tts          # once (narration voices)
sh build.sh ja && sh build.sh en                               # out/kioku-short-<lang>.mp4 (with narration + SFX)
node record.mjs ja stills                                      # out/still-ja-*.png to review frames
```

`build.sh` synthesizes the narration in `narration.json` (ja: Nanami, en: Emma; edit the text
or the `rate`), derives each scene's length from its clip, records the page with those
timings, adds synthesized sound effects (whoosh on scene changes, chime on the logo and end
card, pops on the three steps) and mixes everything into an X-friendly MP4. Edit the `T`
table in `short.html` for on-screen copy. The MP4s are not committed (~9 MB each).

`diagram.html` + `node shoot.mjs` render the one-page flow diagram `docs/media/kioku-flow-{ja,en}.png` (1600×900 @2x).
Post drafts live in `docs/marketing/x-posts.md`.
