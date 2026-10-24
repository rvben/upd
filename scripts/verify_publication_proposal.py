"""Publisher-side policy. This source is embedded verbatim in the trusted workflow."""
import difflib
import os
import functools
from pathlib import PurePosixPath
import re
import subprocess


def git(*args):
    return subprocess.check_output(['git', *args])



@functools.lru_cache(maxsize=128)
def upstream_tags(action):
    repository = '/'.join(action.split('/')[:2])
    # Resolve only upstream tags. Pull refs can contain attacker fork commits.
    result = subprocess.run(['git', '-C', '/', '-c', 'credential.helper=',
        'ls-remote', '--tags', 'https://github.com/' + repository + '.git', 'refs/tags/*'],
        env={'PATH': '/usr/bin:/bin', 'HOME': '/nonexistent',
             'GIT_CONFIG_NOSYSTEM': '1', 'GIT_CONFIG_GLOBAL': '/dev/null',
             'GIT_TERMINAL_PROMPT': '0', 'GIT_ASKPASS': '/bin/false'},
        check=True, capture_output=True, timeout=30)
    if len(result.stdout) > 2 * 1024 * 1024:
        raise ValueError('upstream tag response exceeds bound')
    direct, peeled = {}, {}
    for line in result.stdout.decode('utf-8').splitlines():
        sha, ref = line.split()
        if not re.fullmatch('[0-9a-f]{40}', sha) or not ref.startswith('refs/tags/'):
            continue
        tag = ref.removeprefix('refs/tags/')
        if tag.endswith('^{}'): peeled[tag[:-3]] = sha
        else: direct[tag] = sha
    return {**direct, **peeled}


def verify_upstream_pin(action, sha, comment):
    tag = (comment or '').strip()
    if not tag or upstream_tags(action).get(tag) != sha:
        raise ValueError('action SHA and comment must match an upstream tag')


def validate(base, allowed):
    if not re.fullmatch('[0-9a-f]{40}', base) or not allowed:
        raise ValueError('publication requires a base SHA and caller path allowlist')
    fields = git('diff', '--raw', '-z', '--no-renames', base, 'HEAD').split(b'\0')
    if fields[-1] != b'':
        raise ValueError('invalid Git diff framing')
    fields.pop()
    if not fields:
        raise ValueError('proposal contains no changes')
    for metadata, raw_path in zip(fields[::2], fields[1::2], strict=True):
        path = raw_path.decode('utf-8')
        if not any(PurePosixPath('/' + path).match('/' + pattern) for pattern in allowed):
            raise ValueError('proposal path outside caller allowlist')
        old_mode, new_mode, _, _, status = metadata.decode().lstrip(':').split()
        if new_mode != '100644' or status not in ('M', 'A'):
            raise ValueError('proposal must contain regular files only')
        if not path.startswith('.github/workflows/'):
            continue
        if old_mode != '100644' or status != 'M':
            raise ValueError('workflow creation, deletion or mode change refused')
        old = git('cat-file', 'blob', base + ':' + path).decode('utf-8').splitlines(keepends=True)
        new = git('cat-file', 'blob', 'HEAD:' + path).decode('utf-8').splitlines(keepends=True)
        # Only an existing action reference may change; preserve its YAML prefix
        # and repository/path. Never admit arbitrary workflow source from builds.
        action = re.compile(r'^(\s*(?:-\s*)?uses:\s*)([A-Za-z0-9_.-]+/[A-Za-z0-9_./-]+)@([^\s#]+)(?:[ \t]+#([^\r\n]*))?(\r?\n)?$')
        for op, a, b, c, d in difflib.SequenceMatcher(None, old, new, autojunk=False).get_opcodes():
            if op == 'equal':
                continue
            if op != 'replace' or b - a != d - c:
                raise ValueError('only action-reference replacements are permitted')
            for before, after in zip(old[a:b], new[c:d], strict=True):
                left, right = action.fullmatch(before), action.fullmatch(after)
                if (not left or not right or left.group(1, 2, 5) != right.group(1, 2, 5)
                        or not re.fullmatch('[0-9a-f]{40}', right.group(3))):
                    raise ValueError('workflow change exceeds action-reference policy')
                verify_upstream_pin(right.group(2), right.group(3), right.group(4))


if __name__ == '__main__':
    validate(os.environ['BASE_SHA'], os.environ['ALLOWED_PATHS'].split())
