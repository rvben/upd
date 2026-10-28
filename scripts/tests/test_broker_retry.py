"""Execute the actual publication scripts against a one-use-token broker stub."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = {
    'dependency-health.yml': 'Request publication token',
    'dependency-remediation.yml': 'Request a least-privilege installation token',
}


def script(name, heading):
    step = (ROOT / '.github/workflows' / name).read_text().split(
        f'      - name: {heading}\n', 1)[1].split('        run: |\n', 1)[1]
    lines = []
    for line in step.splitlines():
        if line and not line.startswith('          '):
            break
        lines.append(line[10:])
    return '\n'.join(lines)


CURL = r'''#!/usr/bin/env python3
import json, os, pathlib, sys
args = sys.argv[1:]
root = pathlib.Path(os.environ['FIXTURE'])
output = pathlib.Path(args[args.index('--output') + 1])
if '--get' in args:
    count = root / 'oidc-count'
    n = int(count.read_text()) + 1 if count.exists() else 1
    count.write_text(str(n))
    output.write_text(json.dumps({'value': f'identity-{n}'}))
else:
    assert '--retry' not in args, 'POST retries must get a fresh identity'
    request = json.loads(pathlib.Path(args[args.index('--data-binary')+1][1:]).read_text())
    history = root / 'requests.json'
    requests = json.loads(history.read_text()) if history.exists() else []
    assert request['oidc_token'] not in [r['oidc_token'] for r in requests], 'replayed identity'
    requests.append(request)
    history.write_text(json.dumps(requests))
    statuses = os.environ['STATUSES'].split(',')
    status = statuses[min(len(requests)-1, len(statuses)-1)]
    if status.startswith('curl:'):
        print('000', end='')
        sys.exit(int(status.split(':')[1]))
    output.write_text(json.dumps({'token': 'scoped-token'} if status == '200' else {'error': 'private-upstream-detail'}))
    print(status, end='')
'''


class BrokerRetryTest(unittest.TestCase):
    def check_scenario(self, statuses, expected_attempts, success, changed='true'):
        for workflow, heading in WORKFLOWS.items():
            with self.subTest(workflow=workflow, statuses=statuses), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                bin_dir = root / 'bin'
                bin_dir.mkdir()
                runner = root / 'runner'
                runner.mkdir()
                for name, content in {'curl': CURL, 'sleep': '#!/bin/sh\nexit 0\n'}.items():
                    executable = bin_dir / name
                    executable.write_text(content)
                    executable.chmod(0o755)
                output = root / 'output'
                env = {
                    'PATH': f'{bin_dir}:' + os.environ['PATH'],
                    'FIXTURE': directory, 'STATUSES': statuses,
                    'RUNNER_TEMP': str(runner), 'GITHUB_OUTPUT': str(output),
                    'ACTIONS_ID_TOKEN_REQUEST_TOKEN': 'request-secret',
                    'ACTIONS_ID_TOKEN_REQUEST_URL': 'https://oidc.example.test/token',
                    'BROKER_AUDIENCE': 'test-broker',
                    'BROKER_URL': 'https://broker.example.test/token',
                    'WORKFLOW_FILES_CHANGED': changed,
                }
                result = subprocess.run(['bash', '-c', script(workflow, heading)],
                                        env=env, text=True, capture_output=True, timeout=15)
                self.assertEqual(result.returncode == 0, success, result.stderr)
                requests = json.loads((root / 'requests.json').read_text())
                self.assertEqual(len(requests), expected_attempts)
                self.assertEqual(int((root / 'oidc-count').read_text()), expected_attempts)
                expected = {'contents': 'write', 'pull_requests': 'write'}
                if workflow == 'dependency-health.yml' and changed == 'true':
                    expected['workflows'] = 'write'
                for request in requests:
                    self.assertEqual(request['permissions'], expected)
                self.assertEqual(list(runner.iterdir()), [], 'credential files must be cleaned')
                self.assertNotIn('private-upstream-detail', result.stdout + result.stderr)
                if success:
                    self.assertEqual(output.read_text(), 'token=scoped-token\n')
                    self.assertIn('::add-mask::scoped-token', result.stdout)
                    for n in range(1, expected_attempts + 1):
                        self.assertIn(f'::add-mask::identity-{n}', result.stdout)
                else:
                    self.assertFalse(output.exists(), 'failed requests must not expose a token')

    def test_success_keeps_minimum_permissions(self):
        self.check_scenario('200', 1, True, changed='false')

    def test_transient_failures_get_fresh_identity(self):
        for status in ('429', '500', '502', '503', '504', 'curl:7', 'curl:28', 'curl:56'):
            with self.subTest(status=status):
                self.check_scenario(status + ',200', 2, True)

    def test_retries_are_bounded(self):
        self.check_scenario('504', 3, False)

    def test_security_and_configuration_failures_are_not_retried(self):
        for status in ('400', '401', '403', '409', '422', 'curl:60'):
            with self.subTest(status=status):
                self.check_scenario(status + ',200', 1, False)


if __name__ == '__main__':
    unittest.main()
