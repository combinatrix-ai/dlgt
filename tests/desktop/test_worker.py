import importlib.util
from pathlib import Path
import unittest
spec = importlib.util.spec_from_file_location('worker', Path(__file__).resolve().parents[2]/'assets/desktop/worker.py')
w = importlib.util.module_from_spec(spec)
spec.loader.exec_module(w)

def node(i, role, text, parent=-1, value=''):
    return dict(id=i, role=role, title=text, description='', value=value, parent=parent, enabled=True, help='', url='')

def closed(model):
    return {'nodes':[node(1,'AXPopUpButton','モデル: '+model)]}

def menu():
    return {'nodes':[node(1,'AXMenu','モデル: Sonnet 5'),node(2,'AXMenuItem','Sonnet 5',1,'1'),node(3,'AXMenuItem','Haiku 4.5',1,'0')]}

class Fake(w.Adapter):
    def __init__(self, states, choose):
        super().__init__('unused', choose)
        self.states = iter(states)
        self.actions = []
    def snapshot(self, **kwargs):
        return next(self.states)
    def act(self, state, n, **kwargs):
        self.actions.append(n['title'])

class ModelPickerTests(unittest.TestCase):
    def test_fixed_goal_and_question_reach_target_in_two_clicks(self):
        requests=[]
        def choose(r):
            requests.append(r)
            return 'haiku' if 'haiku' in r['options'] else 'open_model'
        adapter=Fake([closed('Sonnet 5'),menu(),closed('Haiku 4.5')],choose)
        adapter.select_model('Haiku 4.5')
        self.assertEqual(adapter.actions,['モデル: Sonnet 5','Haiku 4.5'])
        self.assertEqual({r['question'] for r in requests},{w.QUESTION})
        self.assertTrue(all(r['state'].startswith('Select Haiku 4.5.') for r in requests))
    def test_success_is_readback_not_laya_finished(self):
        adapter=Fake([closed('Haiku 4.5')],lambda _:self.fail('must not predict once matched'))
        adapter.select_model('Haiku 4.5')
        self.assertEqual(adapter.actions,[])
    def test_premature_finished_never_claims_success(self):
        adapter=Fake([closed('Sonnet 5')],lambda _:'finished')
        with self.assertRaisesRegex(RuntimeError,'premature Finished'):
            adapter.select_model('Haiku 4.5')
        self.assertEqual(adapter.actions,[])
    def test_repeated_state_is_bounded(self):
        adapter=Fake([closed('Sonnet 5')]*3,lambda _:'open_model')
        with self.assertRaisesRegex(RuntimeError,'STALLED'):
            adapter.select_model('Haiku 4.5')
        self.assertEqual(len(adapter.actions),2)
    def test_trust_requires_exact_requested_path_before_prediction(self):
        state={'nodes':[node(1,'AXGroup','このワークスペースを信頼しますか？'),node(2,'AXStaticText','/tmp/different',1),node(3,'AXButton','ワークスペースを信頼',1)]}
        adapter=Fake([],lambda _:self.fail('path mismatch must not reach Laya'))
        with self.assertRaisesRegex(RuntimeError,'WORKSPACE_MISMATCH'):
            adapter.approve_workspace(state,'/tmp/requested')
        self.assertEqual(adapter.actions,[])
    def test_laya_selects_trust_only_for_matching_dialog(self):
        state={'nodes':[node(1,'AXGroup','このワークスペースを信頼しますか？'),node(2,'AXStaticText','/tmp/requested',1),node(3,'AXButton','ワークスペースを信頼',1)]}
        requests=[]
        def choose(r):
            requests.append(r)
            return 'trust_workspace'
        adapter=Fake([],choose)
        self.assertTrue(adapter.approve_workspace(state,'/tmp/requested'))
        self.assertEqual(adapter.actions,['ワークスペースを信頼'])
        self.assertEqual(requests[0]['question'],w.QUESTION)
    def test_laya_cancel_does_not_grant_access(self):
        state={'nodes':[node(1,'AXGroup','dialog'),node(2,'AXStaticText','/tmp/requested',1),node(3,'AXButton','ワークスペースを信頼',1)]}
        adapter=Fake([],lambda _:'cancel')
        with self.assertRaisesRegex(RuntimeError,'WORKSPACE_SELECTION_FAILED'):
            adapter.approve_workspace(state,'/tmp/requested')
        self.assertEqual(adapter.actions,[])
    def test_ambiguous_pickers_are_rejected(self):
        state=closed('Sonnet 5');state['nodes']+=closed('Opus 5')['nodes']
        with self.assertRaisesRegex(RuntimeError,'expected one'):
            w.decision_request(state,'Haiku 4.5')
    def test_sidebar_and_composer_are_not_responses(self):
        state={'nodes':[node(1,'AXGroup','サイドバー'),node(2,'AXStaticText','old private text',1),node(3,'AXGroup','プライマリペイン'),node(4,'AXHeading','Your message:',3),node(5,'AXStaticText','hello',3),node(6,'AXHeading','Claude responded:',3),node(7,'AXStaticText','answer',3),node(8,'AXTextArea','',3),node(9,'AXStaticText','unsent',8)]}
        self.assertEqual(w.Adapter.messages(state),[{'role':'user','text':'hello\n'},{'role':'assistant','text':'answer\n'}])
    def test_loading_after_send_is_not_a_failed_or_completed_turn(self):
        doc=node(1,'AXWebArea','');doc['url']='https://claude.ai/epitaxy'
        adapter=Fake([{'nodes':[doc]}],lambda _:None)
        adapter.started_at=w.time.monotonic()
        adapter.active=True
        self.assertIsNone(adapter.observe())
        self.assertTrue(adapter.active)
    def test_loading_has_a_bounded_observation_timeout(self):
        doc=node(1,'AXWebArea','');doc['url']='https://claude.ai/epitaxy'
        adapter=Fake([{'nodes':[doc]}],lambda _:None)
        adapter.started_at=w.time.monotonic()-31
        with self.assertRaisesRegex(RuntimeError,'inspect Claude before retrying'):
            adapter.observe()
    def test_reauthentication_is_an_explicit_blocker(self):
        adapter=w.Adapter('unused',lambda _:None)
        adapter.ax=lambda _: {'pid':123,'nodes':[node(1,'AXHeading','デバイスを確認するために再度サインインしてください')]}
        with self.assertRaisesRegex(RuntimeError,'AUTHENTICATION_REQUIRED'):
            adapter.snapshot()
    def test_unknown_ui_does_not_become_completed_text(self):
        self.assertEqual(w.Adapter.messages({'nodes':[node(1,'AXStaticText','looks like answer')]}),[])

if __name__ == '__main__': unittest.main()
