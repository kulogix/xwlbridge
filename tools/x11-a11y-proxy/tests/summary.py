#!/usr/bin/env python3
"""One line per toolkit from a parity report.json: speech steps with the same words (arm X vs P), mean word
similarity, effects passed per arm, focus order (app vs mirrors) identical, tree equivalence. Usage: summary.py RUN"""
import json, sys
r = json.load(open(sys.argv[1] + '/report.json'))
print('| toolkit | speech: steps with the same words | mean similarity | effects arm X | effects arm P | focus order app = mirrors | tree (interactive nodes) |')
print('|---|---|---|---|---|---|---|')
for k, v in r['kinds'].items():
    steps = [s for s in v['steps'] if s['step'] not in ('end',) and not s['step'].startswith('S0') and 'pointer' not in s['step']]
    same = sum(1 for s in steps if s['same'])
    sim = sum(s['similarity'] for s in steps) / max(1, len(steps))
    ex = v['effects']['X']['checks']; ep = v['effects']['P']['checks']
    tree = v.get('tree') or []
    print(f"| {k} | {same}/{len(steps)} | {sim:.2f} | {sum(c['ok'] for c in ex)}/{len(ex)} | {sum(c['ok'] for c in ep)}/{len(ep)} | "
          f"{ {True: 'yes', False: 'NO', None: 'n/a (no app tree)'}[v.get('focus', {}).get('same')] } ({len(v.get('focus', {}).get('proxy', []))} changes) | "
          f"{sum(1 for t in tree if not t['diffs'])}/{len(tree)} |")
