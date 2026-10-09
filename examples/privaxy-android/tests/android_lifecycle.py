#!/usr/bin/env python3
"""Run after installing Privaxy and enabling capture on an explicitly selected device.

Explored with ARTEMIS/ADB on the S26 Ultra AVD (Android 16). Requires adb and tesseract.
No coordinates: launch the resolved Activity, use Android Home/CLEAR_TASK transitions, and
wait for OCR-confirmed app rendering. The origin must be reachable from the Android device.
Example: python3 tests/android_lifecycle.py --serial emulator-5554 \
    --origin http://10.0.2.2:18760/lifecycle --output /tmp/privaxy-lifecycle
"""
import argparse
import concurrent.futures
import http.client
import json
from pathlib import Path
import re
import subprocess
import threading
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--serial', required=True)
    parser.add_argument('--origin', required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--cycles', type=int, default=5)
    parser.add_argument('--proxy-port', type=int, default=18100)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)

    def adb(*command, binary=False):
        return subprocess.check_output(['adb', '-s', args.serial, *command],
                                       text=not binary, timeout=25)

    assert adb('get-state').strip() == 'device'
    component = adb('shell', 'cmd', 'package', 'resolve-activity', '--brief',
                    'com.privaxy.android').strip().splitlines()[-1]
    assert component.startswith('com.privaxy.android/'), component
    adb('forward', f'tcp:{args.proxy_port}', 'tcp:8100')
    pid = adb('shell', 'pidof', 'com.privaxy.android').strip()
    assert pid and ' ' not in pid, 'Launch Privaxy and enable capture before running this test.'
    since = adb('shell', 'date', '+%m-%dT%H:%M:%S.000').strip().replace('T', ' ')

    def wait_rendered(label):
        deadline = time.monotonic() + 15
        screenshot = args.output / f'{label}.png'
        while time.monotonic() < deadline:
            assert adb('shell', 'pidof', 'com.privaxy.android').strip() == pid, 'App process restarted'
            screenshot.write_bytes(adb('exec-out', 'screencap', '-p', binary=True))
            text = subprocess.check_output(['tesseract', str(screenshot), 'stdout'],
                                           stderr=subprocess.DEVNULL, text=True, timeout=10)
            if 'Privaxy' in text and 'Capturing' in text:
                (args.output / f'{label}.txt').write_text(text)
                return
            time.sleep(0.25)
        raise AssertionError(f'Privaxy did not render with capture active: {label}; OCR={text!r}')

    def request():
        connection = http.client.HTTPConnection('127.0.0.1', args.proxy_port, timeout=5)
        try:
            connection.request('GET', args.origin)
            response = connection.getresponse()
            body = response.read()
            assert response.status == 200 and body, (response.status, len(body))
        finally:
            connection.close()

    wait_rendered('initial')
    request()
    stop = threading.Event()
    successes = []

    def traffic():
        while not stop.is_set():
            request()
            successes.append(time.monotonic())
            stop.wait(0.1)

    start = time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
        future = pool.submit(traffic)
        try:
            for cycle in range(args.cycles):
                adb('shell', 'input', 'keyevent', 'KEYCODE_HOME')
                request()  # Proxy must serve while its UI is backgrounded.
                adb('shell', 'am', 'start', '-W', '-n', component)
                wait_rendered(f'{cycle + 1}-resume')
                adb('shell', 'am', 'start', '-W', '-f', '0x10008000', '-n', component)
                wait_rendered(f'{cycle + 1}-recreate')
                request()
        finally:
            stop.set()
        future.result()
    logs = adb('logcat', '-d', f'--pid={pid}', '-T', since)
    (args.output / 'logcat.txt').write_text(logs)
    assert not re.search(r'egui-android panic|run_native failed|FATAL EXCEPTION|Fatal signal|ANR in com\.privaxy', logs), logs
    result = {'passed': True, 'serial': args.serial, 'pid': pid, 'home_resume_cycles': args.cycles,
              'activity_recreations': args.cycles, 'continuous_proxy_requests': len(successes),
              'elapsed_seconds': round(time.monotonic() - start, 2)}
    (args.output / 'result.json').write_text(json.dumps(result, indent=2))
    print(json.dumps(result))


if __name__ == '__main__':
    main()
