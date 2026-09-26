#!/usr/bin/env python3
"""Add the tour's pointer, click rings, and shortcut captions to a silent MP4.

The app emits only input timing and coordinates. All visual annotations are
composited here, outside Vespera. Requires ffmpeg with libass, and ffprobe.
"""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile


def timestamp(seconds):
    cs = round(max(0, seconds) * 100)
    return f'{cs // 360000}:{cs // 6000 % 60:02}:{cs // 100 % 60:02}.{cs % 100:02}'


def escape(text):
    return text.replace('\\', '').replace('{', '').replace('}', '').replace('\n', ' ')


def captions(trace):
    width, height = round(trace['width']), round(trace['height'])
    duration = trace['duration']
    header = f'''[Script Info]
ScriptType: v4.00+
PlayResX: {width}
PlayResY: {height + 64}
WrapStyle: 2
ScaledBorderAndShadow: yes

[V4+ Styles]
Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding
Style: Keys,Inter,22,&H00FFFFFF,&H00FFFFFF,&H000B1519,&H000B1519,0,0,0,0,100,100,0,0,1,0,0,5,0,0,0,1
Style: Pointer,Inter,20,&H00FFFFFF,&H00FFFFFF,&H0010181E,&H00000000,0,0,0,0,100,100,0,0,1,1.3,0,7,0,0,0,1

[Events]
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text
'''
    lines = [header]

    def event(layer, start, end, style, text):
        if end > start:
            lines.append(f'Dialogue: {layer},{timestamp(start)},{timestamp(end)},{style},,0,0,0,,{text}\n')

    pointers = [e for e in trace['events'] if e['kind'] == 'pointer']
    for index, point in enumerate(pointers):
        following = pointers[index + 1] if index + 1 < len(pointers) else dict(point, at=duration)
        move = f"\\move({point['x']:.2f},{point['y']:.2f},{following['x']:.2f},{following['y']:.2f})"
        event(3, point['at'], following['at'], 'Pointer',
              '{' + move + '\\p1}m 0 0 l 0 22 l 5 17 l 10 27 l 14 25 l 10 15 l 19 15{\\p0}')

    badges = []
    for item in trace['events']:
        if item['kind'] == 'keys':
            badges.append((item['at'], item['label']))
        elif item['kind'] == 'click':
            x, y = item['x'], item['y']
            ring = (f'{{\\pos({x - 14:.2f},{y - 14:.2f})\\fad(0,350)\\p1'
                    '\\1a&HFF&\\3c&H84A800&\\bord2}'
                    'm 14 0 b 22 0 28 6 28 14 b 28 22 22 28 14 28 '
                    'b 6 28 0 22 0 14 b 0 6 6 0 14 0{\\p0}')
            event(2, item['at'], item['at'] + .35, 'Pointer', ring)
            if item['button'] == 'right':
                badges.append((item['at'], 'Right click · Message menu'))
    badges.sort()
    for index, (at, label) in enumerate(badges):
        end = min(at + 1.8, badges[index + 1][0] if index + 1 < len(badges) else duration)
        # A generously padded, outlined keycap in a separate video caption band.
        half = min(width / 2 - 20, max(150, len(label) * 6 + 24))
        x, y = width / 2, height + 32
        # ASS uses cubic Bezier curves; straight chamfered corners are portable.
        box = (f'{{\\an7\\pos({x - half:.1f},{y - 22:.1f})\\p1\\1c&H192923&\\3c&H84A800&\\bord1.5}}'
               f'm 8 0 l {2 * half - 8:.1f} 0 l {2 * half:.1f} 8 l {2 * half:.1f} 36 '
               f'l {2 * half - 8:.1f} 44 l 8 44 l 0 36 l 0 8 l 8 0{{\\p0}}')
        event(0, at, end, 'Keys', box)
        event(1, at, end, 'Keys', f'{{\\pos({x:.1f},{y:.1f})}}{escape(label)}')
    return ''.join(lines)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('recording', type=Path)
    parser.add_argument('events', type=Path)
    parser.add_argument('output', type=Path)
    parser.add_argument('--start', type=float, default=0, help='Seconds to trim before the tour starts')
    args = parser.parse_args()
    trace = json.loads(args.events.read_text())
    if not trace.get('complete'):
        parser.error('The tour did not complete; inspect the app log before exporting.')
    if args.output.exists():
        parser.error('Output already exists; choose a new filename.')
    width, height = round(trace['width']), round(trace['height'])
    with tempfile.TemporaryDirectory(prefix='vespera-tour-') as tmp:
        ass = Path(tmp) / 'captions.ass'
        ass.write_text(captions(trace))
        subprocess.run([
            'ffmpeg', '-v', 'error', '-ss', str(args.start), '-i', str(args.recording),
            '-t', str(trace['duration']), '-map', '0:v:0', '-an', '-sn',
            '-vf', f'scale={width}:{height},pad=iw:ih+64:0:0:color=0x0b1519,ass={ass}',
            '-r', '30', '-c:v', 'libx264', '-preset', 'slow', '-crf', '18',
            '-pix_fmt', 'yuv420p', '-map_metadata', '-1', '-movflags', '+faststart', str(args.output),
        ], check=True)
    info = json.loads(subprocess.check_output([
        'ffprobe', '-v', 'error', '-show_streams', '-show_format', '-of', 'json', str(args.output)]))
    if len(info['streams']) != 1 or info['streams'][0]['codec_type'] != 'video':
        raise RuntimeError('Export must contain exactly one video stream and no audio')
    print(args.output)


if __name__ == '__main__':
    main()
