"""Exercise the privileged workflow program without GitHub access."""
import base64
import hashlib
import json
from pathlib import Path
import unittest

WORKFLOW = Path(__file__).resolve().parents[2] / '.github/workflows/review-dependencies.yml'
source = WORKFLOW.read_text().split("          python3 - <<'PY_REVIEW'\n", 1)[1].rsplit('          PY_REVIEW', 1)[0]
ns = {'__name__': 'review_under_test'}
exec(compile('\n'.join(line[10:] for line in source.splitlines()), str(WORKFLOW), 'exec'), ns)


class ReviewPolicy(unittest.TestCase):
    def setUp(self):
        self.base, self.head, self.repo = 'a' * 40, 'b' * 40, 'example/widgets'
        self.run = dict(status='completed', conclusion='success', event='workflow_dispatch',
                        head_repository={'full_name': self.repo}, head_branch='main',
                        path='.github/workflows/dependencies.yml', head_sha=self.base,
                        referenced_workflows=[{'path': 'rvben/upd/.github/workflows/dependency-health.yml@' + ns['TRUSTED_UPD_SHA'], 'sha': ns['TRUSTED_UPD_SHA']}])
        self.pr = dict(number=1, state='open', draft=False, user={'id': ns['UPD_BOT_ID'], 'type': 'Bot'},
                       base={'ref': 'main', 'repo': {'full_name': self.repo}},
                       head={'ref': 'deps/upd', 'repo': {'full_name': self.repo}, 'sha': self.head},
                       auto_merge={'merge_method': 'squash', 'enabled_by': {'id': ns['UPD_BOT_ID']}})
        self.presentation = dict(auto_merge_requested=True, auto_merge_eligible=True)
        self.bundle = f'# v2 git bundle\n-{self.base} base\n{self.head} HEAD\n\nPACK'.encode()
        ns['REVIEW_PATHS'][self.repo] = ['Cargo.lock']
        self.approvals = []
        self.dismissals = []
        self.changed_after_approval = False
        self.parents = [self.base]
        self.files = [{'filename': 'Cargo.lock', 'status': 'modified', 'sha': 'd' * 40}]
        self.pr_files = self.files
        self.base_on_main = True
        self.stale_dismissal = True
        self.old_reviews = []
        self.publisher_ok, self.changed = True, False
        self.contents = {self.base: 'version = 4\n[[package]]\nname = "test"\nversion = "1.0.0"\nchecksum = "old"\n', self.head: 'version = 4\n[[package]]\nname = "test"\nversion = "1.0.1"\nchecksum = "new"\n'}

    def api(self, path, payload=None, method="POST"):
        if payload is not None:
            if method == "PUT":
                self.dismissals.append(payload)
            else:
                self.approvals.append(payload)
            return {'id': 123, 'commit_id': self.head, 'state': 'APPROVED'}
        suffix = path.removeprefix('repos/' + self.repo)
        if not suffix:
            return {'default_branch': 'main'}
        if suffix.startswith('/rules/branches/'):
            return [{'type': 'pull_request', 'parameters': {'dismiss_stale_reviews_on_push': self.stale_dismissal, 'required_approving_review_count': 1}}, {'type': 'required_status_checks', 'parameters': {'strict_required_status_checks_policy': True, 'required_status_checks': [{'context': 'test', 'integration_id': 15368}]}}]
        if suffix == '/git/ref/heads/main':
            return {'object': {'sha': self.base}}
        if suffix.startswith('/compare/'):
            return {'status': 'identical' if self.base_on_main else 'diverged', 'merge_base_commit': {'sha': self.base}}
        if suffix == '/pulls/1/files?per_page=100':
            return self.pr_files
        if suffix.startswith('/pulls/1/reviews?'):
            return self.old_reviews
        if suffix.startswith('/contents/'):
            value = self.contents[suffix.split('?ref=')[1]].encode()
            return {'encoding': 'base64', 'size': len(value), 'content': base64.b64encode(value).decode()}
        if suffix.startswith('/commits/'):
            return {'sha': self.head, 'parents': [{'sha': p} for p in self.parents], 'files': self.files}
        if suffix == '/actions/runs/7':
            return self.run
        if '/jobs?' in suffix:
            return {'jobs': [dict(name='update / publish', conclusion='success' if self.publisher_ok else 'failure', steps=[{'name': n, 'conclusion': 'success'} for n in ['Enforce publisher proposal policy', 'Publish rolling pull request']])]}
        if '/artifacts?' in suffix:
            return {'artifacts': [dict(name=n, id=i, expired=False, workflow_run={'id': 7}) for i, n in enumerate(['upd-proposal', 'upd-report'])]}
        if suffix.startswith('/pulls?'):
            return [self.pr]
        if suffix == '/pulls/1':
            return {**self.pr, 'head': {**self.pr['head'], 'sha': 'c' * 40}} if self.changed or (self.changed_after_approval and self.approvals) else self.pr
        raise AssertionError(path)

    def download(self, repo, artifact):
        if artifact['name'] == 'upd-report':
            return {'upd-presentation.json': json.dumps(self.presentation).encode()}
        return {'upd-proposal.bundle': self.bundle, 'upd-proposal.bundle.sha256': (hashlib.sha256(self.bundle).hexdigest() + '  upd-proposal.bundle\n').encode()}

    def review(self):
        ns['review'](self.repo, 7, self.api, self.download)

    def reject(self):
        with self.assertRaises(ValueError):
            self.review()
        self.assertEqual(self.approvals, [])

    def test_reviewer_serializes_with_the_dependency_publisher(self):
        publisher = WORKFLOW.with_name('dependency-health.yml').read_text()
        group = '  group: upd-dependency-writes-${{ github.repository }}'
        self.assertIn(group, publisher)
        self.assertIn(group, WORKFLOW.read_text())

    def test_tag_named_main_cannot_smuggle_an_unreviewed_parent(self):
        self.base_on_main = False
        self.reject()

    def test_pull_request_must_contain_only_the_validated_diff(self):
        self.pr_files = self.files + [{'filename': 'src/attack.rs', 'status': 'added', 'sha': 'e' * 40}]
        self.reject()

    def test_stale_review_dismissal_is_required(self):
        self.stale_dismissal = False
        self.reject()

    def test_older_approval_is_withdrawn_even_when_new_report_is_ineligible(self):
        self.old_reviews = [{'id': 123, 'user': {'login': 'github-actions[bot]'}, 'state': 'APPROVED', 'commit_id': 'c' * 40}]
        self.presentation['auto_merge_eligible'] = False
        self.reject()
        self.assertEqual(len(self.dismissals), 1)

    def test_forged_eligibility_cannot_approve_actual_breaking_files(self):
        self.contents[self.head] = self.contents[self.head].replace('1.0.1', '2.0.0')
        self.reject()

    def test_manifest_changes_cannot_add_scripts_or_switch_sources(self):
        check = ns['content_compatible']
        self.assertTrue(check('package.json', '{"dependencies":{"a":"^1.0.0"}}', '{"dependencies":{"a":"^1.1.0"}}'))
        self.assertFalse(check('package.json', '{"dependencies":{"a":"^1.0.0"}}', '{"dependencies":{"a":"^2.0.0"}}'))
        self.assertFalse(check('package.json', '{}', '{"scripts":{"install":"anything"}}'))
        self.assertFalse(check('Cargo.toml', '[dependencies]\na = { version="1.0.0", path="old" }', '[dependencies]\na = { version="1.0.1", path="new" }'))
        self.assertTrue(check('Cargo.toml', '[dependencies]\na = { version="1.0.0", features=["std"] }', '[dependencies]\na = { version="1.1.0", features=["std"] }'))
        self.assertFalse(check('Cargo.toml', '[package]\nversion="1.0.0"', '[package]\nversion="1.0.1"'))

    def test_action_changes_must_keep_source_and_compatible_tag(self):
        check = ns['content_compatible']
        a = '  - uses: example/action@' + 'a' * 40 + ' # v1.0.0'
        b = '  - uses: example/action@' + 'b' * 40 + ' # v1.1.0'
        self.assertTrue(check('.github/workflows/ci.yml', a, b))
        self.assertFalse(check('.github/actions/setup/action.yml', a, b))
        self.assertFalse(check('.github/workflows/ci.yml', a, b.replace('v1.1.0', 'v2.0.0')))
        self.assertFalse(check('.github/workflows/ci.yml', a, b + '\n  - run: anything'))
        self.assertFalse(check('.github/workflows/ci.yml', a, b.replace('example/action', 'other/action')))

    def test_unknown_new_files_and_unsupported_formats_require_review(self):
        self.files[0]['status'] = 'added'
        self.reject()
        self.assertFalse(ns['content_compatible']('pyproject.toml', '', '[project]'))

    def test_version_boundaries_and_prereleases_fail_closed(self):
        check = ns['compatible']
        for a, b in [('1.2.3', '2.0.0'), ('0.2.3', '0.3.0'), ('0.0.1', '0.0.2'), ('1.2.3', '1.2.2'), ('1.2.3', '1.2.4-rc.1'), ('^1.2.3', '~1.2.4')]:
            self.assertFalse(check(a, b), (a, b))
        self.assertTrue(check('^1.2.3', '^1.3.0'))

    def test_approval_is_bound_to_validated_commit(self):
        self.review()
        self.assertEqual(self.approvals[0]['commit_id'], self.head)
        self.assertEqual(self.approvals[0]['event'], 'APPROVE')

    def test_breaking_or_unclassified_evidence_is_rejected(self):
        for value in [False, None, 'true']:
            with self.subTest(value=value):
                self.presentation['auto_merge_eligible'] = value
                self.reject()

    def test_untrusted_revision_is_rejected(self):
        self.run['referenced_workflows'][0]['sha'] = 'c' * 40
        self.reject()

    def test_fork_run_and_non_default_branch_are_rejected(self):
        self.run['head_repository']['full_name'] = 'attacker/widgets'
        self.reject()
        self.run['head_repository']['full_name'] = self.repo
        self.run['head_branch'] = 'feature'
        self.reject()

    def test_failed_isolated_publisher_is_rejected(self):
        self.publisher_ok = False
        self.reject()

    def test_checksum_tampering_is_rejected(self):
        files = self.download(self.repo, {'name': 'upd-proposal'})
        files['upd-proposal.bundle'] += b'tampered'
        with self.assertRaises(ValueError):
            ns['proposal_head'](files, self.base)

    def test_proposal_for_different_base_is_rejected(self):
        self.bundle = self.bundle.replace(self.base.encode(), b'c' * 40)
        self.reject()

    def test_racing_replacement_is_not_approved(self):
        self.changed = True
        self.reject()

    def test_disabled_auto_merge_and_fork_pr_are_ignored(self):
        self.pr['auto_merge'] = None
        self.review()
        self.pr['auto_merge'] = {'merge_method': 'squash', 'enabled_by': {'id': ns['UPD_BOT_ID']}}
        self.pr['head']['repo']['full_name'] = 'attacker/widgets'
        self.review()
        self.assertEqual(self.approvals, [])

    def test_unrelated_or_merge_commit_is_rejected(self):
        self.parents = [self.base, 'c' * 40]
        self.reject()

    def test_unscoped_source_change_is_rejected(self):
        self.files = [{'filename': 'src/malicious.rs', 'status': 'added'}]
        self.reject()

    def test_owner_armed_auto_merge_does_not_replace_trusted_eligibility(self):
        self.pr['auto_merge']['enabled_by']['id'] = 1
        self.review()
        self.assertEqual(self.approvals, [])

    def test_push_after_post_dismisses_the_approval(self):
        self.changed_after_approval = True
        with self.assertRaises(ValueError):
            self.review()
        self.assertEqual(len(self.dismissals), 1)

    def test_future_revision_requires_a_published_stable_guard_release(self):
        ref = {'path': 'rvben/upd/.github/workflows/dependency-health.yml@' + 'c' * 40, 'sha': 'c' * 40}
        def upstream(path):
            if '/tags?' in path:
                return [{'name': 'v0.18.3', 'commit': {'sha': 'c' * 40}}]
            return {'tag_name': 'v0.18.3', 'draft': False, 'prerelease': False, 'published_at': '2026-11-03'}
        self.assertTrue(ns['trusted_revision'](upstream, ref))
        self.assertFalse(ns['trusted_revision'](lambda path: [], ref))
        self.assertFalse(ns['trusted_revision'](lambda path: [{'name': 'v0.18.1', 'commit': {'sha': 'c' * 40}}], ref))

    def test_immediate_successful_merge_does_not_dismiss_approval(self):
        original = self.api
        def merging_api(path, payload=None, method="POST"):
            result = original(path, payload, method)
            if path.endswith('/pulls/1') and self.approvals:
                return {**result, 'state': 'closed', 'merged': True, 'auto_merge': None}
            return result
        ns['review'](self.repo, 7, merging_api, self.download)
        self.assertEqual(self.dismissals, [])

    def test_another_author_is_not_approved(self):
        self.pr['user']['id'] = 1
        self.review()
        self.assertEqual(self.approvals, [])


if __name__ == '__main__':
    unittest.main()
