"""Local Claude Desktop adapter. JSONL protocol; stdout is reserved for events.
Laya is an action selector only. AX readback owns success/termination.
"""
import fcntl
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import time

PATTERNS = ['multilingual/rl_agent_config.json', 'multilingual/model.safetensors', 'multilingual/tokenizer/*', 'multilingual/encoder/*']
TRUST_LABELS = ('ワークスペースを信頼', 'Trust workspace', 'Trust this workspace')
REVISION = '1c5edc17a7acd8701df6fc341c0d179f1c62c982'
QUESTION = 'What should I click next?'

def label(node):
    return node['title'] or node['description'] or node['value']

def descendants(nodes, parent):
    ids = {parent['id']}
    result = []
    for node in nodes:
        if node['parent'] in ids:
            ids.add(node['id'])
            result.append(node)
    return result

def one(nodes, what):
    if len(nodes) != 1:
        raise RuntimeError(f'UNRECOGNIZED_UI: expected one {what}, found {len(nodes)}')
    return nodes[0]

def model_state(snapshot):
    nodes = snapshot['nodes']
    menus = [n for n in nodes if n['role'] == 'AXMenu' and label(n).startswith(('モデル:', 'Model:'))]
    if menus:
        picker = one(menus, 'model menu')
        items = [n for n in descendants(nodes, picker) if n['role'] == 'AXMenuItem']
        if not 1 <= len(items) <= 9:
            raise RuntimeError('UNRECOGNIZED_UI: model menu candidate count')
        return label(picker).split(':', 1)[1].strip(), items, True
    picker = one([n for n in nodes if n['role'] == 'AXPopUpButton' and label(n).startswith(('モデル:', 'Model:'))], 'model picker')
    return label(picker).split(':', 1)[1].strip(), [picker], False

def decision_request(snapshot, target):
    current, controls, opened = model_state(snapshot)
    rows = [f'AXMenu: モデル: {current}'] if opened else []
    options = {}
    actions = {}
    for i, node in enumerate(controls):
        key = (label(node).split()[0].lower() if label(node).split()[0].isascii() else 'other_models') if opened else 'open_model'
        if key in actions: key = f'choice_{i}'
        rows.append(f"{node['role']}: {label(node)}" + (' (selected)' if node['value'] == '1' else ''))
        options[key] = f'Click AXMenuItem {label(node)}' if opened else 'Open model selector'
        actions[key] = node
    options['finished'] = 'Do nothing, task complete'
    return {
        'state': f"Select {target}. Current model: {current}. Current AX state: " + '; '.join(rows),
        'question': QUESTION, 'options': options,
    }, actions, current == target and not opened

class Adapter:
    def __init__(self, native, predict):
        self.native = native
        self.predict = predict
        self.pid = None
        self.url = None
        self.last_prompt = None
        self.active = False
        self.before_messages = []
        self.started_at = 0
        self.saw_busy = False

    def ax(self, request):
        result = subprocess.run([self.native], input=json.dumps(request), capture_output=True, text=True, timeout=12)
        try:
            value = json.loads(result.stdout)
        except ValueError as exc:
            raise RuntimeError('AX_HELPER_FAILED: invalid response') from exc
        if result.returncode or 'error' in value:
            raise RuntimeError(value.get('error', 'AX_HELPER_FAILED'))
        return value

    def snapshot(self, allow_confirmation=False):
        state = self.ax({'op': 'snapshot'})
        if self.pid is not None and state['pid'] != self.pid:
            raise RuntimeError('TARGET_CHANGED: Claude restarted')
        nodes = state['nodes']
        if any(label(n) in ('デバイスを確認するために再度サインインしてください', 'Sign in again to verify your device') for n in nodes):
            raise RuntimeError('AUTHENTICATION_REQUIRED: sign in again in Claude; inspect submission before retrying')
        if not allow_confirmation and any(label(n) in TRUST_LABELS for n in nodes):
            raise RuntimeError('WORKSPACE_CONFIRMATION_REQUIRED: confirm the requested folder in Claude, then retry')
        if self.url is not None and self.document_url(state) != self.url:
            raise RuntimeError('TARGET_CHANGED: another conversation is visible')
        return state

    @staticmethod
    def document_url(state):
        docs = [n for n in state['nodes'] if n['role'] == 'AXWebArea' and 'claude.ai' in n['url']]
        return one(docs, 'Claude document')['url']

    def act(self, state, node, op='press', **kw):
        try:
            return self.ax({'op': op, 'target': node, 'pid': state['pid'], 'url': self.document_url(state), **kw})
        except RuntimeError as exc:
            raise RuntimeError(f"AX_ACTION_FAILED: {op} {node['role']} {node['title'] or node['description']}: {exc}") from exc

    def select_model(self, target):
        seen = {}
        for _ in range(6):
            state = self.snapshot()
            request, actions, done = decision_request(state, target)
            if done:
                return
            key = json.dumps(request, sort_keys=True)
            seen[key] = seen.get(key, 0) + 1
            if seen[key] > 2:
                raise RuntimeError('MODEL_SELECTION_STALLED')
            choice = self.predict(request)
            if choice not in actions:
                raise RuntimeError('MODEL_SELECTION_FAILED: premature Finished or invalid action')
            self.act(state, actions[choice])
        raise RuntimeError('MODEL_SELECTION_LIMIT')

    def approve_workspace(self, state, path):
        buttons = [n for n in state['nodes'] if n['role'] == 'AXButton' and label(n) in TRUST_LABELS]
        if not buttons:
            return False
        button = one(buttons, 'workspace confirmation')
        dialog = one([n for n in state['nodes'] if n['id'] == button['parent']], 'workspace dialog')
        contents = descendants(state['nodes'], dialog)
        paths = [label(n) for n in contents if n['role'] == 'AXStaticText' and label(n).startswith('/')]
        if paths != [path]:
            raise RuntimeError('WORKSPACE_MISMATCH: confirmation is not for the requested --cwd')
        request = {'state':f'Open the requested workspace {path}. Current AX state: workspace trust confirmation for {paths[0]}.',
                   'question':QUESTION, 'options':{'trust_workspace':'Trust the requested workspace and continue', 'cancel':'Cancel opening the workspace'}}
        if self.predict(request) != 'trust_workspace':
            raise RuntimeError('WORKSPACE_SELECTION_FAILED: Laya did not choose trust')
        self.act(state, button, trusted_path=path)
        return True

    def prepare(self, path, model):
        initial = self.snapshot(allow_confirmation=True)
        drafts = [n for n in initial['nodes'] if n['role'] == 'AXTextArea']
        if any(n['value'].strip() for n in drafts):
            raise RuntimeError('DRAFT_NOT_EMPTY: preserve or clear the current draft first')
        self.pid = initial['pid']
        pending = any(label(n) in TRUST_LABELS for n in initial['nodes'])
        approved = False
        if pending:
            approved = self.approve_workspace(initial, path)
        else:
            self.ax({'op':'open', 'path':path})
        for _ in range(30):
            time.sleep(.1)
            state = self.snapshot(allow_confirmation=True)
            if any(label(n) in TRUST_LABELS for n in state['nodes']):
                if not approved:
                    approved = self.approve_workspace(state, path)
                continue
            try:
                model_state(state)
                break
            except RuntimeError:
                pass
        self.verify_workspace(path)
        self.select_model(model)
        self.url = self.document_url(self.snapshot())
        if not self.url.rstrip('/').endswith('/epitaxy'):
            raise RuntimeError('UNRECOGNIZED_UI: expected new Code composer')
        self.model = model
        self.path = path

    def verify_workspace(self, path):
        state = self.snapshot()
        name = Path(path).name
        button = one([n for n in state['nodes'] if n['role'] == 'AXPopUpButton' and label(n) == name], 'workspace picker')
        self.act(state, button)
        state = self.snapshot()
        candidates = [n for n in state['nodes'] if n['role'] == 'AXMenuItem' and n['help'] == path and n['value'] == '1']
        selected = one(candidates, 'selected workspace with exact absolute path')
        self.act(state, selected)

    @staticmethod
    def composer(state):
        return one([n for n in state['nodes'] if n['role'] == 'AXTextArea'], 'composer')

    @staticmethod
    def messages(state):
        nodes = state['nodes']
        panes = [n for n in nodes if label(n) in ('プライマリペイン', 'Primary pane')]
        if len(panes) != 1:
            return []
        nodes = descendants(nodes, panes[0])
        roles = {'あなたのメッセージ:':'user', 'Your message:':'user', 'Claudeが返答しました:':'assistant', 'Claude responded:':'assistant'}
        result = []
        ignored = set()
        skip_roles = {'AXButton', 'AXPopUpButton', 'AXTextArea', 'AXTextField', 'AXToolbar', 'AXMenu', 'AXRadioButton'}
        for node in nodes:
            if node['parent'] in ignored or node['role'] in skip_roles:
                ignored.add(node['id'])
                continue
            heading = label(node)
            if node['role'] == 'AXHeading' and not heading:
                heading = ''.join(label(n) for n in descendants(nodes, node) if n['role'] == 'AXStaticText')
            if node['role'] == 'AXHeading' and heading in roles:
                result.append({'role':roles[heading], 'text':''})
                ignored.add(node['id'])
            elif result and node['role'] == 'AXStaticText':
                result[-1]['text'] += label(node) + '\n'
        return result

    def send(self, prompt):
        if self.active:
            raise RuntimeError('BUSY')
        self.verify_workspace(self.path)
        state = self.snapshot()
        current, _, opened = model_state(state)
        if current != self.model or opened:
            raise RuntimeError('MODEL_CHANGED')
        editor = self.composer(state)
        if editor['value'].strip() and editor['value'] != prompt:
            raise RuntimeError('DRAFT_NOT_EMPTY')
        self.before_messages = self.messages(state)
        self.act(state, editor, op='set', expected=editor['value'], text=prompt)
        state = self.snapshot()
        if self.composer(state)['value'] != prompt:
            raise RuntimeError('DRAFT_VERIFICATION_FAILED')
        button = one([n for n in state['nodes'] if n['role'] == 'AXButton' and n['enabled'] and label(n) in ('送信', 'Send', 'Send message', 'メッセージを送信')], 'send button')
        # Claim the execution BEFORE pressing. Never retry after this boundary.
        self.active = True
        self.last_prompt = prompt
        self.started_at = time.monotonic()
        self.saw_busy = False
        self.url = None if self.url.rstrip('/').endswith('/epitaxy') else self.url  # The first send navigates from the new composer to a session.
        self.act(state, button)

    def observe(self):
        state = self.snapshot()
        url = self.document_url(state)
        nodes = state['nodes']
        editors = [n for n in nodes if n['role'] == 'AXTextArea']
        if not editors:
            if time.monotonic() - self.started_at > 30:
                raise RuntimeError('OBSERVATION_UNAVAILABLE: composer did not return; inspect Claude before retrying')
            return None
        editor = one(editors, 'composer')
        busy = any(n['role'] == 'AXButton' and n['enabled'] and label(n) in ('停止', 'Stop', 'Stop generating', '応答を停止') for n in nodes)
        if self.url is None:
            if '/epitaxy' in url and url.rstrip('/').endswith('/epitaxy'):
                if time.monotonic() - self.started_at > 20:
                    raise RuntimeError('SUBMISSION_UNCONFIRMED: inspect Claude; do not resend')
                return None
            # Require both a conversation navigation and the draft to be consumed.
            if not editor['value'].strip():
                self.url = url
                self.saw_busy = busy
                return {'event':'started', 'url':url, 'evidence':'ax_navigation_and_consumed_draft'}
        self.saw_busy |= busy
        messages = self.messages(state)
        if self.saw_busy and not busy and len(messages) >= len(self.before_messages) + 2 and messages[-1]['role'] == 'assistant' and messages[-2]['role'] == 'user' and messages[-2]['text'].strip() == self.last_prompt:
            text = messages[-1]['text'].strip()
            if text:
                self.active = False
                return {'event':'completed', 'text':text, 'evidence':'ax_stop_control_and_speaker_heading'}
        if time.monotonic() - self.started_at > 300:
            raise RuntimeError('OBSERVATION_UNAVAILABLE: no recognized completion evidence; inspect Claude')
        return None

def emit(value):
    print(json.dumps(value, ensure_ascii=False), flush=True)

def main():
    init = json.loads(sys.stdin.readline())
    # Across daemon versions and DLGT_HOME overrides, only one adapter owns this app.
    lock_path = Path.home()/'.dlgt'/'desktop-claude.lock'
    lock_path.parent.mkdir(mode=0o700, exist_ok=True)
    lock = os.open(lock_path, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    os.environ['USE_TF'] = '0'
    os.environ['HF_HUB_OFFLINE'] = '1'
    os.environ['HF_HUB_DISABLE_IMPLICIT_TOKEN'] = '1'
    import laya
    from huggingface_hub import snapshot_download
    root = snapshot_download('convaiinnovations/laya', revision=REVISION, allow_patterns=PATTERNS, local_files_only=True)
    agent = laya.load(root, subfolder='multilingual', device='cpu')
    def predict(request):
        result = agent.predict(request['state'], {'action': {'type':'choice', 'instructions':request['question'], 'criteria':request['options']}})
        return result['answers']['action']['choice']
    adapter = Adapter(sys.argv[1], predict)
    adapter.prepare(init['path'], init['model'])
    emit({'event':'ready'})
    while True:
        if select.select([sys.stdin], [], [], .25)[0]:
            line = sys.stdin.readline()
            if not line:
                return
            request = json.loads(line)
            if request['op'] == 'stop':
                return
            if request['op'] == 'send':
                adapter.send(request['prompt'])
        if adapter.active:
            event = adapter.observe()
            if event:
                emit(event)

if __name__ == '__main__':
    try:
        main()
    except Exception as exc:
        emit({'event':'error', 'message':str(exc)})
        sys.exit(1)
