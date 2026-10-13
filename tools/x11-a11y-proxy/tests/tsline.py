#!/usr/bin/env python3
"""Copies stdin to RAW (unchanged) and to TS with a wall-clock prefix 'HH:MM:SS.ffffff ' per line, so report.py can
bind each app effect to the step whose mark precedes it. Usage: cmd | tsline.py RAW TS"""
import sys
from datetime import datetime
raw = open(sys.argv[1], 'a', buffering=1)
ts = open(sys.argv[2], 'a', buffering=1)
for line in iter(sys.stdin.buffer.readline, b''):
    s = line.decode('utf-8', 'replace')
    raw.write(s)
    ts.write(datetime.now().strftime('%H:%M:%S.%f ') + s)
