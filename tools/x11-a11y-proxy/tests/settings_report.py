#!/usr/bin/env python3
"""Evaluates a settings.sh run: per toolkit, what it reported (its SETTINGS lines) and how its window looked
(screenshot size, share of pure black/white pixels, mean luminance) on GNOME's Xwayland with gsd-xsettings (G), on
the engine without the mirror (E0) and with it (E), before the change (p0), after it while running (p0live) and
started after it (p1). Writes settings_report.json / .md into the run directory. Usage: settings_report.py RUN_DIR"""
import ast
import json
import os
import re
import sys

from PIL import Image

D = sys.argv[1]
KINDS = ['gtk3', 'gtk4', 'qt5', 'qt6', 'java', 'electron', 'tk', 'motif', 'xcalc']
LABELS = [('E0', 'E0', 'E0'), ('G', 'G0', 'G_p0'), ('G', 'G0', 'G_p0live'), ('G', 'G1', 'G_p1'),
          ('E', 'E0m', 'E_p0'), ('E', 'E0m', 'E_p0live'), ('E', 'E1', 'E_p1')]


def settings_lines(kind, tag):
    f = os.path.join(D, f'app_{kind}_{tag}.log')
    if not os.path.exists(f):
        return []
    out = []
    for line in open(f, errors='replace'):
        if 'SETTINGS' in line:
            m = re.search(r'SETTINGS (electron(?:-css)?) (\{.*\})', line)
            if m:
                try:
                    out.append((m.group(1), json.loads(m.group(2))))
                except ValueError:
                    pass
                continue
            m = re.search(r'SETTINGS (gtk3|gtk4|tk) (\{.*\})', line)
            if m:
                try:
                    out.append((m.group(1), ast.literal_eval(m.group(2))))
                except (ValueError, SyntaxError):
                    pass
                continue
            m = re.search(r'SETTINGS qt (\d) (\{.*\})', line)
            if m:
                out.append(('qt', ast.literal_eval(m.group(2))))
                continue
            m = re.search(r'SETTINGS java \{(.*)\}', line)
            if m:
                out.append(('java', dict(kv.split('=', 1) for kv in re.split(r', (?=[\w.]+(?:/\w+)?=)', m.group(1)) if '=' in kv)))
    return out


KEYS = {
    'gtk3': ['gtk-theme-name', 'gtk-xft-dpi', 'gtk-cursor-theme-size', 'gtk-enable-animations'],
    'gtk4': ['gtk-theme-name', 'gtk-xft-dpi', 'gtk-cursor-theme-size', 'gtk-enable-animations',
             'gtk-interface-reduced-motion', 'gtk-interface-contrast'],
    'qt': ['logicalDpi', 'dpr', 'contrast', 'window'],
    'java': ['gnome.Net/ThemeName', 'gnome.Xft/DPI', 'gnome.Gtk/CursorThemeSize', 'uiScale', 'lnf'],
    'electron': ['hc', 'scale'], 'electron-css': ['reducedMotion', 'dpr', 'contrast', 'forcedColors'],
    'tk': ['bg', 'button_fg', 'button_font', 'tk_scaling'],
}


def pick(kind, lines, when):
    """The settings a toolkit reported: the first lines (p0), or the last (live/after)."""
    if not lines:
        return {}
    out = {}
    order = lines if when == 'first' else list(reversed(lines))
    for name, d in order:
        for k in KEYS.get(name, []):
            if k in d and f'{name}:{k}' not in out:
                out[f'{name}:{k}'] = d[k]
    return out


def shot(kind, label):
    f = os.path.join(D, f'shot_{kind}_{label}.png')
    if not os.path.exists(f):
        return None
    im = Image.open(f).convert('L')
    px = list(im.getdata())
    n = len(px) or 1
    return {'size': im.size, 'black': round(sum(1 for p in px if p < 30) / n, 3),
            'white': round(sum(1 for p in px if p > 225) / n, 3), 'mean': round(sum(px) / n, 1)}


def main():
    rep, md = {}, ['# settings carry-over', '',
                   'p0 = defaults; p0live = the same apps after high contrast + text scaling 1.5 + cursor 48 + '
                   'reduced motion were switched on; p1 = apps started after the change. G = GNOME\'s Xwayland with '
                   'gsd-xsettings; E0 = engine, no mirror; E = engine with the proxy\'s settings mirror.', '']
    for kind in KINDS:
        r = {}
        md += [f'## {kind}', '', '| arm/phase | reported | window (size, black, white, mean) |', '|---|---|---|']
        for arm, tag, label in LABELS:
            lines = settings_lines(kind, tag)
            when = 'last' if label.endswith('live') or label.endswith('p1') else 'first'
            s = pick(kind, lines, when)
            sh = shot(kind, label)
            r[label] = {'reported': s, 'shot': sh}
            rep_s = ', '.join(f'{k.split(":", 1)[1]}={v}' for k, v in s.items()) or '(nothing reported)'
            md.append(f'| {label} | {rep_s} | {sh["size"] if sh else "-"} {sh["black"] if sh else ""} '
                      f'{sh["white"] if sh else ""} {sh["mean"] if sh else ""} |')
        rep[kind] = r
        md.append('')
    for lab in ('E0', 'G_p0', 'G_p1', 'E_p0', 'E_p1', 'E_after_mirror_exit'):
        f = os.path.join(D, f'xs_{lab}.json')
        if os.path.exists(f):
            try:
                d = json.load(open(f))
            except ValueError:
                continue
            xs = d.get('xsettings') or {}
            md.append(f'- XSETTINGS {lab}: owner {d.get("xsettings_owner")}; ' + ', '.join(
                f'{k}={xs[k]}' for k in ('Net/ThemeName', 'Xft/DPI', 'Gtk/CursorThemeSize', 'Gtk/EnableAnimations',
                                         'Gtk/FontName') if k in xs) +
                f'; RESOURCE_MANAGER lines: {len(d.get("resource_manager") or [])}')
    json.dump(rep, open(os.path.join(D, 'settings_report.json'), 'w'), indent=1, default=str)
    open(os.path.join(D, 'settings_report.md'), 'w').write('\n'.join(md) + '\n')
    print('\n'.join(md))


main()
