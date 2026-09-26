#!/usr/bin/env python3
"""Render the published benchmark charts from the saved numeric samples.

Requires matplotlib. Does not launch either application or measure the desktop.
"""
import json
from pathlib import Path
import statistics

import matplotlib
matplotlib.use('Agg')
matplotlib.rcParams['svg.hashsalt'] = 'vespera-benchmark-2026-09-15'
import matplotlib.pyplot as plt


ROOT = Path(__file__).resolve().parents[1]
ASSETS = ROOT / 'docs/assets/benchmarks/2026-09-15'
DATA = json.loads((ASSETS / 'measurements.json').read_text())
BACKGROUND = '#0b141a'
TEXT = '#edf3f5'
MUTED = '#a3b4bf'
GREEN = '#25d366'
GRAY = '#657b89'


def values(app, field):
    runs = [r for r in DATA['runs'] if r['app'] == app]
    if field == 'pss_mb':
        return [statistics.median(s['pss_kib'] for s in r['idle_samples']) * 1024 / 1e6 for r in runs]
    return [r[field] for r in runs]


def canvas(title, subtitle):
    fig = plt.figure(figsize=(12, 6.75), dpi=160, facecolor=BACKGROUND)
    fig.text(.07, .91, title, fontsize=28, weight='bold', color=TEXT)
    fig.text(.07, .85, subtitle, fontsize=12, color=MUTED)
    return fig


def bars(fig, bounds, field, unit, label):
    ax = fig.add_axes(bounds, facecolor=BACKGROUND)
    samples = [values(app, field) for app in ['vespera', 'whatsapp_web']]
    medians = [statistics.median(v) for v in samples]
    ax.barh([1, 0], medians, height=.35, color=[GREEN, GRAY])
    ax.set_yticks([1, 0], ['Vespera', 'WhatsApp Web\n+ Chromium'], color=TEXT, fontsize=12)
    ax.set_xlim(0, max(medians) * 1.29)
    ax.set_ylim(-.7, 1.7)
    ax.set_xticks([])
    ax.tick_params(axis='y', length=0, pad=12)
    for spine in ax.spines.values():
        spine.set_visible(False)
    ax.set_title(label, loc='left', color=TEXT, fontsize=15, weight='bold', pad=10)
    for y, number in zip([1, 0], medians):
        formatted = f'{number:,.0f}' if unit != 's' else f'{number / 1000:.2f}'
        ax.text(number + max(medians)*.025, y, f'{formatted} {unit}', va='center', color=TEXT, fontsize=14, weight='bold')


def save(fig, stem, description):
    for extension in ['svg', 'png']:
        metadata = {'Description': description}
        if extension == 'svg':
            metadata['Date'] = None
        output = ASSETS / f'{stem}.{extension}'
        fig.savefig(output, facecolor=BACKGROUND, metadata=metadata)
        if extension == 'svg':
            output.write_text('\n'.join(line.rstrip() for line in output.read_text().splitlines()) + '\n')
    plt.close(fig)


def main():
    native = statistics.median(values('vespera', 'pss_mb'))
    web = statistics.median(values('whatsapp_web', 'pss_mb'))
    saved = 100 * (1 - native / web)
    fig = canvas(f'{saved:.0f}% less idle RAM in our Linux test', 'Vespera 0.13.1 vs WhatsApp Web + Chromium 152')
    bars(fig, [.22, .29, .70, .45], 'pss_mb', 'MB', 'Resident RAM · PSS · lower is better')
    fig.text(.07, .20, 'Same account · 4 paired runs · median of 5 idle samples per run', fontsize=12, color=TEXT)
    fig.text(.07, .145, 'Includes Chromium’s full process tree; shared memory counted proportionally.', fontsize=11, color=MUTED)
    fig.text(.07, .10, 'One Linux desktop. Browser overhead included; an empty-browser baseline was not measured.', fontsize=10, color=MUTED)
    fig.text(.07, .04, 'zapfast.rocks/benchmarks', fontsize=12, weight='bold', color=GREEN)
    fig.text(.93, .04, '15 September 2026', fontsize=10, color=MUTED, ha='right')
    save(fig, 'memory', 'Four paired Linux runs. Vespera 150 MB PSS; WhatsApp Web and Chromium 1128 MB PSS. Full browser process tree included.')

    fig = canvas('From launch to the chat list', 'Four paired Linux launches · fresh processes · warm OS caches')
    bars(fig, [.22, .49, .68, .25], 'window_mapped_ms', 'ms', 'First window · same compositor event')
    bars(fig, [.22, .17, .68, .25], 'chat_ui_observed_ms', 's', 'Chat UI observed · approximate, different detectors*')
    fig.text(.07, .085, '*Native: “Chats” header via OCR. Web: chat pane + a row via DOM. Includes detection overhead.', fontsize=9.5, color=MUTED)
    fig.text(.07, .055, 'These observations do not measure full synchronization or establish an exact chat-readiness speedup.', fontsize=9.5, color=MUTED)
    fig.text(.07, .015, 'zapfast.rocks/benchmarks', fontsize=11, weight='bold', color=GREEN)
    save(fig, 'startup', 'Window appearance: Vespera 152 ms, Chromium 528 ms. Chat UI observations: 0.69 s and 4.13 s, using different detectors.')


if __name__ == '__main__':
    main()
