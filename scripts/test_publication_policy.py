"""Exercise publisher policy against real Git objects, without running proposals."""
import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('policy', ROOT / 'scripts/verify_publication_proposal.py')
policy = importlib.util.module_from_spec(spec); spec.loader.exec_module(policy)


class PublicationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(); self.addCleanup(self.temp.cleanup)
        self.before = Path.cwd(); self.addCleanup(os.chdir, self.before)
        os.chdir(self.temp.name)
        self.git('init', '-q')
        self.git('config', 'user.name', 'Test'); self.git('config', 'user.email', 'test@example.invalid')
        self.path = Path('.github/workflows/check.yml'); self.path.parent.mkdir(parents=True)
        tag_mock = patch.object(policy, 'upstream_tags', return_value={'v2': 'b' * 40, 'v3': 'c' * 40})
        tag_mock.start(); self.addCleanup(tag_mock.stop)
        self.original = 'jobs:\n  build:\n    steps:\n      - uses: owner/action@' + 'a' * 40 + ' # v1\n      - run: echo safe\n'
        self.path.write_text(self.original); Path('Cargo.toml').write_text('version = "1"\n')
        self.commit(); self.base = self.git('rev-parse', 'HEAD').strip().decode()

    def git(self, *args):
        return subprocess.check_output(['git', *args], stderr=subprocess.DEVNULL)

    def commit(self): self.git('add', '--all'); self.git('commit', '-qm', 'test: fixture')
    def check(self, content, accepted, paths=None):
        self.path.write_text(content); self.commit()
        if accepted: policy.validate(self.base, paths or ['.github/workflows/*.yml'])
        else:
            with self.assertRaises(ValueError): policy.validate(self.base, paths or ['.github/workflows/*.yml'])

    def test_sha_and_comment_update_preserves_yaml_and_action_path(self):
        self.check(self.original.replace('a' * 40, 'b' * 40).replace('# v1', '# v2'), True)

    def test_commands_permissions_events_action_paths_and_unpinned_refs_are_rejected(self):
        variants = [self.original.replace('echo safe', 'echo attacker'),
                    self.original.replace('owner/action', 'attacker/action'),
                    self.original.replace('a' * 40, 'main'),
                    'permissions: write-all\n' + self.original,
                    self.original + 'on: push\n',
                    self.original.replace('      - uses:', '      -  uses:')]
        for content in variants:
            with self.subTest(content=content):
                self.git('reset', '--hard', self.base)
                self.check(content, False)

    def test_workflow_add_delete_rename_and_mode_changes_are_rejected(self):
        for kind in ['add', 'delete', 'rename', 'mode']:
            self.git('reset', '--hard', self.base)
            if kind == 'add': self.path.with_name('new.yml').write_text(self.original)
            if kind == 'delete': self.path.unlink()
            if kind == 'rename': self.path.rename(self.path.with_name('renamed.yml'))
            if kind == 'mode': self.path.chmod(0o755)
            self.commit()
            with self.subTest(kind=kind), self.assertRaises(ValueError):
                policy.validate(self.base, ['.github/workflows/*.yml'])

    def test_dependency_file_requires_explicit_caller_allowlist(self):
        Path('Cargo.toml').write_text('version = "2"\n'); self.commit()
        with self.assertRaises(ValueError): policy.validate(self.base, ['.github/workflows/*.yml'])
        policy.validate(self.base, ['Cargo.toml'])

    def test_nested_manifest_is_not_an_exact_root_allowlist_match(self):
        Path('tools').mkdir(); Path('tools/package.json').write_text('{}')
        self.commit()
        with self.assertRaises(ValueError): policy.validate(self.base, ['package.json'])

    def test_unknown_fork_sha_and_tag_comment_mismatch_are_rejected(self):
        for sha, comment in [('d' * 40, 'v2'), ('b' * 40, 'v3')]:
            self.git('reset', '--hard', self.base)
            self.check(self.original.replace('a' * 40, sha).replace('# v1', '# ' + comment), False)

    def test_upstream_resolution_uses_tags_and_peels_annotated_tags(self):
        # Use the unwrapped production function despite the fixture tag stub.
        spec = importlib.util.spec_from_file_location('resolver', ROOT / 'scripts/verify_publication_proposal.py')
        resolver = importlib.util.module_from_spec(spec); spec.loader.exec_module(resolver)
        response = (('a' * 40 + '\trefs/tags/v2\n') +
                    ('b' * 40 + '\trefs/tags/v2^{}\n') +
                    ('d' * 40 + '\trefs/pull/1/head\n')).encode()
        with patch.object(resolver.subprocess, 'run', return_value=SimpleNamespace(stdout=response)) as command:
            self.assertEqual(resolver.upstream_tags('owner/action/path'), {'v2': 'b' * 40})
            args = command.call_args.args[0]
            self.assertIn('refs/tags/*', args); self.assertIn('--tags', args)
            self.assertNotIn('refs/pull/*', args)

    def test_removing_upstream_check_is_detected_by_denial_control(self):
        self.check(self.original.replace('a' * 40, 'd' * 40).replace('# v1', '# v2'), False)
        source = (ROOT / 'scripts/verify_publication_proposal.py').read_text()
        mutant = source.replace('                verify_upstream_pin(right.group(2), right.group(3), right.group(4))', '                pass')
        self.assertNotEqual(mutant, source)
        namespace = {'__name__': 'mutated_policy'}; exec(compile(mutant, '<mutation>', 'exec'), namespace)
        with self.assertRaises(AssertionError):
            with self.assertRaises(ValueError): namespace['validate'](self.base, ['.github/workflows/*.yml'])

    def test_workflow_embeds_reviewed_policy_before_credential_selection(self):
        workflow = (ROOT / '.github/workflows/dependency-health.yml').read_text()
        start = workflow.index("          python3 -I - <<'PY_POLICY'\n")
        embedded = workflow[start:].split('\n', 1)[1].split('          PY_POLICY\n')[0]
        self.assertEqual(''.join(line[10:] + '\n' for line in embedded.splitlines()),
                         (ROOT / 'scripts/verify_publication_proposal.py').read_text())
        self.assertLess(start, workflow.index('      - name: Select a check-triggering publishing credential'))


if __name__ == '__main__': unittest.main()
