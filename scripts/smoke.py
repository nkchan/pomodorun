"""Noninteractive app/process smoke test. Does not save a token or change Slack."""
import pathlib
import plistlib
import sqlite3
import subprocess
import time

root = pathlib.Path(__file__).resolve().parents[1]
bundle = root / 'src-tauri/target/release/bundle/macos/Pomodorun.app'
binary = bundle / 'Contents/MacOS/pomodorun'
with (bundle / 'Contents/Info.plist').open('rb') as f:
    info = plistlib.load(f)
assert info['LSUIElement']
print(subprocess.check_output(['file', str(binary)], text=True).strip())
process = subprocess.Popen([str(binary)], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
try:
    time.sleep(3)
    assert process.poll() is None, 'App exited during launch'
    print('App alive after 3s. Parent PID / CPU% / RSS KiB / elapsed:')
    print(subprocess.check_output(['ps', '-p', str(process.pid), '-o', 'pid=,%cpu=,rss=,etime='], text=True).strip())
    second = subprocess.Popen([str(binary)], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        second.communicate(timeout=5)
        assert second.returncode == 0, 'Second process did not exit successfully'
        assert process.poll() is None, 'First process exited'
        print('Single-instance: second process exited, first process survived.')
    finally:
        if second.poll() is None:
            second.terminate()
            second.communicate(timeout=5)
    path = pathlib.Path.home() / 'Library/Application Support' / info['CFBundleIdentifier'] / 'pomodorun.sqlite3'
    with sqlite3.connect(f'file:{path}?mode=ro', uri=True) as db:
        assert db.execute('PRAGMA integrity_check').fetchone()[0] == 'ok'
        print('SQLite integrity: ok, schema version:', db.execute('PRAGMA user_version').fetchone()[0])
    print('Data path:', path)
finally:
    if process.poll() is None:
        process.terminate()
    _, error = process.communicate(timeout=10)
    # Do not print arbitrary runtime output that could contain third-party details.
    print('App stderr empty:', not error)
