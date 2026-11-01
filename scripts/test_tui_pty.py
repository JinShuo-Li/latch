#!/usr/bin/env python3
"""Exercise the real Unix TUI through a controlling PTY and a loopback provider.

Install scripts/requirements-tui.txt; pass a built latch binary. Captures contain
only isolated mock data, and include reconstructed screens rather than ANSI grep.
"""
import argparse
import errno
import fcntl
import html
import json
import os
from pathlib import Path
import platform
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# A fresh helper acquires the controlling terminal before exec. Avoid Python
# preexec_fn / pty.fork in a parent that runs the mock provider's threads.
if len(sys.argv) > 1 and sys.argv[1] == '--exec':
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)
    os.chdir(sys.argv[4])
    os.execv(sys.argv[2], [sys.argv[2], '--config', sys.argv[3]])

import pyte  # noqa: E402; test-only dependency, not needed by the exec helper


class Provider(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self):
        super().__init__(('127.0.0.1', 0), Handler)
        self.requests = []
        self.errors = []


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        try:
            request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            self.server.requests.append(request)
            index = len(self.server.requests)
            if index == 1:
                delta = {'tool_calls': [{'index': 0, 'id': 'pty-call', 'type': 'function',
                         'function': {'name': 'shell', 'arguments': json.dumps(
                             {'command': 'cat proof.txt'})}}]}
                finish = 'tool_calls'
            else:
                delta = {'content': 'PTY reply one verified' if index == 2 else 'PTY reply two verified'}
                finish = 'stop'
            events = [{'choices': [{'delta': delta, 'finish_reason': None}]},
                      {'choices': [{'delta': {}, 'finish_reason': finish}]}]
            body = ''.join('data: ' + json.dumps(event) + '\n\n' for event in events)
            body = (body + 'data: [DONE]\n\n').encode()
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.send_header('Content-Length', str(len(body)))
            self.send_header('Connection', 'close')
            self.end_headers()
            self.wfile.write(body)
        except Exception as error:
            self.server.errors.append(repr(error))


class Screen(pyte.Screen):
    def __init__(self, master):
        super().__init__(120, 36)
        self.master = master

    def write_process_input(self, data):
        # Respond to real terminal queries, including cursor position reports.
        os.write(self.master, data.encode())


class Terminal:
    def __init__(self, binary, config, workspace, output):
        self.output = output
        self.raw = bytearray()
        self.child = None
        self.master, self.slave = os.openpty()
        self.original = termios.tcgetattr(self.slave)
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack('HHHH', 36, 120, 0, 0))
        self.screen = Screen(self.master)
        self.stream = pyte.ByteStream(self.screen)
        env = os.environ.copy()
        for name in ('RUST_LOG', 'LATCH_MOUSE_CAPTURE'):
            env.pop(name, None)
        env.update(TERM='xterm-256color', COLORTERM='truecolor', MOCK_API_KEY='pty-fixture-key')
        try:
            self.child = subprocess.Popen(
                [sys.executable, str(Path(__file__).resolve()), '--exec', str(binary),
                 str(config), str(workspace)], env=env, stdin=self.slave,
                stdout=self.slave, stderr=self.slave, start_new_session=True)
        except BaseException:
            os.close(self.master)
            os.close(self.slave)
            raise

    @property
    def text(self):
        return '\n'.join(self.screen.display)

    def pump(self, timeout=0.1):
        if select.select([self.master], [], [], timeout)[0]:
            try:
                data = os.read(self.master, 65536)
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
                return
            self.raw.extend(data)
            self.stream.feed(data)

    def wait(self, predicate, label, timeout=30):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.pump()
            if predicate():
                return
            if self.child.poll() is not None:
                raise AssertionError(f'TUI exited before {label}: {self.child.returncode}\n{self.text}')
        raise AssertionError(f'Timed out waiting for {label}\n{self.text}')

    def send(self, text):
        os.write(self.master, text.encode())

    def capture(self, name):
        (self.output / f'{name}.txt').write_text(self.text + '\n', encoding='utf-8')
        # HTML is rendered from actual parsed terminal cells, including colors.
        colors = dict(black='#000000', red='#cd0000', green='#00cd00', brown='#cdcd00',
                      blue='#5c5cff', magenta='#cd00cd', cyan='#00cdcd', white='#e5e5e5',
                      default='#d7dae0')
        def color(value, default):
            if value == 'default':
                return default
            return colors.get(value, '#' + value if len(value) == 6 else default)
        rows = []
        for y in range(self.screen.lines):
            cells = []
            for x in range(self.screen.columns):
                char = self.screen.buffer[y][x]
                fg, bg = color(char.fg, '#d7dae0'), color(char.bg, '#181b20')
                if char.reverse:
                    fg, bg = bg, fg
                weight = 'bold' if char.bold else 'normal'
                cells.append(f'<span style="color:{fg};background:{bg};font-weight:{weight}">'
                             + html.escape(char.data) + '</span>')
            rows.append(''.join(cells))
        page = '<!doctype html><meta charset="utf-8"><title>Latch PTY capture</title>'
        page += '<body style="background:#181b20"><pre style="line-height:1.2">'
        page += '\n'.join(rows) + '</pre></body>'
        (self.output / f'{name}.html').write_text(page, encoding='utf-8')

    def close(self):
        try:
            if self.child.poll() is None:
                os.killpg(self.child.pid, signal.SIGKILL)
            self.child.wait(timeout=5)
            (self.output / 'terminal.bin').write_bytes(self.raw)
        finally:
            os.close(self.master)
            os.close(self.slave)


def exercise(binary, output):
    with tempfile.TemporaryDirectory(prefix='latch-tui-pty-') as temp:
        root = Path(temp)
        workspace = root / 'workspace'
        workspace.mkdir()
        (workspace / 'proof.txt').write_text('PTY_SANDBOX_PROOF\n')
        provider = Provider()
        server = threading.Thread(target=provider.serve_forever, daemon=True)
        server.start()
        config = root / 'config.toml'
        config.write_text(f'''state_dir = {json.dumps(str(root / 'state'))}
default_mode = "WORK"
[providers.mock]
kind = "openai-compatible"
base_url = "http://127.0.0.1:{provider.server_port}/v1"
credential = "env:MOCK_API_KEY"
default_model = "pty-mock"
[inference]
provider = "mock"
model = "pty-mock"
[permissions]
mode = "human"
[safety]
level = "standard"
''')
        terminal = None
        try:
            terminal = Terminal(binary, config, workspace, output)
            terminal.wait(lambda: 'pty-mock' in terminal.text and 'Ask Latch' in terminal.text,
                          'startup screen')
            terminal.capture('01-startup')
            terminal.send('TUI_PROMPT_ONEX')
            terminal.wait(lambda: 'TUI_PROMPT_ONEX' in terminal.text, 'typed composer text')
            terminal.send('\x7f')
            terminal.wait(lambda: 'TUI_PROMPT_ONE' in terminal.text
                          and 'TUI_PROMPT_ONEX' not in terminal.text, 'backspace edit')
            terminal.capture('02-composer')
            terminal.send('\r')
            terminal.wait(lambda: 'PTY reply one verified' in terminal.text, 'first model reply')
            terminal.capture('03-reply')
            terminal.send('TUI_PROMPT_TWO\r')
            terminal.wait(lambda: 'PTY reply two verified' in terminal.text, 'follow-up model reply')
            terminal.capture('04-followup')
            terminal.send('/mode plan\r')
            terminal.wait(lambda: 'PLAN' in terminal.text, 'PLAN mode switch')
            terminal.capture('05-plan')
            terminal.screen.resize(lines=30, columns=92)
            fcntl.ioctl(terminal.slave, termios.TIOCSWINSZ, struct.pack('HHHH', 30, 92, 0, 0))
            os.kill(terminal.child.pid, signal.SIGWINCH)
            terminal.wait(lambda: 'pty-mock' in terminal.text and 'Ask Latch' in terminal.text
                          and 'PLAN' in terminal.text
                          and 'pty-mock' not in terminal.screen.display[0]
                          and 'PTY reply two verified' in terminal.text,
                          'redraw after terminal resize')
            terminal.capture('06-resized')
            terminal.send('/quit\r')
            terminal.wait(lambda: terminal.child.poll() is not None, 'graceful quit')
            assert terminal.child.returncode == 0, terminal.child.returncode
            assert termios.tcgetattr(terminal.slave) == terminal.original, 'raw mode was not restored'
            for enabled, disabled in ((b'\x1b[?1049h', b'\x1b[?1049l'),
                                      (b'\x1b[?2004h', b'\x1b[?2004l'),
                                      (b'\x1b[?1000h', b'\x1b[?1000l')):
                assert enabled in terminal.raw and disabled in terminal.raw, (enabled, disabled)
                assert terminal.raw.rfind(disabled) > terminal.raw.rfind(enabled)
            requests = provider.requests
            (output / 'mock-requests.json').write_text(json.dumps(requests, indent=2) + '\n')
            assert len(requests) == 3, f'expected three mock calls, got {len(requests)}'
            assert any(message.get('role') == 'tool'
                       and '[latch:tool:ok]' in str(message.get('content'))
                       and 'PTY_SANDBOX_PROOF' in str(message.get('content'))
                       for request in requests for message in request['messages']), 'missing successful shell result'
            for prompt in ('TUI_PROMPT_ONE', 'TUI_PROMPT_TWO'):
                assert any(message.get('role') == 'user' and str(message.get('content', '')).splitlines()[0] == prompt
                           for request in requests for message in request['messages']), prompt
            assert not provider.errors, provider.errors
            report = dict(status='passed', platform=platform.platform(), architecture=platform.machine(),
                          mock_requests=len(requests), checks=['startup', 'composer edit', 'submit',
                          'sandboxed shell', 'rendered replies', 'follow-up', 'mode switch',
                          'resize', 'quit', 'terminal restoration'])
            (output / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
            print(json.dumps(report, indent=2))
        except BaseException:
            if terminal:
                terminal.capture('failure')
            raise
        finally:
            if terminal:
                terminal.close()
            provider.shutdown()
            provider.server_close()
            server.join(timeout=5)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary', type=Path)
    parser.add_argument('--output', type=Path, default=Path('target/tui-pty'))
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    for name in ('failure.txt', 'failure.html'):
        (args.output / name).unlink(missing_ok=True)
    (args.output / 'report.json').write_text(json.dumps({'status': 'running'}) + '\n')
    try:
        exercise(args.binary.resolve(strict=True), args.output)
    except BaseException as error:
        (args.output / 'report.json').write_text(json.dumps({'status': 'failed', 'error': str(error)}, indent=2) + '\n')
        raise


if __name__ == '__main__':
    main()
