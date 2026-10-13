#!/usr/bin/env python3
"""Evaluates a parity.sh run: per toolkit and step, Orca's speech in arm X (the app on GNOME's Xwayland) vs arm P
(the app on the engine behind the proxy); the app's effects (mnemonics, typing, values, shortcuts, AT actions,
pointer) per arm; the focus order on the app's private bus vs the proxy's mirrors on the desktop's bus; and tree
equivalence (app tree vs mirror tree). Writes report.json and report.md into the run directory.

Effects are STEP-BOUND when the run has app_KIND_ARM_ts.log (parity.sh writes them): an expected effect counts only
if the app printed it between its step's mark and the next mark, and each step's effects are compared between the
arms. (Until proxy_fix the check took an expected line anywhere later in the log: GTK 4's S10 ACTIVATE was counted
for S7, and Tk's / Motif's results were mis-attributed.) Runs without the
time-stamped logs fall back to that ordered-subsequence check, and the report says so.
Usage: report.py RUN_DIR"""
import difflib
import json
import os
import re
import sys
from datetime import datetime, timezone

D = sys.argv[1]
API = ['gtk3', 'gtk4', 'qt5', 'qt6', 'java', 'electron']
INF = ['tk', 'motif', 'xcalc']
MENU = ['gtk3menu', 'qt6menu']
EXPECT_API = [('S2 alt+c', r'TOGGLE (True|true)'), ('S3 type 12', r'TEXT 12'), ('S5 right x2', r'VALUE 42'),
              ('S7 space', r'ACTIVATE Seven'), ('S8 ctrl+k', r'SHORTCUT ctrl\+k'),
              ('S10 AT action', r'ACTIVATE Seven'), ('S11 AT set value', r'VALUE 75(\.0)?$'),
              ('S12 pointer click', r'TOGGLE (False|false)')]
# what the native app does (arm X, step-bound): Tk's Tab x3 lands in the entry, so Space types a space; Motif's
# activates Seven (the proxy's geometric Tab order is not Motif's traversal: through the proxy it toggles Check me)
EXPECT_INF = {'tk': [('S2 space', r'^TEXT\s*$'), ('S4 alt+s', r'ACTIVATE Seven'),
                     ('S6 AT action', r'ACTIVATE Seven')],
              'motif': [('S2 space', r'ACTIVATE Seven'), ('S6 AT action', r'ACTIVATE Seven')],
              'xcalc': [],
              'gtk3menu': [('S4 ctrl+s', r'ACTIVATE Save'), ('S5 menu: Save', r'ACTIVATE Save')],
              'qt6menu': [('S4 ctrl+s', r'ACTIVATE Save'), ('S5 menu: Save', r'ACTIVATE Save')]}


def ts(s):
    """'08:21:13.743' -> seconds of the day."""
    h, m, rest = s.split(':')
    return int(h) * 3600 + int(m) * 60 + float(rest)


def read_speech():
    steps, cur = {}, None
    for line in open(os.path.join(D, 'speech.log'), errors='replace'):
        line = line.rstrip('\n')
        m = re.match(r'^(\S+) === MARK (\S+) (\S+) (.*)$', line)
        if m:
            cur = (m.group(2), m.group(3), m.group(4))
            steps.setdefault(cur, {'t': ts(m.group(1)), 'lines': []})
            continue
        m = re.match(r'^(\S+) (.*)$', line)
        if m and cur:
            steps[cur]['lines'].append(m.group(2).strip())
    return steps


def norm(lines):
    words = ' '.join(lines).lower()
    words = re.sub(r'[.,;:]+(\s|$)', ' ', words)
    return words.split()


def _effect(line):
    m = re.search(r'"((?:ACTIVATE|TOGGLE|TEXT|VALUE|SHORTCUT)[^"]*)"', line)   # Electron's console lines
    if m:
        return m.group(1)
    m = re.match(r'^((?:ACTIVATE|TOGGLE|TEXT|VALUE|SHORTCUT).*)$', line.strip())
    return m.group(1) if m else None


def effects(kind, arm):
    f = os.path.join(D, f'app_{kind}_{arm}.log')
    if not os.path.exists(f):
        return []
    return [e for e in (_effect(line) for line in open(f, errors='replace')) if e]


def read_marks():
    out = []
    p = os.path.join(D, 'marks.txt')
    if os.path.exists(p):
        for line in open(p):
            m = re.match(r'^(\S+) === (\S+) (\S+) (.*)$', line.strip())
            if m:
                out.append((ts(m.group(1)), m.group(2), m.group(3), m.group(4)))
    return sorted(out)


def effects_by_step(kind, arm, marks):
    """{step id ('S2'): [effects]} from the time-stamped app log, each effect in the step whose mark precedes it
    (None without the time-stamped log)."""
    f = os.path.join(D, f'app_{kind}_{arm}_ts.log')
    if not os.path.exists(f) or not marks:
        return None
    out = {}
    times = [mk[0] for mk in marks]
    for line in open(f, errors='replace'):
        m = re.match(r'^(\d\d:\d\d:\d\d\.\d+) (.*)$', line.rstrip('\n'))
        if not m:
            continue
        e = _effect(m.group(2))
        if not e:
            continue
        t = ts(m.group(1))
        i = max((k for k, mt in enumerate(times) if mt <= t), default=None)
        if i is None or marks[i][1] != arm or marks[i][2] != kind:
            continue
        out.setdefault(marks[i][3].split(' ')[0], []).append(e)
    return out


def check_effects_bound(by_step, expect):
    res = []
    for label, pat in expect:
        sid = label.split(' ')[0]
        hit = next((e for e in by_step.get(sid, []) if re.search(pat, e)), None)
        res.append({'step': label, 'expect': pat, 'ok': hit is not None, 'got': hit})
    return res


def check_effects(eff, expect):
    """Each expected effect in order (as a subsequence)."""
    res, i = [], 0
    for label, pat in expect:
        j = next((k for k in range(i, len(eff)) if re.search(pat, eff[k])), None)
        res.append({'step': label, 'expect': pat, 'ok': j is not None, 'got': eff[j] if j is not None else None})
        if j is not None:
            i = j + 1
    return res


def focus_seq(path, app_filter, t0, t1):
    out = []
    if not os.path.exists(path):
        return out
    for line in open(path):
        try:
            r = json.loads(line)
        except ValueError:
            continue
        if 'focused' not in r.get('type', '') or r.get('d1') != 1:
            continue
        t = datetime.fromtimestamp(r["t"], timezone.utc)
        sec = t.hour * 3600 + t.minute * 60 + t.second + t.microsecond / 1e6
        if not (t0 <= sec <= t1) or (app_filter and app_filter not in (r.get('app') or '')):
            continue
        role = FOCUS_ROLE_EQ.get(r.get('role'), r.get('role'))
        item = f"{role}:{r.get('name') or ''}"
        if not out or out[-1] != item:
            out.append(item)
    return out


# roles a mirror reports differently by design (GTK 4 has no "document web"; a text field is "text")
FOCUS_ROLE_EQ = {'entry': 'text', 'password text': 'text', 'push button': 'button', 'document web': 'container',
                 'document text': 'container', 'panel': 'container', 'grouping': 'container'}
ROLE_EQ = {'push button': 'button', 'text': 'text', 'entry': 'text', 'password text': 'text', 'filler': 'panel'}


def tree_compare(kind):
    a = os.path.join(D, f'dump_{kind}_P_app.json')
    p = os.path.join(D, f'dump_{kind}_P_proxy.json')
    if not (os.path.exists(a) and os.path.exists(p)):
        return None
    try:
        app = [n for x in json.load(open(a)) for n in x['nodes']]
        prx = [n for x in json.load(open(p)) for n in x['nodes']]
    except (ValueError, KeyError):
        return None
    keep = {'button', 'push button', 'check box', 'text', 'entry', 'slider', 'radio button', 'toggle button',
            'combo box', 'spin button', 'link', 'menu item', 'page tab', 'list item'}
    A = [n for n in app if n.get('role') in keep and 'showing' in n.get('states', [])]
    P = [n for n in prx if n.get('role') in keep]
    rows = []
    for n in A:
        role = ROLE_EQ.get(n['role'], n['role'])
        cand = [m for m in P if ROLE_EQ.get(m['role'], m['role']) == role and (m.get('name') or '') == (n.get('name') or '')]
        m = cand[0] if cand else None
        if m in P:
            P.remove(m)
        diffs = []
        if m is None:
            diffs.append('missing')
        else:
            for k in ('desc',):
                if (n.get(k) or '') != (m.get(k) or ''):
                    diffs.append(f'{k}: {n.get(k)!r} vs {m.get(k)!r}')
            for st in ('checked', 'focusable', 'sensitive', 'editable'):
                if (st in n.get('states', [])) != (st in m.get('states', [])):
                    diffs.append(f'state {st}: app {st in n.get("states", [])} mirror {st in m.get("states", [])}')
            if (n.get('value') or [None])[0] != (m.get('value') or [None])[0]:
                diffs.append(f'value {n.get("value")} vs {m.get("value")}')
            if (n.get('text') or '') != (m.get('text') or '') and n.get('role') in ('text', 'entry'):
                diffs.append(f'text {n.get("text")!r} vs {m.get("text")!r}')
        rows.append({'role': n['role'], 'name': n.get('name'), 'diffs': diffs,
                     'app_keys': [x[1] for x in n.get('actions') or [] if x[1]][:1] +
                     [(n.get('attrs') or {}).get('keyshortcuts', '')],
                     'mirror_keys': ([x[1] for x in m.get('actions') or [] if x[1]][:1] +
                                     [(m.get('attrs') or {}).get('keyshortcuts', '')]) if m else None})
    return rows


MARKS = read_marks()


def main():
    sp = read_speech()
    marks = sorted(((v['t'], k) for k, v in sp.items()))
    rep = {'run': D, 'kinds': {}}
    md = [f'# parity report: {os.path.basename(D.rstrip("/"))}', '']
    for kind in API + INF + MENU:
        arms = {arm: {k[2]: v for k, v in sp.items() if k[0] == arm and k[1] == kind} for arm in ('X', 'P')}
        if not arms['P'] and not arms['X']:
            continue
        r = {'steps': [], 'effects': {}, 'focus': {}, 'tree': tree_compare(kind)}
        md += [f'## {kind}', '', '| step | arm X (Xwayland) | arm P (proxy) | same words |', '|---|---|---|---|']
        for step in sorted(set(arms['X']) | set(arms['P']), key=lambda s: (arms['P'].get(s) or arms['X'].get(s))['t']):
            x = arms['X'].get(step, {}).get('lines', [])
            p = arms['P'].get(step, {}).get('lines', [])
            same = norm(x) == norm(p)
            ratio = difflib.SequenceMatcher(None, norm(x), norm(p)).ratio()
            r['steps'].append({'step': step, 'X': x, 'P': p, 'same': same, 'similarity': round(ratio, 2)})
            if step == 'end':
                continue
            md.append(f'| {step} | {" / ".join(x)} | {" / ".join(p)} | {"yes" if same else f"no ({ratio:.2f})"} |')
        exp = EXPECT_API if kind in API else EXPECT_INF.get(kind, [])
        by = {arm: effects_by_step(kind, arm, MARKS) for arm in ('X', 'P')}
        bound = all(by[a] is not None for a in ('X', 'P'))
        for arm in ('X', 'P'):
            eff = effects(kind, arm)
            checks = check_effects_bound(by[arm], exp) if bound else check_effects(eff, exp)
            r['effects'][arm] = {'lines': eff, 'checks': checks, 'by_step': by[arm]}
        r['effects']['step_bound'] = bound
        md += ['', f'effects ({"step-bound" if bound else "NOT step-bound: an expected effect anywhere later counts"})',
               '', '| effect | ' + ' | '.join(f'arm {a}' for a in ('X', 'P')) + ' |', '|---|---|---|']
        for i, (label, pat) in enumerate(exp):
            cells = []
            for arm in ('X', 'P'):
                c = r['effects'][arm]['checks'][i]
                cells.append(('PASS ' + (c['got'] or '')) if c['ok'] else 'FAIL')
            md.append(f'| {label} | ' + ' | '.join(cells) + ' |')
        if bound:
            sids = sorted(set(by['X']) | set(by['P']), key=lambda x: int(x[1:]) if x[1:].isdigit() else 99)
            diff = [f'{sid}: X {by["X"].get(sid, [])} vs P {by["P"].get(sid, [])}' for sid in sids
                    if by['X'].get(sid, []) != by['P'].get(sid, [])]
            r['effects']['steps_differing'] = diff
            md += ['', 'every effect per step, arm X vs arm P: ' + ('identical' if not diff else
                                                                     'differs in ' + '; '.join(diff))]
        # focus order of arm P: app (private bus) vs mirrors (desktop bus)
        if arms['P']:
            t0 = min(v['t'] for v in arms['P'].values())
            t1 = max(v['t'] for v in arms['P'].values())
            fa = focus_seq(os.path.join(D, 'events_app.jsonl'), '', t0, t1)
            fp = focus_seq(os.path.join(D, 'events_proxy.jsonl'), 'x11-a11y-proxy', t0, t1)
            r['focus'] = {'app': fa, 'proxy': fp, 'same': (fa == fp) if fa else None}
            md += ['', f'focus order, arm P: app (private bus) {len(fa)} changes, mirrors (desktop bus) {len(fp)}; '
                   f'identical: **{fa == fp}**', '', f'- app: {" > ".join(fa)}', f'- proxy: {" > ".join(fp)}']
        if r['tree']:
            bad = [t for t in r['tree'] if t['diffs']]
            md += ['', f'tree equivalence (interactive nodes): {len(r["tree"]) - len(bad)}/{len(r["tree"])} equal']
            for t in bad:
                md.append(f'- {t["role"]} {t["name"]!r}: {"; ".join(t["diffs"])}')
            md.append('- key bindings (app -> mirror): ' + '; '.join(
                f'{t["name"]!r}: {t["app_keys"]} -> {t["mirror_keys"]}' for t in r['tree'] if any(t['app_keys'])))
        md.append('')
        rep['kinds'][kind] = r
    json.dump(rep, open(os.path.join(D, 'report.json'), 'w'), indent=1)
    open(os.path.join(D, 'report.md'), 'w').write('\n'.join(md) + '\n')
    print('\n'.join(md))


main()
