#!/usr/bin/env python3
"""Interactive PTY fixture: uses the real registered bridge, no model or network."""
import json
import os
import subprocess
import sys
import tty
import uuid

args = sys.argv[1:]
assert '--print' not in args and 'acp' not in args
conversation = args[args.index('--resume') + 1] if '--resume' in args else str(uuid.uuid4())
config = json.load(open(os.path.join(os.environ['HOME'], '.cursor', 'hooks.json')))
tty.setraw(0)
print('Cursor fixture ready', flush=True)

def emit(event, generation, **fields):
    payload = dict(hook_event_name=event, conversation_id=conversation,
                   generation_id=generation, workspace_roots=[os.getcwd()], **fields)
    for handler in config['hooks'][event]:
        subprocess.run(handler['command'], shell=True, input=json.dumps(payload).encode(), check=True)

def turn(prompt):
    generation = str(uuid.uuid4())
    emit('beforeSubmitPrompt', generation, prompt=prompt)
    print('result:' + prompt, flush=True)
    # Deliberately stop before the asynchronous response hook.
    emit('stop', generation, status='completed')
    emit('afterAgentResponse', generation, text='result:' + prompt)

turn(args[args.index('--') + 1])
buffer = b''
while True:
    char = os.read(0, 1)
    if not char:
        break
    if char == b'\r':
        prompt = buffer.replace(b'\x1b[200~', b'').replace(b'\x1b[201~', b'').decode()
        buffer = b''
        turn(prompt)
    else:
        buffer += char
