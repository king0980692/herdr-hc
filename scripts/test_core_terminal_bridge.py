"""Wire checks for the opt-in core terminal bridge PoC."""

import json
import os
from pathlib import Path
import selectors
import shlex
import shutil
import signal
import socket
import stat
import struct
import subprocess
import sys
import tempfile
import time
import unittest

from scripts.core_terminal_bridge import Decoder, frame, rpc_result


def cleanup_scratch(scratch, stop, ssh=()):
    try:
        if json.loads(stop()).get("stopped") is not True:
            raise RuntimeError("core stop was not confirmed")
        if ssh:
            remove = ["/bin/sh", "-c",
                      'sockets=$(find "$1" -type s -print -quit) && test -z "$sockets" && rm -rf -- "$1"',
                      "hc-fixture", scratch]
            subprocess.check_call([*ssh, shlex.join(remove)], timeout=15)
        else:
            for path in Path(scratch).iterdir():
                try:
                    is_socket = stat.S_ISSOCK(path.lstat().st_mode)
                except FileNotFoundError:
                    continue
                if not is_socket:
                    continue
                with socket.socket(socket.AF_UNIX) as probe:
                    probe.settimeout(0.1)
                    try:
                        probe.connect(str(path))
                    except (FileNotFoundError, ConnectionRefusedError):
                        continue
                    raise RuntimeError("core or holder still reachable")
            shutil.rmtree(scratch)
    except Exception as error:
        raise RuntimeError(f"scratch cleanup failed; retained {scratch}: {error}") from error


class CoreTerminalBridgeTests(unittest.TestCase):
    def test_frames_survive_fragmented_headers_and_utf8(self):
        wire = frame(7, 0, "中".encode()) + frame(7, 2, b"")
        decoder = Decoder()
        received = []
        for byte in wire:
            received.extend(decoder.feed(bytes([byte])))
        self.assertEqual(received, [(7, 0, "中".encode()), (7, 2, b"")])

    def test_oversized_payload_is_rejected_before_allocation(self):
        with self.assertRaisesRegex(ValueError, "16 MiB"):
            Decoder().feed(struct.pack("<IBI", 7, 0, 16 * 1024 * 1024 + 1))

    def test_rpc_error_is_not_treated_as_success(self):
        with self.assertRaisesRegex(ValueError, "no terminal"):
            rpc_result({"id": 2, "error": {"message": "no terminal"}}, 2)

    def test_unrelated_rpc_reply_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "unexpected"):
            rpc_result({"id": 1, "result": {}}, 2)

    def test_valid_rpc_result(self):
        self.assertEqual(rpc_result({"id": 2, "result": {"channel": 4}}, 2), {"channel": 4})

    def test_failed_cleanup_retains_scratch_evidence(self):
        for reply in ('{"stopped":false}', '{}', 'not JSON'):
            with tempfile.TemporaryDirectory(prefix="hc-cleanup-") as scratch:
                with self.assertRaisesRegex(RuntimeError, "retained " + scratch):
                    cleanup_scratch(scratch, lambda: reply)
                self.assertTrue(Path(scratch).is_dir())

    def test_stop_error_retains_scratch_evidence(self):
        def failed_stop():
            raise subprocess.TimeoutExpired("private hc stop", 15)
        with tempfile.TemporaryDirectory(prefix="hc-cleanup-") as scratch:
            with self.assertRaisesRegex(RuntimeError, "retained " + scratch):
                cleanup_scratch(scratch, failed_stop)
            self.assertTrue(Path(scratch).is_dir())

    def test_confirmed_cleanup_removes_only_its_scratch(self):
        with tempfile.TemporaryDirectory(prefix="hc-cleanup-") as base:
            scratch = Path(base) / "owned"
            other = Path(base) / "other"
            scratch.mkdir()
            other.mkdir()
            cleanup_scratch(str(scratch), lambda: '{"stopped":true}')
            self.assertFalse(scratch.exists())
            self.assertTrue(other.is_dir())


@unittest.skipUnless(os.name == "posix" and os.environ.get("HERDR_CORE_SMOKE") == "1",
                     "opt in with HERDR_CORE_SMOKE=1; needs installed hc")
class LiveBridgeTests(unittest.TestCase):
    def test_input_resize_detach_and_terminal_survives(self):
        import fcntl
        import pty
        import termios

        host = os.environ.get("HERDR_CORE_SMOKE_SSH")
        hc = os.environ.get("HERDR_CORE_SMOKE_HC") or ("hc" if host else shutil.which("hc"))
        self.assertIsNotNone(hc, "hc must be installed for the live smoke")
        ssh = ["ssh", "-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", host] if host else []
        if host:
            scratch = subprocess.check_output(
                ssh + ["mktemp -d /var/tmp/hc-b-XXXXXX"], text=True, timeout=15).strip()
            self.assertTrue(scratch.startswith("/var/tmp/hc-b-") and
                            "/" not in scratch[len("/var/tmp/"):], repr(scratch))
        else:
            scratch = tempfile.mkdtemp(prefix="hc-b-", dir="/var/tmp")
        env = {key: value for key, value in os.environ.items() if not key.startswith("HC_")}
        env.update(HC_SESSION="bridge-smoke", XDG_CONFIG_HOME=scratch,
                   XDG_STATE_HOME=scratch, XDG_RUNTIME_DIR=scratch)
        bridge = None
        master = slave = None

        def command(*args):
            if not host:
                return [hc, *args]
            remote = ["env", "-u", "HC_SOCKET", "-u", "HC_TERMINAL_ID",
                      "-u", "HC_TERMINAL_SOCKET", "-u", "HC_SESSION"]
            remote.extend(key + "=" + env[key] for key in
                          ("HC_SESSION", "XDG_CONFIG_HOME", "XDG_STATE_HOME", "XDG_RUNTIME_DIR"))
            return ssh + [shlex.join(remote + [hc, *args])]

        def core(*args):
            return subprocess.check_output(command(*args), env=env, text=True, timeout=15)

        try:
            program = ("import os,sys,signal; "
                       "signal.signal(signal.SIGWINCH, lambda *_: "
                       "print('CORE_SIZE:' + str(os.get_terminal_size(0)),flush=True)); "
                       "print('BRIDGE_READY',flush=True); "
                       "exec(\"for line in sys.stdin:\\n print('CORE_REPLY:' + line.strip() + ':' + "
                       "str(os.get_terminal_size(0)), flush=True)\")")
            python = "python3" if host else sys.executable
            core("run", "--bg", "--name", "bridge-smoke", python, "-u", "-c", program)
            terminals = json.loads(core("ls"))["terminals"]
            self.assertEqual(len(terminals), 1)
            terminal = terminals[0]["address"]
            master, slave = pty.openpty()
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 12, 60, 0, 0))
            script = Path(__file__).with_name("core_terminal_bridge.py")
            bridge = subprocess.Popen([sys.executable, str(script), terminal,
                                       "--connect", *command("connect")], env=env,
                                      stdin=slave, stdout=slave, stderr=slave)
            output = bytearray()

            def expect(marker):
                deadline = time.monotonic() + 10
                with selectors.DefaultSelector() as selector:
                    selector.register(master, selectors.EVENT_READ)
                    while marker not in output:
                        remaining = deadline - time.monotonic()
                        self.assertGreater(remaining, 0, repr(output))
                        self.assertTrue(selector.select(remaining), repr(output))
                        output.extend(os.read(master, 65536))

            expect(b"BRIDGE_READY")
            os.write(master, b"first\r")
            expect(b"CORE_REPLY:first:os.terminal_size(columns=60, lines=12)")
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 18, 80, 0, 0))
            bridge.send_signal(signal.SIGWINCH)
            expect(b"CORE_SIZE:os.terminal_size(columns=80, lines=18)")
            os.write(master, b"resized\r")
            expect(b"CORE_REPLY:resized:os.terminal_size(columns=80, lines=18)")
            os.write(master, b"\x1d")
            self.assertEqual(bridge.wait(timeout=5), 0, repr(output))
            self.assertTrue(termios.tcgetattr(slave)[3] & termios.ICANON)
            terminals = json.loads(core("ls"))["terminals"]
            self.assertEqual([record["address"] for record in terminals], [terminal])
        finally:
            try:
                if bridge is not None and bridge.poll() is None:
                    bridge.terminate()
                    try:
                        bridge.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        bridge.kill()
                        bridge.wait(timeout=5)
            finally:
                if master is not None:
                    os.close(master)
                    os.close(slave)
                cleanup_scratch(scratch, lambda: core("server", "stop", "--json"), ssh)


if __name__ == "__main__":
    unittest.main()
