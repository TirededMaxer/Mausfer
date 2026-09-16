"""Collect existing build outputs; component versions come from the source."""
import argparse
import hashlib
from pathlib import Path
import shutil
import subprocess
import tomllib
import xml.etree.ElementTree as ET
import zipfile

ROOT = Path(__file__).resolve().parents[2]
DIST = ROOT / 'dist'
CLIENT = tomllib.loads((ROOT / 'Cargo.toml').read_text())['workspace']['package']['version']
SERVER = ET.parse(ROOT / 'signaling-server/pom.xml').findtext('{*}version')


def expected_files():
    return {
        f'Mausfer-{CLIENT}-windows-x64.exe',
        f'Mausfer-{CLIENT}-windows-x64-setup.exe',
        f'Mausfer-{CLIENT}-windows-x64.msi',
        f'Mausfer-{CLIENT}-macos-arm64.dmg',
        f'Mausfer-{CLIENT}-macos-arm64.zip',
        f'Mausfer-{CLIENT}-android-universal.apk',
        f'Mausfer-{SERVER}-signaling-server.zip',
        f'mausfer-signaling-{SERVER}.jar',
        'config.json',
    }


def copy(source, name):
    shutil.copy2(source, DIST / name)


def only(pattern):
    paths = list(ROOT.glob(pattern))
    if len(paths) != 1:
        raise RuntimeError(f'Expected one output matching {pattern}, found {paths}')
    return paths[0]


def checksum(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def package(component):
    DIST.mkdir(exist_ok=True)
    if component == 'windows':
        base = 'target/x86_64-pc-windows-msvc/release'
        copy(ROOT / base / 'mausfer-windows.exe', f'Mausfer-{CLIENT}-windows-x64.exe')
        copy(only(f'{base}/bundle/nsis/*.exe'), f'Mausfer-{CLIENT}-windows-x64-setup.exe')
        copy(only(f'{base}/bundle/msi/*.msi'), f'Mausfer-{CLIENT}-windows-x64.msi')
    elif component == 'macos':
        base = 'target/aarch64-apple-darwin/release/bundle'
        app = ROOT / base / 'macos/Mausfer.app'
        subprocess.run(['codesign', '--verify', '--deep', '--strict', str(app)], check=True)
        subprocess.run(['ditto', '-c', '-k', '--sequesterRsrc', '--keepParent', str(app),
                        str(DIST / f'Mausfer-{CLIENT}-macos-arm64.zip')], check=True)
        subprocess.run(['bash', str(ROOT / 'platforms/macos/rebuild_dmg.sh'),
                        str(only(f'{base}/dmg/*.dmg')),
                        str(DIST / f'Mausfer-{CLIENT}-macos-arm64.dmg')], check=True)
    elif component == 'android':
        apk = ROOT / 'platforms/android/gen/android/app/build/outputs/apk/universal/release/app-universal-release.apk'
        with zipfile.ZipFile(apk) as archive:
            for abi in ('arm64-v8a', 'armeabi-v7a', 'x86', 'x86_64'):
                archive.getinfo(f'lib/{abi}/libmausfer_android.so')
            if archive.testzip() is not None:
                raise RuntimeError('APK archive is corrupt')
        copy(apk, f'Mausfer-{CLIENT}-android-universal.apk')
    elif component == 'signaling':
        jar = ROOT / 'signaling-server/target/mausfer-signaling.jar'
        with zipfile.ZipFile(jar) as archive:
            if archive.testzip() is not None:
                raise RuntimeError('JAR archive is corrupt')
        copy(jar, f'mausfer-signaling-{SERVER}.jar')
        copy(ROOT / 'signaling-server/config.json', 'config.json')
        with zipfile.ZipFile(DIST / f'Mausfer-{SERVER}-signaling-server.zip', 'w', zipfile.ZIP_DEFLATED) as archive:
            for source, name in [(jar, 'mausfer-signaling.jar'),
                                 (ROOT / 'signaling-server/config.json', 'config.json'),
                                 (ROOT / 'signaling-server/README.md', 'README.md'),
                                 (ROOT / 'LICENSE', 'LICENSE')]:
                archive.write(source, f'Mausfer-{SERVER}-signaling-server/{name}')
    elif component == 'checksums':
        actual = {p.name for p in DIST.iterdir() if p.name != 'SHA256SUMS.txt'}
        if actual != expected_files():
            raise RuntimeError(f'Incomplete package set: missing={expected_files()-actual}, unexpected={actual-expected_files()}')
        (DIST / 'SHA256SUMS.txt').write_text(''.join(
            f'{checksum(DIST / name)}  {name}\n'
            for name in sorted(actual)))
    print(f'Packaged {component}')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('component', choices=['windows', 'macos', 'android', 'signaling', 'checksums'])
    package(parser.parse_args().component)
