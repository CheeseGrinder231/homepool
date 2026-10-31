#!/usr/bin/env python3
"""Persistent engine fixture with independent pending engine-side mutations."""
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time

args = sys.argv[3:]
root = Path(os.environ['HOMEPOOL_FAKE_STATE'])
member = re.search(r'/(\d+)\.sock$', sys.argv[2]).group(1)
state = root / 'engine.json'
if (root / 'stall').exists() or (root / f'stall-{member}').exists():
    descendant = subprocess.Popen(['sleep', '300'])
    (root / 'transport').write_text(f'{os.getpid()} {descendant.pid}')
    time.sleep(300)
if (root / 'unavailable').exists() or (root / f'unavailable-{member}').exists():
    sys.exit(1)
data = json.loads(state.read_text()) if state.exists() else {'next': 1, 'items': {}}
items = data['items']
op = args[1]
if op == 'ls':
    found = [k for k, v in items.items() if v['labels']['homepool.member'] == member]
    if found:
        print('\n'.join(found))
elif op == 'inspect':
    key = args[-1]
    if key not in items:
        key = next((k for k, v in items.items() if v['name'] == key), None)
    if key is None or ((root / 'partial').exists() and key == sorted(items)[-1]):
        sys.exit(1)
    print(json.dumps(key if args[args.index('--format') + 1] == '{{json .Id}}' else items[key]))
elif op == 'logs':
    if (root / 'stall-logs').exists():
        time.sleep(300)
    (root / 'logs-read').touch()
    item = items[args[-1]]
    sys.stdout.buffer.write(bytes(item.get('stdout', [])))
    sys.stderr.buffer.write(bytes(item.get('stderr', [])))
    if (root / 'fail-logs').exists():
        sys.exit(1)
elif op == 'create':
    sys.stdin.buffer.read()
    name = args[args.index('--name') + 1]
    if any(v['name'] == name for v in items.values()):
        sys.exit(1)
    ident = f"{data['next']:064x}"
    data['next'] += 1
    labels = dict(args[i + 1].split('=', 1) for i, a in enumerate(args) if a == '--label')
    record = {'id': ident, 'name': name, 'labels': labels, 'state': 'created', 'exit_code': 0, 'limits': {'nano_cpus': round(float(args[args.index('--cpus') + 1]) * 1_000_000_000), 'cpu_quota': 0, 'cpu_period': 0, 'memory_bytes': int(args[args.index('--memory') + 1])}}
    if (root / 'delay-create').exists():
        state.write_text(json.dumps(data))
        (root / 'pending-create').write_text(json.dumps(record))
        # The test completes this engine-side record independently of this CLI.
        time.sleep(300)
    items[ident] = record
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
if op in ['create', 'start', 'stop', 'rm']:
    state.write_text(json.dumps(data))
if op == 'start' and (root / f'unconfirmed-{member}').exists():
    (root / f'unavailable-{member}').touch()
if op == 'start' and (root / f'uncertain-unavailable-{member}').exists():
    (root / f'unavailable-{member}').touch()
    sys.exit(1)
if op == 'start' and (root / 'uncertain').exists():
    sys.exit(1)
