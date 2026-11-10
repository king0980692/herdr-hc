#!/usr/bin/env python3
"""Unix-only, opt-in herdr-core terminal bridge PoC; run in a disposable pane.

    python3 scripts/core_terminal_bridge.py t1
    python3 scripts/core_terminal_bridge.py t1 --connect ssh -T box hc connect

This talks to the framed API, not `hc attach`. Herdr's existing PTY/parser
renders the bytes, so layout and borders stay local. Ctrl-] detaches without
killing the core terminal. No layout persistence, status integration or reconnect.
"""

import argparse
import json
import os
import selectors
import signal
import struct
import subprocess
import sys
import time


HEADER = struct.Struct("<IBI")
MAX_PAYLOAD = 16 * 1024 * 1024
DATA, CREDIT, CLOSE, RESIZE = range(4)


def frame(channel, kind, payload):
    if len(payload) > MAX_PAYLOAD:
        raise ValueError("frame exceeds 16 MiB")
    return HEADER.pack(channel, kind, len(payload)) + payload


class Decoder:
    def __init__(self):
        self.pending = bytearray()

    def feed(self, data):
        self.pending.extend(data)
        frames = []
        while len(self.pending) >= HEADER.size:
            channel, kind, size = HEADER.unpack_from(self.pending)
            if size > MAX_PAYLOAD:
                raise ValueError("frame exceeds 16 MiB")
            end = HEADER.size + size
            if len(self.pending) < end:
                break
            frames.append((channel, kind, bytes(self.pending[HEADER.size:end])))
            del self.pending[:end]
        return frames


def rpc_result(reply, request_id):
    if not isinstance(reply, dict) or reply.get("id") != request_id:
        raise ValueError("unexpected RPC reply")
    if "error" in reply:
        raise ValueError(str(reply["error"]))
    result = reply.get("result")
    if not isinstance(result, dict):
        raise ValueError("missing RPC result")
    return result


def write_all(fd, data):
    remaining = memoryview(data)
    while remaining:
        written = os.write(fd, remaining)
        if written == 0:
            raise OSError("short write")
        remaining = remaining[written:]


def read_hello(fd):
    # Read only the plain reply; subsequent bytes use the framed codec.
    reply = bytearray()
    deadline = time.monotonic() + 10
    with selectors.DefaultSelector() as selector:
        selector.register(fd, selectors.EVENT_READ)
        while len(reply) < MAX_PAYLOAD:
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not selector.select(remaining):
                raise TimeoutError("core hello timed out")
            byte = os.read(fd, 1)
            if not byte:
                raise EOFError("core disconnected during hello")
            if byte == b"\n":
                return rpc_result(json.loads(reply), 1)
            reply.extend(byte)
    raise ValueError("hello exceeds 16 MiB")


def bridge(terminal, command):
    # Unix console handling lives only in this experimental Unix entry point.
    import termios
    import tty

    if not os.isatty(0) or not os.isatty(1):
        raise ValueError("run this bridge in a disposable terminal pane")
    saved = termios.tcgetattr(0)
    wake_read, wake_write = os.pipe()
    os.set_blocking(wake_read, False)
    os.set_blocking(wake_write, False)
    old_wakeup = signal.set_wakeup_fd(wake_write)
    old_handlers = {}
    child = None
    raw = False
    stopping = False

    def caught(signum, _):
        nonlocal stopping
        if signum != signal.SIGWINCH:
            stopping = True

    try:
        for sig in (signal.SIGWINCH, signal.SIGTERM, signal.SIGHUP, signal.SIGINT):
            old_handlers[sig] = signal.signal(sig, caught)
        child = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, bufsize=0)
        transport_in, transport_out = child.stdin.fileno(), child.stdout.fileno()

        def send(channel, kind, data):
            write_all(transport_in, frame(channel, kind, data))

        def size():
            geometry = os.get_terminal_size(0)
            return geometry.columns, geometry.lines

        hello = {"jsonrpc": "2.0", "id": 1, "method": "hello",
                 "params": {"generation": 1, "framed": True}}
        write_all(transport_in, json.dumps(hello).encode() + b"\n")
        result = read_hello(transport_out)
        if result.get("generation") != 1 or result.get("framed") is not True:
            raise ValueError("core did not negotiate framed generation 1")
        if "terminal.attach" not in result.get("methods", []):
            raise ValueError("core does not advertise terminal.attach")
        last_size = size()
        cols, rows = last_size
        request = {"jsonrpc": "2.0", "id": 2, "method": "terminal.attach",
                   "params": {"terminal": terminal, "cols": cols, "rows": rows}}
        send(0, DATA, json.dumps(request).encode())
        decoder = Decoder()
        channel = None
        attach_deadline = time.monotonic() + 10
        with selectors.DefaultSelector() as selector:
            selector.register(transport_out, selectors.EVENT_READ, "core")
            selector.register(wake_read, selectors.EVENT_READ, "signal")
            while not stopping:
                timeout = None if channel is not None else max(0, attach_deadline - time.monotonic())
                ready = selector.select(timeout)
                if not ready:
                    raise TimeoutError("core attach timed out")
                for key, _ in ready:
                    if key.data == "signal":
                        os.read(wake_read, 4096)
                        if channel is not None and not stopping:
                            last_size = size()
                            send(channel, RESIZE, struct.pack("<HH", *last_size))
                    elif key.data == "input":
                        data = os.read(0, 65536)
                        before, detach, _ = data.partition(b"\x1d")
                        if before:
                            current_size = size()
                            if current_size != last_size:
                                last_size = current_size
                                send(channel, RESIZE, struct.pack("<HH", *last_size))
                            # shortcut: PTY input cannot tag automatic replies; use legacy DATA
                            # until a native runtime adapter preserves response provenance.
                            send(channel, DATA, before)
                        if detach or not data:
                            send(channel, CLOSE, b"")
                            return
                    else:
                        data = os.read(transport_out, 65536)
                        if not data:
                            raise EOFError("core disconnected; terminal is not killed, reattach manually")
                        for stream, kind, payload in decoder.feed(data):
                            if stream == 0 and kind == DATA:
                                if channel is not None:
                                    raise ValueError("unexpected control message after attach")
                                attached = rpc_result(json.loads(payload), 2)
                                channel = attached.get("channel")
                                if type(channel) is not int or not 0 < channel <= 0xFFFFFFFF:
                                    raise ValueError("invalid attach channel")
                                tty.setraw(0)
                                raw = True
                                selector.register(0, selectors.EVENT_READ, "input")
                                send(channel, RESIZE, struct.pack("<HH", *size()))
                            elif channel is None or stream != channel:
                                raise ValueError("unexpected stream before attach")
                            elif kind == DATA:
                                write_all(1, payload)
                                send(channel, CREDIT, struct.pack("<I", len(payload)))
                            elif kind == CLOSE:
                                if payload:
                                    print("\r\ncore terminal closed: " + str(json.loads(payload)), file=sys.stderr)
                                return
                            elif kind != CREDIT:
                                raise ValueError("unexpected core frame kind")
    finally:
        # Never stop core or kill the attached terminal; terminate only our transport.
        if child is not None:
            child.stdin.close()
            if child.poll() is None:
                child.terminate()
            try:
                child.wait(timeout=3)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()
            child.stdout.close()
        try:
            if raw:
                # This PoC is for disposable panes, not arbitrary terminal-mode restoration.
                write_all(1, b"\x18\x1b[?2026l\x1b[?1049l\x1b[0m\x1b[?25h\r\n")
        finally:
            termios.tcsetattr(0, termios.TCSANOW, saved)
            signal.set_wakeup_fd(old_wakeup)
            for sig, handler in old_handlers.items():
                signal.signal(sig, handler)
            os.close(wake_read)
            os.close(wake_write)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("terminal", help="core terminal ID or machine/terminal address")
    parser.add_argument("--connect", nargs=argparse.REMAINDER,
                        help="transport argv, e.g. ssh -T box hc connect (default: hc connect)")
    args = parser.parse_args()
    if os.name != "posix":
        parser.error("this experimental bridge supports Unix only")
    try:
        bridge(args.terminal, args.connect if args.connect is not None else ["hc", "connect"])
    except (OSError, ValueError, EOFError) as error:
        print("core bridge: " + str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
