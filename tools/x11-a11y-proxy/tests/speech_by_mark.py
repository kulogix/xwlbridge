#!/usr/bin/env python3
"""Prints Orca's speech (from the Speech Dispatcher logger file speech.log) grouped by test marks."""
import sys, re
for line in open(sys.argv[1], errors='replace'):
    line = line.rstrip('\n')
    m = re.match(r'^(\S+) === MARK (.*)$', line)
    if m: print(f'\n== {m.group(2)}'); continue
    m = re.match(r'^(\S+) (.*)$', line)
    if m: print('   ', m.group(2))
