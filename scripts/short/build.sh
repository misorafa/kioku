#!/bin/sh
# build.sh <ja|en> — narration (edge-tts) → scene timings → Playwright video → SFX → final MP4.
set -eu
L=${1:-ja}
cd "$(dirname "$0")"
export PATH=/Users/room/.nvm/versions/node/v20.19.4/bin:/opt/homebrew/bin:$PATH
mkdir -p out/tts-$L
# 1. narration clips + per-scene timing (scene length = max(narration + 0.6 s, min))
python3 - "$L" <<'PY'
import json, subprocess, sys, os
L = sys.argv[1]; cfg = json.load(open('narration.json'))[L]
starts, t, meta = [], 0.0, []
for i, s in enumerate(cfg['scenes']):
    mp3 = f"out/tts-{L}/{i:02d}-{s['id']}.mp3"
    if not os.path.exists(mp3):
        subprocess.run(['./tts/bin/edge-tts', '--voice', cfg['voice'], f"--rate={cfg['rate']}", '--text', s['text'], '--write-media', mp3], check=True, capture_output=True)
    dur = float(subprocess.run(['ffprobe', '-v', 'error', '-show_entries', 'format=duration', '-of', 'csv=p=0', mp3], capture_output=True, text=True).stdout.strip())
    starts.append(round(t, 2)); length = max(dur + 0.6, s['min']); meta.append({'id': s['id'], 'start': round(t, 2), 'dur': round(dur, 2), 'len': round(length, 2), 'mp3': mp3}); t += length
total = round(t + 0.8, 1)
json.dump({'starts': starts, 'total': total, 'scenes': meta}, open(f'out/timing-{L}.json', 'w'), indent=1)
print('starts', ','.join(map(str, starts)), 'total', total)
PY
STARTS=$(python3 -c "import json;print(','.join(map(str,json.load(open('out/timing-$L.json'))['starts'])))")
TOTAL=$(python3 -c "import json;print(json.load(open('out/timing-$L.json'))['total'])")
# 2. video
node record.mjs "$L" "starts=$STARTS" "total=$TOTAL" | tail -1
# 3. sound effects (synthesized): a soft whoosh at each scene change, a chime on the logo and the end card
[ -f out/sfx-whoosh.wav ] || ffmpeg -v error -y -f lavfi -i "anoisesrc=color=pink:amplitude=0.6:duration=0.45" -af "bandpass=f=900:width_type=o:w=1.2,afade=t=in:d=0.05,afade=t=out:st=0.15:d=0.3,volume=0.5" out/sfx-whoosh.wav
[ -f out/sfx-chime.wav ] || ffmpeg -v error -y -f lavfi -i "sine=frequency=880:duration=0.9" -f lavfi -i "sine=frequency=1320:duration=0.9" -filter_complex "[0][1]amix=inputs=2,afade=t=in:d=0.01,afade=t=out:st=0.25:d=0.65,volume=0.35" out/sfx-chime.wav
[ -f out/sfx-pop.wav ] || ffmpeg -v error -y -f lavfi -i "sine=frequency=520:duration=0.12" -af "afade=t=in:d=0.005,afade=t=out:st=0.03:d=0.09,volume=0.35" out/sfx-pop.wav
# 4. mix: narration at scene starts, whoosh at scene changes (not the first), chime at logo (scene 3) and end (scene 9)
python3 - "$L" <<'PY'
import json, subprocess, sys
L = sys.argv[1]; tm = json.load(open(f'out/timing-{L}.json'))
inputs, filters, labels = [], [], []
def add(path, at_ms, vol=1.0):
    idx = len(inputs) // 2 + 1; inputs.extend(['-i', path])
    filters.append(f"[{idx}:a]volume={vol},adelay={at_ms}|{at_ms}[a{idx}]"); labels.append(f"[a{idx}]")
for i, s in enumerate(tm['scenes']):
    add(s['mp3'], int((s['start'] + 0.25) * 1000), 1.0)
    if i > 0: add('out/sfx-whoosh.wav', int(s['start'] * 1000), 0.9)
    if s['id'] in ('s3', 's8'): add('out/sfx-chime.wav', int((s['start'] + 0.15) * 1000), 1.0)
    if s['id'] == 's4':
        for k in range(3): add('out/sfx-pop.wav', int((s['start'] + 0.5 + k * 0.8) * 1000), 0.8)
fc = ';'.join(filters) + ';' + ''.join(labels) + f"amix=inputs={len(labels)}:normalize=0,alimiter=limit=0.95,loudnorm=I=-16:TP=-1.5:LRA=11[aout]"
cmd = ['ffmpeg', '-v', 'error', '-y', '-i', f'out/kioku-short-{L}.webm'] + inputs + ['-filter_complex', fc, '-map', '0:v', '-map', '[aout]',
       '-r', '30', '-c:v', 'libx264', '-preset', 'medium', '-crf', '20', '-pix_fmt', 'yuv420p', '-c:a', 'aac', '-b:a', '160k', '-t', str(tm['total']), '-movflags', '+faststart', f'out/kioku-short-{L}.mp4']
subprocess.run(cmd, check=True)
print('mixed', f'out/kioku-short-{L}.mp4')
PY
ffprobe -v error -show_entries format=duration -of csv=p=0 out/kioku-short-$L.mp4
