#!/usr/bin/env python3
"""Build portable executables; the packaged app has no Python/Docker dependency."""
import hashlib
import os
from pathlib import Path
import platform
import shutil
import struct
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
DIST = ROOT / 'dist'
IMAGE = 'lan-drop-cross:rust-1.88-v1'


def run(*args, **kwargs):
    print('+ ' + ' '.join(map(str, args)), flush=True)
    subprocess.run(list(map(str, args)), cwd=ROOT, check=True, **kwargs)


def publish(source, name):
    destination = DIST / name
    temporary = destination.with_suffix(destination.suffix + '.tmp')
    shutil.copy2(source, temporary)
    if not name.endswith('.exe'):
        temporary.chmod(0o755)
    temporary.replace(destination)
    print(f'Built {destination}', flush=True)


def native():
    host = next(line.split(': ', 1)[1] for line in subprocess.check_output(
        ['rustc', '-vV'], text=True, cwd=ROOT).splitlines() if line.startswith('host:'))
    system = {'Darwin': 'macos', 'Windows': 'windows', 'Linux': 'linux'}[platform.system()]
    arch = {'aarch64': 'arm64', 'x86_64': 'x64'}.get(host.split('-')[0], host.split('-')[0])
    suffix = '.exe' if system == 'windows' else ''
    run('cargo', 'build', '--release', '--locked', '--target', host)
    built = ROOT / 'target' / host / 'release' / ('lan-drop' + suffix)
    if system == 'macos':
        # macOS 只产出 .app：裸二进制和 bundle 里的那份完全一样，
        # 需要命令行时用 "LAN Drop.app/Contents/MacOS/lan-drop"。
        bundle_macos(built)
    else:
        publish(built, f'lan-drop-{system}-{arch}{suffix}')


APP_NAME = 'LAN Drop'
BUNDLE_ID = 'dev.landrop.LANDrop'
VERSION = '0.1.0'

INFO_PLIST = f"""<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleName</key><string>{APP_NAME}</string>
<key>CFBundleDisplayName</key><string>{APP_NAME}</string>
<key>CFBundleIdentifier</key><string>{BUNDLE_ID}</string>
<key>CFBundleExecutable</key><string>lan-drop</string>
<key>CFBundleIconFile</key><string>AppIcon</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>{VERSION}</string>
<key>CFBundleVersion</key><string>1</string>
<key>NSHighResolutionCapable</key><true/>
<key>NSSupportsAutomaticGraphicsSwitching</key><true/>
<key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
</dict></plist>
"""


ICON = ROOT / 'assets/app-icon.png'          # 1024×1024 母图，唯一的图标来源
ICON_UI = ROOT / 'assets/app-icon-256.png'   # 嵌进程序，供界面、窗口图标和网页使用
ICON_ICO = ROOT / 'assets/app-icon.ico'      # Windows 可执行文件的资源图标


def derive_icons():
    """从母图派生出界面用的小图和 Windows 的 .ico，两者都提交进仓库，
    这样普通 `cargo build` 不需要跑这个脚本。缩放用 macOS 自带的 sips，
    别的系统上跳过，直接用仓库里已有的派生文件。"""
    if platform.system() != 'Darwin':
        print('Skipping icon derivation (sips is macOS-only); using committed files.', flush=True)
        return
    run('sips', '-s', 'format', 'png', '-Z', 256, ICON, '--out', ICON_UI,
        stdout=subprocess.DEVNULL)
    # .ico 就是一组 PNG 加一张目录表；直接拼字节，不引入图像库。
    sizes = (16, 32, 48, 64, 128, 256)
    frames = []
    for size in sizes:
        frame = DIST / f'.ico-{size}.png'
        run('sips', '-s', 'format', 'png', '-Z', size, ICON, '--out', frame,
            stdout=subprocess.DEVNULL)
        frames.append(frame.read_bytes())
        frame.unlink()
    offset = 6 + 16 * len(sizes)
    header = struct.pack('<HHH', 0, 1, len(sizes))
    directory = b''
    for size, data in zip(sizes, frames):
        # 目录表里 256 记作 0，宽高各占一个字节。
        directory += struct.pack('<BBBBHHII', size % 256, size % 256, 0, 0,
                                 1, 32, len(data), offset)
        offset += len(data)
    ICON_ICO.write_bytes(header + directory + b''.join(frames))
    print(f'Built {ICON_UI} and {ICON_ICO}', flush=True)


def icns(destination):
    """PNG -> .icns，用 macOS 自带的 sips 和 iconutil，不需要额外工具。"""
    source = ICON
    iconset = DIST / 'AppIcon.iconset'
    shutil.rmtree(iconset, ignore_errors=True)
    iconset.mkdir(parents=True)
    for size in (16, 32, 128, 256, 512):
        for scale, suffix in ((1, ''), (2, '@2x')):
            run('sips', '-z', size * scale, size * scale, source,
                '--out', iconset / f'icon_{size}x{size}{suffix}.png',
                stdout=subprocess.DEVNULL)
    run('iconutil', '-c', 'icns', iconset, '-o', destination)
    shutil.rmtree(iconset, ignore_errors=True)


def bundle_macos(binary):
    """裸二进制在 Finder 里双击会拉起 Terminal，也挂不上图标；打成 .app 才正常。"""
    app = DIST / f'{APP_NAME}.app'
    shutil.rmtree(app, ignore_errors=True)
    (app / 'Contents/MacOS').mkdir(parents=True)
    (app / 'Contents/Resources').mkdir(parents=True)
    shutil.copy2(binary, app / 'Contents/MacOS/lan-drop')
    (app / 'Contents/MacOS/lan-drop').chmod(0o755)
    (app / 'Contents/Info.plist').write_text(INFO_PLIST, encoding='utf-8')
    icns(app / 'Contents/Resources/AppIcon.icns')
    # 本地 ad-hoc 签名，仅供自用测试；没有 Developer ID 签名和公证。
    run('codesign', '--force', '--deep', '--sign', '-', app)
    print(f'Built {app}', flush=True)


def cross():
    run('docker', 'info', '--format', '{{.ServerVersion}}')
    exists = subprocess.run(['docker', 'image', 'inspect', IMAGE], stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL).returncode == 0
    if not exists:
        run('docker', 'build', '-f', 'scripts/Dockerfile.cross', '-t', IMAGE, '.')
    for target, output in [('x86_64-pc-windows-gnu', 'lan-drop-windows-x64.exe'),
                           ('x86_64-unknown-linux-gnu', 'lan-drop-linux-x64')]:
        # Dedicated Cargo caches keep host and cross-target artifacts separate.
        run('docker', 'run', '--rm',
            '-v', f'{ROOT}:/src',
            '-v', 'lan-drop-build:/build',
            '-v', 'lan-drop-registry:/usr/local/cargo/registry',
            IMAGE, 'sh', '-ec',
            'cargo build --release --locked --target "$1"; '
            'cp "/build/$1/release/$2" "/src/dist/$3.tmp"; '
            'chmod 755 "/src/dist/$3.tmp"; mv "/src/dist/$3.tmp" "/src/dist/$3"',
            'build', target, 'lan-drop.exe' if output.endswith('.exe') else 'lan-drop', output)


def checksums():
    files = sorted(p for p in DIST.glob('lan-drop-*') if p.is_file() and not p.name.endswith('.tmp'))
    # macOS 只产出 .app，对 bundle 里的可执行文件计校验和。
    app_binary = DIST / f'{APP_NAME}.app/Contents/MacOS/lan-drop'
    if app_binary.is_file():
        files.append(app_binary)
    if not files:
        return
    with (DIST / 'SHA256SUMS').open('w') as manifest:
        for path in files:
            with path.open('rb') as binary:
                digest = hashlib.file_digest(binary, 'sha256').hexdigest() if sys.version_info >= (3, 11) else digest_stream(binary)
            manifest.write(f'{digest}  {path.relative_to(DIST)}\n')


def digest_stream(binary):
    digest = hashlib.sha256()
    for chunk in iter(lambda: binary.read(1024 * 1024), b''):
        digest.update(chunk)
    return digest.hexdigest()


def main():
    os.chdir(ROOT)
    DIST.mkdir(exist_ok=True)
    mode = sys.argv[1] if len(sys.argv) > 1 else 'all'
    if mode not in ('all', 'native', 'cross'):
        raise SystemExit('Usage: build.py [all|native|cross]')
    if mode == 'all' and platform.system() != 'Darwin':
        raise SystemExit('三平台构建需在 macOS 上运行（Apple SDK）。当前系统可使用 task build:native 或 task build:cross。')
    derive_icons()
    if mode in ('all', 'native'):
        if mode == 'all' and platform.machine() != 'arm64':
            run('rustup', 'target', 'add', 'aarch64-apple-darwin')
            run('cargo', 'build', '--release', '--locked', '--target', 'aarch64-apple-darwin')
            bundle_macos(ROOT / 'target/aarch64-apple-darwin/release/lan-drop')
        else:
            native()
    if mode in ('all', 'cross'):
        cross()
    checksums()


if __name__ == '__main__':
    try:
        main()
    except (subprocess.CalledProcessError, FileNotFoundError) as error:
        raise SystemExit(f'构建失败：{error}') from error
