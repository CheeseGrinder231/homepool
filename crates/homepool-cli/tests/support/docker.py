#!/usr/bin/env python3
"""Persistent, deterministic Docker protocol fixture; never a real engine."""
import json
import os
from pathlib import Path
import subprocess
import sys
import time

args = sys.argv[3:]
root = Path(os.environ['HOMEPOOL_FAKE_STATE'])
state = root / 'engine.json'
if (root / 'stall').exists():
    descendant = subprocess.Popen(['sleep', '300'])
    (root / 'transport').write_text(f'{os.getpid()} {descendant.pid}')
    time.sleep(300)
if (root / 'unavailable').exists():
    sys.exit(1)
data = json.loads(state.read_text()) if state.exists() else {'next': 1, 'items': {}}
items = data['items']
op = args[1]
if op == 'ls':
    member = next(a.split('=', 2)[2] for a in args if a.startswith('label=homepool.member='))
    found = [k for k, v in items.items() if v['labels']['homepool.member'] == member]
    if found:
        print('\n'.join(found))
elif op == 'inspect':
    if (root / 'partial').exists() and args[-1] == sorted(items)[-1]:
        sys.exit(1)
    print(json.dumps(items[args[-1]]))
elif op == 'create':
    sys.stdin.buffer.read()
    ident = f"{data['next']:064x}"
    data['next'] += 1
    labels = dict(args[i + 1].split('=', 1) for i, a in enumerate(args) if a == '--label')
    items[ident] = {'id': ident, 'labels': labels, 'state': 'created', 'limits': {'nano_cpus': int(float(args[args.index('--cpus') + 1]) * 1_000_000_000), 'cpu_quota': 0, 'cpu_period': 0, 'memory_bytes': int(args[args.index('--memory') + 1])}}
    print(ident)
elif op == 'start':
    items[args[-1]]['state'] = 'running'
elif op == 'stop':
    items[args[-1]]['state'] = 'exited'
elif op == 'rm':
    if (root / 'fail-cleanup').exists():
        sys.exit(1)
    items.pop(args[-1])
else:
    sys.exit(1)
state.write_text(json.dumps(data))
if op == 'start' and (root / 'uncertain').exists():
    sys.exit(1)
