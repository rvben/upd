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


def sri(seed, algorithm='sha512'):
    """A well-formed integrity string whose digest depends on the seed."""
    return algorithm + '-' + base64.b64encode(getattr(hashlib, algorithm)(seed.encode()).digest()).decode()


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
        self.path_contents = {}
        self.on_refresh = None
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
            location, ref = suffix.removeprefix('/contents/').split('?ref=')
            value = self.path_contents.get((location, ref), self.contents[ref]).encode()
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
            if self.on_refresh:
                self.on_refresh()
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

    def decline(self):
        with self.assertRaises(ns['ManualReview']):
            self.review()
        self.assertEqual(self.approvals, [])

    def run_main(self):
        import os, tempfile
        summary = tempfile.NamedTemporaryFile('r', suffix='.md')
        env = {'GH_REPO': self.repo, 'UPD_RUN_ID': '7', 'GITHUB_STEP_SUMMARY': summary.name}
        old = {k: os.environ.get(k) for k in env}
        os.environ.update(env)
        original = ns['review']
        ns['review'] = lambda repo, run_id: original(repo, run_id, self.api, self.download)
        try:
            ns['main']()
        finally:
            ns['review'] = original
            for k, v in old.items():
                os.environ.pop(k, None) if v is None else os.environ.__setitem__(k, v)
        try:
            return summary.read()
        finally:
            summary.close()

    def test_proposal_needing_a_person_exits_cleanly_with_a_notice(self):
        self.presentation['auto_merge_eligible'] = False
        self.decline()
        written = self.run_main()
        self.assertIn('Manual review required', written)
        self.assertEqual(self.approvals, [])

    def test_new_and_incompatible_files_are_manual_review_not_integrity_failures(self):
        self.files[0]['status'] = 'added'
        self.decline()
        self.files[0]['status'] = 'modified'
        self.contents[self.head] = self.contents[self.head].replace('1.0.1', '2.0.0')
        self.decline()

    def test_integrity_problems_still_fail_the_check_rather_than_decline(self):
        self.changed = True
        with self.assertRaises(ValueError) as caught:
            self.run_main()
        self.assertNotIsInstance(caught.exception, ns['ManualReview'])
        self.assertEqual(self.approvals, [])
        self.base_on_main = False
        self.changed = False
        with self.assertRaises(ValueError) as caught:
            self.run_main()
        self.assertNotIsInstance(caught.exception, ns['ManualReview'])

    def test_a_diverged_base_fails_even_when_the_proposal_needs_a_person(self):
        self.presentation['auto_merge_eligible'] = False
        self.base_on_main = False
        with self.assertRaises(ValueError) as caught:
            self.review()
        self.assertNotIsInstance(caught.exception, ns['ManualReview'])

    def test_a_malformed_file_fails_even_after_a_new_file_needs_a_person(self):
        ns['REVIEW_PATHS'][self.repo] = ['Cargo.lock', 'extra/Cargo.lock']
        self.files = [{'filename': 'extra/Cargo.lock', 'status': 'added', 'sha': 'e' * 40},
                      {'filename': 'Cargo.lock', 'status': 'modified', 'sha': 'd' * 40}]
        self.pr_files = self.files
        self.contents[self.head] = '[[package]\n'
        with self.assertRaises(ValueError) as caught:
            self.review()
        self.assertNotIsInstance(caught.exception, ns['ManualReview'])
        self.assertEqual(self.approvals, [])

    def test_every_manual_review_reason_is_reported(self):
        ns['REVIEW_PATHS'][self.repo] = ['Cargo.lock', 'extra/Cargo.lock']
        self.files = [{'filename': 'extra/Cargo.lock', 'status': 'added', 'sha': 'e' * 40},
                      {'filename': 'Cargo.lock', 'status': 'modified', 'sha': 'd' * 40}]
        self.pr_files = self.files
        self.contents[self.head] = self.contents[self.head].replace('1.0.1', '2.0.0')
        with self.assertRaises(ns['ManualReview']) as caught:
            self.review()
        self.assertIn('new dependency files', str(caught.exception))
        self.assertIn('Cargo.lock', str(caught.exception).split(';')[-1])

    def two_files(self, extra, status):
        ns['REVIEW_PATHS'][self.repo] = ['Cargo.lock', extra]
        self.files = [{'filename': extra, 'status': status, 'sha': 'e' * 40},
                      {'filename': 'Cargo.lock', 'status': 'modified', 'sha': 'd' * 40}]
        self.pr_files = self.files

    def assert_hard_failure(self):
        with self.assertRaises(ValueError) as caught:
            self.review()
        self.assertNotIsInstance(caught.exception, ns['ManualReview'])
        self.assertEqual(self.approvals, [])

    def test_an_added_malformed_file_fails_instead_of_declining(self):
        self.two_files('extra/Cargo.lock', 'added')
        self.path_contents[('extra/Cargo.lock', self.head)] = '[[package]\n'
        self.assert_hard_failure()

    def test_a_modified_unsupported_malformed_file_fails_instead_of_declining(self):
        self.two_files('pyproject.toml', 'modified')
        self.path_contents[('pyproject.toml', self.base)] = '[project]\nname = "a"\n'
        self.path_contents[('pyproject.toml', self.head)] = '[project\nname = "a"\n'
        self.assert_hard_failure()

    def test_the_manual_path_rechecks_the_base_after_the_pull_request_refresh(self):
        self.presentation['auto_merge_eligible'] = False
        # The base is on main at the early lineage check and moves before the refresh.
        def refresh():
            self.base_on_main = False
        self.on_refresh = refresh
        self.assert_hard_failure()

    def test_the_manual_path_compares_the_pull_request_file_set(self):
        self.presentation['auto_merge_eligible'] = False
        self.pr_files = self.files + [{'filename': 'README.md', 'status': 'modified', 'sha': 'f' * 40}]
        self.assert_hard_failure()

    def test_every_integrity_check_runs_on_the_manual_path(self):
        self.presentation['auto_merge_eligible'] = False
        seen = []
        for name in ['verify_lineage', 'verify_pr_integrity', 'verify_contents', 'parse_document']:
            original = ns[name]
            ns[name] = (lambda original, name: lambda *a, **k: (seen.append(name), original(*a, **k))[1])(original, name)
            self.addCleanup(ns.__setitem__, name, original)
        self.decline()
        self.assertEqual(set(seen), {'verify_lineage', 'verify_pr_integrity', 'verify_contents', 'parse_document'})

    def test_the_manual_path_finds_a_pull_request_whose_auto_merge_is_off(self):
        # The publisher switches auto-merge off for a proposal that needs a person.
        self.presentation['auto_merge_eligible'] = False
        self.pr['auto_merge'] = None
        self.pr_files = self.files + [{'filename': 'README.md', 'status': 'modified', 'sha': 'f' * 40}]
        self.assert_hard_failure()
        self.pr_files = self.files
        self.decline()

    def test_approval_still_requires_the_armed_auto_merge(self):
        self.pr['auto_merge'] = None
        self.review()
        self.assertEqual(self.approvals, [])

    def test_a_published_pull_request_is_verified_before_any_exit(self):
        mismatch = self.files + [{'filename': 'README.md', 'status': 'modified', 'sha': 'f' * 40}]
        for armed in (True, False):
            for eligible in (True, False):
                with self.subTest(armed=armed, eligible=eligible):
                    self.approvals = []
                    self.pr['auto_merge'] = {'merge_method': 'squash', 'enabled_by': {'id': ns['UPD_BOT_ID']}} if armed else None
                    self.presentation['auto_merge_eligible'] = eligible
                    self.pr_files = mismatch
                    self.assert_hard_failure()

    def test_an_eligible_proposal_without_armed_auto_merge_is_not_approved(self):
        self.pr['auto_merge'] = None
        self.review()
        self.assertEqual(self.approvals, [])

    def test_an_added_yaml_file_is_an_integrity_failure(self):
        for name in ['.github/actions/setup/action.yml', 'ci/config.yaml']:
            with self.subTest(name=name):
                self.two_files(name, 'added')
                self.path_contents[(name, self.head)] = 'name: [broken\n'
                self.assert_hard_failure()

    def test_go_files_are_parsed_strictly_whatever_their_status(self):
        for name, bad in [('go.mod', 'module example.com/m\nrequire (\n\texample.com/a v1.0.0\n'),
                          ('go.mod', 'bogus directive\n'),
                          ('go.mod', 'module example.org/demo\ngo 1.24 unexpected\n'),
                          ('go.mod', 'module example.org/demo\ntool example.org/a example.org/b\n'),
                          ('go.mod', 'module example.org/demo\nrequire example.org/a v1.0.0 extra\n'),
                          ('go.mod', 'module example.org/demo\ngo (\n\t1.24\n)\n'),
                          ('go.sum', 'example.com/a v1.0.0 h1:aa=\x0bexample.com/a v1.0.0/go.mod h1:bb=\n'),
                          ('go.sum', 'example.com/a v1.0.0 notahash\n')]:
            for status in ['added', 'modified']:
                with self.subTest(name=name, status=status):
                    self.approvals = []
                    self.two_files(name, status)
                    self.path_contents[(name, self.head)] = bad
                    self.path_contents[(name, self.base)] = bad
                    self.assert_hard_failure()

    def test_go_mod_directives_take_the_operands_go_gives_them(self):
        # Shapes checked against `go mod edit -json` from Go 1.27.
        parse = ns['parse_document']
        head = 'module example.org/m\ngo 1.24\n'
        for valid in ['tool (\n\texample.org/cmd/x\n)\n', 'ignore (\n\t./x\n)\n',
                      'exclude example.org/a v1.0.0\n', 'retract [v1.0.0, v1.1.0]\n',
                      'retract (\n\tv1.0.0 // why\n\t[v1.1.0, v1.2.0]\n)\n',
                      'replace example.org/a v1.0.0 => example.org/b v1.1.0\n', 'replace example.org/a => ../a\n',
                      'godebug (\n\tx=1\n)\n', 'require "example.org/a" v1.0.0 // indirect\n']:
            with self.subTest(valid=valid):
                parse('go.mod', head + valid)
        for invalid in ['tool a b\n', 'ignore a b\n', 'exclude example.org/a\n', 'retract v1.0.0 v1.1.0\n',
                        'replace example.org/a =>\n', 'godebug a=1 b=2\n', 'toolchain go1.24.1\ntoolchain go1.24.2\n',
                        'go 1.25\n', 'require (\n\texample.org/a\n)\n']:
            with self.subTest(invalid=invalid):
                with self.assertRaises(ValueError):
                    parse('go.mod', head + invalid)

    def test_a_valid_go_mod_with_a_tool_block_can_still_be_bumped(self):
        check = ns['content_compatible']
        text = 'module m\ngo 1.24\nrequire example.org/a v%s\ntool (\n\texample.org/cmd/x\n)\n'
        self.assertTrue(check('go.mod', text % '1.0.0', text % '1.0.1'))
        self.two_files('go.mod', 'modified')
        self.path_contents[('go.mod', self.base)] = text % '1.0.0'
        self.path_contents[('go.mod', self.head)] = text % '1.0.1'
        self.review()
        self.assertEqual(len(self.approvals), 1)

    def test_go_sum_lines_are_split_on_newlines_only(self):
        parse = ns['parse_document']
        parse('go.sum', 'example.com/a v1.0.0 h1:aa=\r\nexample.com/a v1.0.0/go.mod h1:bb=\r\n')
        for separator in ['\x0b', '\x0c', '\u2028', '\x85', '\xa0']:
            with self.subTest(separator=repr(separator)):
                with self.assertRaises(ValueError):
                    parse('go.sum', f'example.com/a v1.0.0 h1:aa={separator}example.com/a v1.0.0/go.mod h1:bb=\n')

    def test_json_with_non_standard_constants_or_duplicate_keys_fails(self):
        for bad in ['{"a": NaN}', '{"a": Infinity}', '{"a": -Infinity}', '{"a": 1, "a": 2}']:
            with self.subTest(bad=bad):
                self.approvals = []
                self.two_files('package.json', 'modified')
                self.path_contents[('package.json', self.base)] = '{"a": 1}'
                self.path_contents[('package.json', self.head)] = bad
                self.assert_hard_failure()
        self.two_files('docs/data.json', 'added')
        self.path_contents[('docs/data.json', self.head)] = '{"a": NaN}'
        self.assert_hard_failure()

    def test_a_table_that_only_shares_a_dependency_name_is_not_a_dependency(self):
        check = ns['content_compatible']
        old = '[package.metadata.dependencies]\na = "1.0.0"\n'
        self.assertFalse(check('Cargo.toml', old, old.replace('1.0.0', '1.0.1')))
        self.assertFalse(check('Cargo.toml', '[package.metadata.dependencies]\na = { version = "1.0.0" }\n',
                               '[package.metadata.dependencies]\na = { version = "1.0.1" }\n'))
        real = '[target.\'cfg(unix)\'.dependencies]\na = "1.0.0"\n'
        self.assertTrue(check('Cargo.toml', real, real.replace('1.0.0', '1.1.0')))
        ws = '[workspace.dependencies]\na = "1.0.0"\n'
        self.assertTrue(check('Cargo.toml', ws, ws.replace('1.0.0', '1.1.0')))

    def test_equality_keeps_booleans_apart_from_numbers(self):
        check = ns['content_compatible']
        self.assertFalse(check('package.json', '{"private":1,"dependencies":{"a":"^1.0.0"}}',
                               '{"private":true,"dependencies":{"a":"^1.1.0"}}'))
        self.assertFalse(check('package.json', '{"private":false,"dependencies":{"a":"^1.0.0"}}',
                               '{"private":0,"dependencies":{"a":"^1.1.0"}}'))
        self.assertTrue(check('package.json', '{"private":true,"dependencies":{"a":"^1.0.0"}}',
                              '{"private":true,"dependencies":{"a":"^1.1.0"}}'))
        self.assertFalse(check('Cargo.toml', '[package]\nx = 1\n[dependencies]\na = "1.0.0"\n',
                               '[package]\nx = true\n[dependencies]\na = "1.1.0"\n'))

    def test_the_base_branch_is_url_encoded_in_the_pull_request_lookup(self):
        seen = []
        def api(path, payload=None, method="POST"):
            seen.append(path)
            if path == 'repos/' + self.repo:
                return {'default_branch': 'release+stable'}
            return self.api(path.replace('release%2Bstable', 'main').replace('release+stable', 'main'), payload, method)
        self.run['head_branch'] = 'release+stable'
        self.pr['base']['ref'] = 'release+stable'
        original = ns['review']
        try:
            original(self.repo, 7, api, self.download)
        except Exception:
            pass
        lookups = [p for p in seen if '/pulls?' in p]
        self.assertTrue(lookups, seen)
        self.assertTrue(all('base=release%2Bstable&' in p for p in lookups), lookups)

    def test_reviewer_serializes_with_the_dependency_publisher(self):
        publisher = WORKFLOW.with_name('dependency-health.yml').read_text()
        group = '  group: upd-dependency-writes-${{ github.repository }}'
        self.assertIn(group, publisher)
        self.assertIn(group, WORKFLOW.read_text())

    def test_validation_only_keeps_reports_and_skips_publication(self):
        publisher = WORKFLOW.with_name('dependency-health.yml').read_text()
        inputs, jobs = publisher.split('\njobs:\n', 1)
        self.assertRegex(inputs, r'(?m)^      publish:\n        description: [^\n]+\n'
                                 r'        type: boolean\n        default: true$')
        update, publish = jobs.split('\n  publish:\n', 1)
        self.assertRegex(inputs, r'(?m)^permissions:\n  contents: read\n'
                                 r'  packages: read\n  pull-requests: read\n')
        self.assertNotIn('id-token: write', inputs)
        self.assertNotRegex(update, r'(?m)^    permissions:')
        header = publish.split('    steps:\n', 1)[0]
        self.assertRegex(header, r'(?m)^    if: inputs\.publish$')
        self.assertRegex(header, r'(?m)^    needs: update$')
        for name in ('Package validated proposal', 'Transfer validated proposal to isolated publisher'):
            with self.subTest(step=name):
                self.assertIn(f"      - name: {name}\n"
                              "        if: inputs.publish && steps.update.outputs.changed == 'true'\n", update)
        self.assertIn('      - name: Retain machine-readable report\n'
                      '        id: report\n        if: always()\n', update)

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

    def test_uses_text_inside_a_block_scalar_is_not_an_action_bump(self):
        check = ns['content_compatible']
        path = '.github/workflows/ci.yml'
        line = 'example/action@%s # v1.%d.0'

        def workflow(header, body, sha, minor):
            return 'jobs:\n  build:\n    steps:\n' + header + '\n' + body + 'uses: ' + line % (sha * 40, minor) + '\n      - run: true\n'

        real = lambda sha, minor: 'jobs:\n  build:\n    steps:\n      - uses: ' + line % (sha * 40, minor) + '\n      - run: true\n'
        self.assertTrue(check(path, real('a', 0), real('b', 1)))
        for header in ['      - run: |', '      - run: >-', '      - run: |+ # note', '      - name: x\n        run: |2', '      - run: !!str |']:
            body = '          ' if '\n' not in header or True else ''
            self.assertFalse(check(path, workflow(header, body, 'a', 0), workflow(header, body, 'b', 1)), header)
        # A bump next to a block scalar is still a bump.
        sibling = lambda sha, minor: '    steps:\n      - run: |\n          echo\n      - uses: ' + line % (sha * 40, minor) + '\n'
        self.assertTrue(check(path, sibling('a', 0), sibling('b', 1)))
        # Ambiguous layouts fail closed: tabs, multi-line plain and quoted scalars.
        for text in ['\t- uses: ' + line, '      - run: echo a\n          uses: ' + line, "      - name: 'a\n          uses: " + line]:
            self.assertFalse(check(path, text % ('a' * 40, 0), text % ('b' * 40, 1)), text)

    def test_a_block_header_alone_on_its_line_makes_the_workflow_ambiguous(self):
        check = ns['content_compatible']
        path = '.github/workflows/ci.yml'
        use = 'uses: example/action@%s # v1.%d.0\n'
        for head in ['k:\n  |\n  ', 'k:\n  >\n  ', '-\n  |\n  ', 'steps:\n  - run:\n      |\n      ',
                     'steps:\n  - run: !!str\n      |\n      ']:
            self.assertFalse(check(path, head + use % ('a' * 40, 0), head + use % ('b' * 40, 1)), head)
        # A header after a key or a dash on the same line is still understood.
        fine = 'steps:\n  - run: |\n      echo\n  - '
        self.assertTrue(check(path, fine + use % ('a' * 40, 0), fine + use % ('b' * 40, 1)))

    def test_line_separators_other_than_newline_do_not_start_a_new_workflow_line(self):
        check = ns['content_compatible']
        path = '.github/workflows/ci.yml'
        use = 'uses: example/action@%s # v1.%d.0'
        for separator in ['\x85', '\u2028', '\u2029', '\x0b', '\x0c', '\x1c', '\x1d', '\x1e', '\r']:
            text = 'steps:\n  - run: |\n      echo hi' + separator + use + '\n'
            self.assertFalse(check(path, text % ('a' * 40, 0), text % ('b' * 40, 1)), repr(separator))
        # CRLF files keep working when both sides use it.
        crlf = lambda sha, minor: 'steps:\r\n  - ' + use % (sha * 40, minor) + '\r\n'
        self.assertTrue(check(path, crlf('a', 0), crlf('b', 1)))
        self.assertFalse(check(path, crlf('a', 0), crlf('b', 1).replace('\r\n', '\n')))

    def test_a_pin_change_needs_a_strictly_newer_version_written_in_ascii(self):
        check = ns['content_compatible']
        compatible = ns['compatible']
        path = '.github/workflows/ci.yml'
        pin = lambda sha, version: 'steps:\n  - uses: example/action@%s # %s\n' % (sha * 40, version)
        self.assertTrue(check(path, pin('a', 'v1.0.0'), pin('b', 'v1.0.1')))
        # A new commit under the same version comment is not a version bump.
        self.assertFalse(check(path, pin('a', 'v1.0.0'), pin('b', 'v1.0.0')))
        self.assertFalse(check(path, pin('a', 'v1.0.1'), pin('b', 'v1.0.0')))
        self.assertTrue(compatible('1.0.0', '1.0.1'))
        self.assertTrue(compatible('1.0.0', '1.0.0', allow_equal=True))
        self.assertFalse(compatible('1.0.0', '1.0.0'))
        # Build metadata and non-ASCII digits never make a version string acceptable.
        for old, new in [('1.0.0', '1.0.0+evil'), ('1.0.0+a', '1.0.1+a'), ('1.0.0', '1.0.\uff11'), ('1.0.0', '1.0.\u0661'), ('\u0661.0.0', '\u0661.0.1')]:
            self.assertFalse(compatible(old, new), (old, new))
        lock = lambda version: json.dumps({'lockfileVersion': 3, 'packages': {'': {}, 'node_modules/a': {
            'version': version, 'resolved': 'https://registry.npmjs.org/a/-/a-1.0.0.tgz', 'integrity': 'sha512-x'}}})
        self.assertFalse(check('package-lock.json', lock('1.0.0'), lock('1.0.0+evil')))
        manifest = lambda version: json.dumps({'name': 'x', 'dependencies': {'a': version}})
        self.assertTrue(check('package.json', manifest('^1.0.0'), manifest('^1.0.1')))
        self.assertFalse(check('package.json', manifest('^1.0.0'), manifest('^1.0.0+evil')))
        cargo = lambda version: '[package]\nname = "x"\n[dependencies]\na = "%s"\n' % version
        self.assertFalse(check('Cargo.toml', cargo('1.0.0'), cargo('1.0.0+evil')))

    def test_unexpected_document_shapes_decline_instead_of_raising(self):
        check = ns['content_compatible']
        lock = 'package-lock.json'
        record = lambda **fields: json.dumps({'packages': {'': {}, 'node_modules/a': fields}})
        for path, text in [
            (lock, '[]'), (lock, '1'), ('package.json', '1'), ('package.json', '[]'),
            (lock, '{"packages": []}'), (lock, '{"packages": 5}'), (lock, '{"packages": {"": null}}'),
            (lock, '{"packages": {"": {}, "node_modules/a": 5}}'), (lock, '{"packages": {"": {}, "node_modules/a": null}}'),
            (lock, '{"packages": {"": {}}, "dependencies": []}'), (lock, '{"packages": {"": {}}, "dependencies": {"a": 1}}'),
            ('package.json', '{"dependencies": []}'),
            ('Cargo.lock', 'package = [1]'), ('Cargo.lock', '[[package]]\nname = "a"'),
            ('Cargo.toml', '[dependencies]\na = 1'),
        ]:
            if path != 'Cargo.toml':  # an identical manifest is unchanged, whatever its values
                self.assertFalse(check(path, text, text), (path, text))
            self.assertFalse(check(path, text, text.replace('1', '2')), (path, text))
        # An unparsable tarball URL declines like any other URL change.
        good = record(version='1.0.0', resolved='https://registry.npmjs.org/a/-/a-1.0.0.tgz', integrity=sri('1'))
        bad = record(version='1.0.1', resolved='http://[bad', integrity=sri('2'))
        self.assertFalse(check(lock, good, bad))
        self.assertFalse(check(lock, bad, good))

    def test_multiline_flow_collections_do_not_block_unrelated_action_bumps(self):
        check = ns['content_compatible']
        path = '.github/workflows/ci.yml'
        use = '      - uses: example/action@%s # v1.%d.0\n'
        head = 'jobs:\n  t:\n    strategy:\n      matrix:\n        node: %s\n    steps:\n'
        for flow in ['[20,\n          22]', '{a: 1,\n          b: [2,\n            3]}', '[20, # first\n          22]']:
            self.assertTrue(check(path, head % flow + use % ('a' * 40, 0), head % flow + use % ('b' * 40, 1)), flow)
        # A changed line inside a flow collection is not a step, and a quote left open is ambiguous.
        for flow in ['[20,\n          uses: example/action@%s # v1.%d.0]', "['a,\n          b']"]:
            old = head % (flow % ('a' * 40, 0) if '%s' in flow else flow)
            new = head % (flow % ('b' * 40, 1) if '%s' in flow else flow)
            self.assertFalse(check(path, old + use % ('a' * 40, 0), new + use % ('b' * 40, 1)), flow)

    def test_lockfile_tarball_urls_may_only_change_the_version_in_the_file_name(self):
        check = ns['content_compatible']

        def lock(url, version):
            return json.dumps({'lockfileVersion': 3, 'packages': {'': {}, 'node_modules/a': {
                'version': version, 'resolved': url, 'integrity': 'sha512-x'}}})

        npm = 'https://registry.npmjs.org/a/-/a-%s.tgz'
        self.assertTrue(check('package-lock.json', lock(npm % '1.0.0', '1.0.0'), lock(npm % '1.1.0', '1.1.0')))
        scoped = 'https://registry.npmjs.org/@s/a/-/a-%s.tgz'
        self.assertTrue(check('package-lock.json', lock(scoped % '1.0.0', '1.0.0'), lock(scoped % '1.1.0', '1.1.0')))
        # The version also appears in the host or a directory: that is a different source.
        moved = [
            ('https://1.0.0.example.com/a/-/a-1.0.0.tgz', 'https://1.1.0.example.com/a/-/a-1.1.0.tgz'),
            ('https://example.com/a-1.0.0/-/a-1.0.0.tgz', 'https://example.com/a-1.1.0/-/a-1.1.0.tgz'),
            (npm % '1.0.0', 'http://registry.npmjs.org/a/-/a-1.1.0.tgz'),
            (npm % '1.0.0', 'https://registry.npmjs.org/a/-/a-1.1.0.tgz?x=1'),
            (npm % '1.0.0', 'https://registry.npmjs.org:444/a/-/a-1.1.0.tgz'),
            (npm % '1.0.0', 'https://registry.npmjs.org/b/-/a-1.1.0.tgz'),
        ]
        for old, new in moved:
            self.assertFalse(check('package-lock.json', lock(old, '1.0.0'), lock(new, '1.1.0')), (old, new))

    def test_only_require_directives_are_go_dependency_bumps(self):
        check = ns['content_compatible']
        head = 'module example.com/app\n\ngo 1.24\n\n'
        a, b = 'example.com/m v1.0.0', 'example.com/m v1.1.0'
        self.assertTrue(check('go.mod', head + 'require ' + a, head + 'require ' + b))
        self.assertTrue(check('go.mod', head + 'require (\n\t' + a + '\n)', head + 'require (\n\t' + b + '\n)'))
        self.assertTrue(check('go.mod', head + 'require ' + a + ' // indirect', head + 'require ' + b + ' // indirect'))
        for directive in ['exclude', 'retract', 'replace example.com/other =>', 'replace example.com/m v1.0.0 =>']:
            if directive.startswith('replace'):
                old, new = head + directive + ' example.com/m v1.0.0', head + directive + ' example.com/m v1.1.0'
            else:
                old, new = head + directive + ' ' + a.split()[-1], head + directive + ' ' + b.split()[-1]
                if directive == 'exclude':
                    old, new = head + 'exclude ' + a, head + 'exclude ' + b
            self.assertFalse(check('go.mod', old, new), directive)
        self.assertFalse(check('go.mod', head + 'exclude (\n\t' + a + '\n)', head + 'exclude (\n\t' + b + '\n)'))
        self.assertFalse(check('go.mod', head + 'replace (\n\texample.com/m => example.com/m v1.0.0\n)', head + 'replace (\n\texample.com/m => example.com/m v1.1.0\n)'))
        # The directive itself may not change either.
        self.assertFalse(check('go.mod', head + 'require ' + a, head + 'exclude ' + a))

    def test_go_sum_versions_are_compared_by_semantic_order(self):
        check = ns['content_compatible']

        def records(*versions):
            return ''.join('example.com/m %s h1:%s=\n' % (v, 'a' * 43) for v in versions)

        self.assertTrue(check('go.sum', records('v1.9.0'), records('v1.10.0')))
        # Lexically v1.10.0 sorts before v1.8.5, which paired v1.9.0 with v1.8.5.
        self.assertTrue(check('go.sum', records('v1.8.0', 'v1.9.0'), records('v1.8.5', 'v1.10.0')))
        self.assertFalse(check('go.sum', records('v1.8.0', 'v1.9.0'), records('v1.8.5', 'v1.8.1')))
        self.assertFalse(check('go.sum', records('v1.9.0'), records('v2.0.0')))
        # Rows that are unchanged on both sides do not take part in the pairing.
        def rows(*pairs):
            return ''.join('example.com/m %s h1:%s=\n' % (v, c * 43) for v, c in pairs)

        rc = ('v1.1.0-rc.1/go.mod', 'B')
        self.assertTrue(check('go.sum', rows(('v1.0.0/go.mod', 'A'), rc), rows(rc, ('v1.1.0/go.mod', 'C'))))
        self.assertTrue(check('go.sum', rows(('v1.0.0/go.mod', 'A'), ('v1.0.0', 'D'), rc), rows(rc, ('v1.1.0/go.mod', 'C'), ('v1.1.0', 'E'))))
        self.assertFalse(check('go.sum', rows(('v1.0.0/go.mod', 'A'), rc), rows(rc)))
        self.assertFalse(check('go.sum', rows(rc), rows(rc, ('v1.1.0/go.mod', 'C'))))
        self.assertFalse(check('go.sum', rows(('v1.0.0/go.mod', 'A'), rc), rows(rc, ('v1.1.0', 'C'))))

    def test_go_sum_blank_lines_are_not_free_to_appear_or_vanish(self):
        check = ns['content_compatible']
        row = lambda version: 'example.com/a %s h1:%s=\n' % (version, 'A' * 43)
        self.assertTrue(check('go.sum', row('v1.0.0'), row('v1.0.1')))
        self.assertFalse(check('go.sum', row('v1.0.0'), row('v1.0.1') + '\n\n'))
        self.assertFalse(check('go.sum', row('v1.0.0') + '\n', row('v1.0.1')))

    def test_a_lockfile_bump_keeps_a_well_formed_integrity_of_the_same_algorithm(self):
        check = ns['content_compatible']

        def lock(version, **integrity):
            record = {'version': version, 'resolved': 'https://registry.npmjs.org/a/-/a-%s.tgz' % version}
            if integrity:
                record['integrity'] = integrity['value']
            return json.dumps({'lockfileVersion': 3, 'packages': {'': {'dependencies': {'a': '^1.0.0'}}, 'node_modules/a': record}})

        path = 'package-lock.json'
        old = lock('1.0.0', value=sri('1'))
        self.assertTrue(check(path, old, lock('1.0.1', value=sri('2'))))
        self.assertTrue(check(path, lock('1.0.0'), lock('1.0.1')))
        for name, new in {
            'removed': lock('1.0.1'),
            'empty': lock('1.0.1', value=''),
            'weaker algorithm': lock('1.0.1', value=sri('2', 'sha1')),
            'other algorithm': lock('1.0.1', value=sri('2', 'sha256')),
            'truncated': lock('1.0.1', value=sri('2')[:-10]),
            'not a string': lock('1.0.1', value=7),
            'added': None,
        }.items():
            if new is None:
                self.assertFalse(check(path, lock('1.0.0'), lock('1.0.1', value=sri('2'))), name)
            else:
                self.assertFalse(check(path, old, new), name)

    def test_go_sum_rows_must_be_canonical_and_keep_their_markers(self):
        check = ns['content_compatible']
        checksum = lambda seed: 'h1:' + base64.b64encode(hashlib.sha256(seed.encode()).digest()).decode()
        row = lambda version, seed, module='example.com/a', suffix='': '%s %s%s %s\n' % (module, version, suffix, checksum(seed))
        keep = row('v1.0.0', 'keep', 'example.com/b')
        go_sum = lambda *rows: ''.join(rows)
        self.assertTrue(check('go.sum', go_sum(keep, row('v1.0.0', '1')), go_sum(keep, row('v1.0.1', '2'))))
        self.assertTrue(check('go.sum', row('v2.0.0+incompatible', '1'), row('v2.1.0+incompatible', '2')))
        self.assertTrue(check('go.sum', row('v1.0.0', '1', suffix='/go.mod'), row('v1.0.1', '2', suffix='/go.mod')))
        old = go_sum(keep, row('v1.0.0', '1'))
        new = go_sum(keep, row('v1.0.1', '2'))
        refused = {
            'double space': new.replace(' v1.0.1', '  v1.0.1'),
            'tab': new.replace(' v1.0.1', '\tv1.0.1'),
            'CRLF': new.replace('\n', '\r\n'),
            'trailing space': new.replace('\n', ' \n'),
            'leading space': ' ' + new,
            'empty checksum': new.replace(checksum('2'), 'h1:'),
            'short checksum': new.replace(checksum('2'), checksum('2')[:-5] + '='),
            'checksum without padding': new.replace(checksum('2'), checksum('2').rstrip('=')),
            'extra field': new.replace('\n', ' extra\n'),
        }
        for name, text in refused.items():
            self.assertFalse(check('go.sum', old, text), name)
        # An unchanged row is compared byte for byte, so reformatting it is not a bump.
        self.assertFalse(check('go.sum', old, new.replace(keep, keep.replace(' v1.0.0', '  v1.0.0'))))
        self.assertFalse(check('go.sum', old, new.replace(keep, keep.rstrip('\n') + '\r\n')))
        # The +incompatible marker is part of the identity of a row.
        self.assertFalse(check('go.sum', row('v2.0.0', '1'), row('v2.1.0+incompatible', '2')))
        self.assertFalse(check('go.sum', row('v2.0.0+incompatible', '1'), row('v2.1.0', '2')))
        self.assertFalse(check('go.sum', row('v1.0.0+a', '1'), row('v1.0.0+b', '1')))

    def test_a_non_root_lockfile_record_keeps_its_dependency_ranges(self):
        check = ns['content_compatible']

        def lock(version, bar, root='^1.0.0', **extra):
            foo = {'version': version, 'resolved': 'https://registry.npmjs.org/foo/-/foo-%s.tgz' % version,
                   'integrity': sri(version), 'dependencies': {'bar': bar}}
            foo.update(extra)
            return json.dumps({'lockfileVersion': 3, 'packages': {'': {'dependencies': {'foo': root}}, 'node_modules/foo': foo}})

        path = 'package-lock.json'
        self.assertTrue(check(path, lock('1.0.0', '^1.0.0'), lock('1.0.1', '^1.0.0')))
        # The root record keeps its range-bump rule.
        self.assertTrue(check(path, lock('1.0.0', '^1.0.0'), lock('1.0.1', '^1.0.0', root='^1.1.0')))
        self.assertFalse(check(path, lock('1.0.0', '^1.0.0'), lock('1.0.0', '^1.5.0')))
        self.assertFalse(check(path, lock('1.0.0', '^1.0.0'), lock('1.0.1', '^1.5.0')))
        for field in ['optionalDependencies', 'peerDependencies']:
            self.assertFalse(check(path, lock('1.0.0', '^1.0.0', **{field: {'x': '^1.0.0'}}),
                                   lock('1.0.1', '^1.0.0', **{field: {'x': '^1.1.0'}})), field)
        self.assertFalse(check(path, lock('1.0.0', '^1.0.0', engines={'node': '>=18'}), lock('1.0.1', '^1.0.0', engines={'node': '>=12'})))

    def test_lockfile_v2_dependency_tree_follows_the_same_bump_rules(self):
        check = ns['content_compatible']

        def tarball(name, version, host='registry.npmjs.org'):
            return 'https://%s/%s/-/%s-%s.tgz' % (host, name, name, version)

        def lock(lodash, nested, host='registry.npmjs.org', extra=None):
            record = {'version': lodash, 'resolved': tarball('lodash', lodash, host), 'integrity': sri(lodash),
                      'requires': {'inner': '^1.0.0'},
                      'dependencies': {'inner': {'version': nested, 'resolved': tarball('inner', nested), 'integrity': sri(nested)}}}
            record.update(extra or {})
            return json.dumps({'name': 'f', 'lockfileVersion': 2, 'packages': {
                '': {'name': 'f', 'dependencies': {'lodash': lodash}},
                'node_modules/lodash': {'version': lodash, 'resolved': tarball('lodash', lodash), 'integrity': sri(lodash)},
                'node_modules/lodash/node_modules/inner': {'version': nested, 'resolved': tarball('inner', nested), 'integrity': sri(nested)},
            }, 'dependencies': {'lodash': record}})

        path = 'package-lock.json'
        self.assertTrue(check(path, lock('4.17.20', '1.0.0'), lock('4.17.20', '1.0.0')))
        self.assertTrue(check(path, lock('4.17.20', '1.0.0'), lock('4.17.21', '1.0.0')))
        self.assertTrue(check(path, lock('4.17.20', '1.0.0'), lock('4.17.21', '1.1.0')))
        # A record's dependency ranges follow its version, never a bump of their own.
        tree_range = json.loads(lock('4.17.20', '1.0.0'))
        tree_range['dependencies']['lodash']['requires']['inner'] = '^1.5.0'
        self.assertFalse(check(path, lock('4.17.20', '1.0.0'), json.dumps(tree_range)))
        tree_range['dependencies']['lodash'].update(version='4.17.21', resolved=tarball('lodash', '4.17.21'), integrity=sri('4.17.21'))
        self.assertFalse(check(path, lock('4.17.20', '1.0.0'), json.dumps(tree_range)))
        # The legacy tree is held to the same rules as `packages`.
        self.assertFalse(check(path, lock('4.17.20', '1.0.0'), lock('4.17.21', '1.0.0', host='evil.example')))
        self.assertFalse(check(path, lock('4.17.20', '1.0.0'), lock('4.17.21', '2.0.0')))
        self.assertFalse(check(path, lock('4.17.20', '1.0.0'), lock('4.17.21', '1.0.0', extra={'bin': {'x': 'y'}})))
        self.assertFalse(check(path, lock('4.17.20', '1.0.0'), lock('4.17.21', '1.0.0', extra={'requires': {'inner': '^1.0.0', 'new': '^1.0.0'}})))
        # The tree and `packages` must describe the same tarball at every path.
        diverged = json.loads(lock('4.17.20', '1.0.0'))
        diverged['dependencies']['lodash']['version'] = '4.17.21'
        diverged['dependencies']['lodash']['resolved'] = tarball('lodash', '4.17.21')
        diverged['dependencies']['lodash']['integrity'] = sri('other')
        self.assertFalse(check(path, lock('4.17.20', '1.0.0'), json.dumps(diverged)))
        nested = json.loads(lock('4.17.20', '1.0.0'))
        nested['dependencies']['lodash']['dependencies']['inner'].update(version='1.0.1', resolved=tarball('inner', '1.0.1'), integrity=sri('1.0.1'))
        self.assertFalse(check(path, lock('4.17.20', '1.0.0'), json.dumps(nested)))

    def test_go_mod_bumps_keep_incompatible_markers_and_comments_identical(self):
        check = ns['content_compatible']
        head = 'module example.com/app\n\n'
        pair = lambda old, new: check('go.mod', head + old + '\n', head + new + '\n')
        self.assertTrue(pair('require a/b v2.0.0+incompatible', 'require a/b v2.1.0+incompatible'))
        self.assertTrue(pair('require (\n\ta/b v1.0.0 // indirect; kept for compat\n)', 'require (\n\ta/b v1.2.0 // indirect; kept for compat\n)'))
        self.assertTrue(pair('require a/b v1.0.0 // pinned', 'require a/b v1.1.0 // pinned'))
        self.assertTrue(pair('require a/b v1.0.0\r', 'require a/b v1.1.0\r'))
        # Anything but the version token changing is not a bump.
        self.assertFalse(pair('require a/b v2.0.0+incompatible', 'require a/b v2.1.0'))
        self.assertFalse(pair('require a/b v1.0.0', 'require a/b v1.1.0+incompatible'))
        self.assertFalse(pair('require a/b v1.0.0 // pinned', 'require a/b v1.1.0 // unpinned'))
        self.assertFalse(pair('require a/b v1.0.0 // pinned', 'require a/b v1.1.0'))
        self.assertFalse(pair('require a/b v1.0.0', 'require a/b v1.1.0 // new'))
        self.assertFalse(pair('require a/b v2.0.0+incompatible', 'require a/b v3.0.0+incompatible'))

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
