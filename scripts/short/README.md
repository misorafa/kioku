# Short video (9:16) for X

`short.html` is a 58-second motion-graphics story (problem → kioku → how it works → what the
next session sees → search → one-line install → end card), in Japanese by default and in
English with `?lang=en`. `record.mjs` renders it headless with Playwright at 1080×1920:

```sh
cd scripts/short && npm i playwright@1 && npx playwright install chromium
node record.mjs ja && node record.mjs en          # out/kioku-short-<lang>.webm
ffmpeg -i out/kioku-short-ja.webm -r 30 -c:v libx264 -crf 20 -pix_fmt yuv420p -movflags +faststart kioku-short-ja.mp4
node record.mjs ja stills                          # out/still-ja-*.png to review frames
```

Edit the `T` table in `short.html` for the copy and `starts` for the timing. The MP4s are
not committed (≈7 MB each); render them when needed.
