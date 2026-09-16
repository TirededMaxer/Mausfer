"""Create verified drafts, or publish a tested draft and retain only that Release."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile

from package import CLIENT, DIST, SERVER, expected_files

REPO = os.environ.get('GITHUB_REPOSITORY', 'TirededMaxer/Mausfer')


def gh(*args):
    return subprocess.check_output(['gh', *args], text=True)


def api(path, method='GET', data=None):
    command = ['gh', 'api', f'repos/{REPO}/{path}', '--method', method]
    if data is not None:
        command += ['--input', '-']
    result = subprocess.run(command, input=json.dumps(data) if data is not None else None,
                            text=True, capture_output=True, check=True)
    return json.loads(result.stdout) if result.stdout.strip() else None


def releases():
    return json.loads(gh('api', f'repos/{REPO}/releases', '--paginate', '--slurp'))


def find_release(tag):
    return next((r for page in releases() for r in page if r['tag_name'] == tag), None)


def verify(release):
    with tempfile.TemporaryDirectory() as folder:
        gh('release', 'download', release['tag_name'], '--repo', REPO,
           '--pattern', 'SHA256SUMS.txt', '--dir', folder)
        checksums = {}
        for line in (Path(folder) / 'SHA256SUMS.txt').read_text().splitlines():
            digest, name = line.split('  ', 1)
            if not re.fullmatch('[a-f0-9]{64}', digest) or Path(name).name != name or name in checksums:
                raise RuntimeError('Invalid checksum manifest')
            checksums[name] = 'sha256:' + digest
        if set(checksums) != expected_files():
            raise RuntimeError('Release is missing expected platform packages')
        checksums['SHA256SUMS.txt'] = 'sha256:' + hashlib.sha256(
            (Path(folder) / 'SHA256SUMS.txt').read_bytes()).hexdigest()
    assets = release['assets']
    if len(assets) != len(checksums) or {a['name'] for a in assets} != set(checksums):
        raise RuntimeError('Release asset list does not match its checksum manifest')
    for asset in assets:
        if asset['state'] != 'uploaded' or asset.get('digest') != checksums[asset['name']] or asset['size'] <= 0:
            raise RuntimeError(f'Unverified release asset: {asset["name"]}')
    print(f'All {len(assets)} remote assets verified')


def draft(tag):
    sha = os.environ['GITHUB_SHA']
    existing = find_release(tag)
    if existing is None:
        notes = (f'云端构建候选版本，等待实际设备测试确认。\n\n'
                 f'- 客户端版本：{CLIENT}\n- 信令服务版本：{SERVER}\n- 源码提交：`{sha}`\n'
                 f'- 构建记录：https://github.com/{REPO}/actions/runs/{os.environ["GITHUB_RUN_ID"]}\n\n'
                 '附件包含 Windows 便携程序、NSIS/MSI 安装包、macOS ARM64 DMG/ZIP、'
                 'Android 四架构通用 APK，以及服务端 JAR/ZIP、配置和 SHA-256 清单。\n\n'
                 'Android 沿用原发布签名；macOS 为本地签名，未经 Apple 公证；Windows 未作商业代码签名。\n\n'
                 '测试通过后，手动运行 Publish Tested Release，填写本草稿标签。'
                 '该操作会发布为最新版，并在校验成功后删除其他已发布 Release。\n')
        with tempfile.TemporaryDirectory() as folder:
            body = Path(folder) / 'notes.md'
            body.write_text(notes)
            gh('release', 'create', tag, '--repo', REPO, '--draft', '--target', sha,
               '--title', f'Mausfer {tag}', '--notes-file', str(body))
    elif not existing['draft'] or existing['target_commitish'] != sha:
        raise RuntimeError('Tag already belongs to another release or source commit; use a new tag')
    gh('release', 'upload', tag, '--repo', REPO, '--clobber',
       *[str(p) for p in sorted(DIST.iterdir())])
    verify(find_release(tag))
    print(f'Release draft {tag} is ready for testing')


def publish(tag):
    release = find_release(tag)
    if release is None:
        raise RuntimeError('Release draft does not exist')
    verify(release)
    if release['draft']:
        api(f'releases/{release["id"]}', 'PATCH', {'draft': False, 'make_latest': 'true'})
    latest = api('releases/latest')
    if latest['id'] != release['id'] or latest['draft']:
        raise RuntimeError('Latest Release has not switched; older Releases were retained')
    verify(latest)
    for page in releases():
        for older in page:
            if older['id'] != release['id'] and not older['draft']:
                api(f'releases/{older["id"]}', 'DELETE')
                print(f'Removed older Release {older["tag_name"]}')
    print(f'Published https://github.com/{REPO}/releases/tag/{tag}')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=['draft', 'publish'])
    parser.add_argument('tag')
    args = parser.parse_args()
    if not re.fullmatch(r'v[0-9]+\.[0-9]+\.[0-9]+(?:-[A-Za-z0-9.-]+)?', args.tag):
        parser.error('Use a version tag such as v0.1.2')
    {'draft': draft, 'publish': publish}[args.command](args.tag)
