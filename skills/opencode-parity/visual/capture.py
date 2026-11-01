#!/usr/bin/env python3
"""Run a TUI in a pty and dump its final screen as a cell grid.

usage: capture.py OUT.json ROWS COLS SETTLE_SECONDS [--keys HEX] -- cmd args...

Each cell is [char, fg, bg, flags] where flags is a string of b/i/u/r/s
(bold/italic/underline/reverse/strike). pyte resolves SGR into named colours or
6-digit hex, so truecolor and indexed output compare on the same terms.
"""
import fcntl
import json
import os
import pty
import re
import select
import struct
import subprocess
import sys
import termios
import time

import pyte


# The terminal both TUIs see. Fixed to the palette upstream's `system` theme was
# captured under (upstream caches it in system-theme.json), so ox-tui's own OSC
# 4/10/11 probe resolves the same colours as the gold grids.
TERM = {
    "fg": "a6b2c0",
    "bg": "1b2738",
    "palette": [
        "1d1f21", "cc6666", "b5bd68", "f0c674", "81a2be", "b294bb", "8abeb7", "c5c8c6",
        "666666", "d54e53", "b9ca4a", "e7c547", "7aa6da", "c397d8", "70c0b1", "eaeaea",
    ],
}


def osc_rgb(hex6):
    r, g, b = hex6[0:2], hex6[2:4], hex6[4:6]
    return f"rgb:{r}{r}/{g}{g}/{b}{b}".encode()


def capture(cmd, rows, cols, settle, keys=None, env_extra=None):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    env = dict(os.environ, TERM="xterm-256color", COLORTERM="truecolor")
    env.update(env_extra or {})
    proc = subprocess.Popen(
        cmd, stdin=slave, stdout=slave, stderr=slave, env=env, close_fds=True
    )
    os.close(slave)

    screen = pyte.Screen(cols, rows)
    stream = pyte.ByteStream(screen)

    def pump(seconds):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            ready, _, _ = select.select([master], [], [], 0.05)
            if not ready:
                continue
            try:
                data = os.read(master, 65536)
            except OSError:
                return
            if not data:
                return
            # Terminal queries (OSC 10/11/4, DA, DSR) get a canned answer so a
            # TUI that probes the terminal does not stall waiting for one.
            reply(data)
            # pyte reads ESC[>4;1m (modifyOtherKeys) as SGR 4;1; drop private-prefixed 'm'.
            stream.feed(re.sub(rb'\x1b\[[>?=<][0-9;:]*m', b'', data))

    def reply(data):
        if b"\x1b]10;?" in data:
            os.write(master, b"\x1b]10;" + osc_rgb(TERM["fg"]) + b"\x1b\\")
        if b"\x1b]11;?" in data:
            os.write(master, b"\x1b]11;" + osc_rgb(TERM["bg"]) + b"\x1b\\")
        if b"\x1b]4;0;?" in data:
            body = b";".join(
                b"%d;" % i + osc_rgb(c) for i, c in enumerate(TERM["palette"])
            )
            os.write(master, b"\x1b]4;" + body + b"\x1b\\")
        if b"\x1b[c" in data or b"\x1b[0c" in data:
            os.write(master, b"\x1b[?62;c")
        if b"\x1b[6n" in data:
            os.write(master, b"\x1b[1;1R")

    pump(settle)
    for chunk in keys or []:
        os.write(master, chunk)
        pump(0.4)
    pump(0.3)

    grid = []
    for y in range(rows):
        line = []
        for x in range(cols):
            c = screen.buffer[y][x]
            flags = "".join(
                f
                for f, on in (
                    ("b", c.bold),
                    ("i", c.italics),
                    ("u", c.underscore),
                    ("r", c.reverse),
                    ("s", c.strikethrough),
                )
                if on
            )
            line.append([c.data, c.fg, c.bg, flags])
        grid.append(line)

    try:
        os.write(master, b"\x03")
    except OSError:
        pass
    time.sleep(0.2)
    proc.kill()
    proc.wait()
    return grid


def main():
    argv = sys.argv[1:]
    out, rows, cols, settle = argv[0], int(argv[1]), int(argv[2]), float(argv[3])
    rest = argv[4:]
    keys = []
    while rest and rest[0] != "--":
        if rest[0] == "--keys":
            keys = [bytes.fromhex(h) for h in rest[1].split(",") if h]
            rest = rest[2:]
        else:
            raise SystemExit(f"unexpected arg {rest[0]}")
    cmd = rest[1:]
    grid = capture(cmd, rows, cols, settle, keys)
    with open(out, "w") as f:
        json.dump({"rows": rows, "cols": cols, "cells": grid}, f)
    for y, line in enumerate(grid):
        text = "".join(c[0] for c in line).rstrip()
        if text:
            print(f"{y + 1:2}|{text}")


if __name__ == "__main__":
    main()
