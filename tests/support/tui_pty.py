"""Exercise the real binary/terminal boundary with only the Python standard library."""

import fcntl
import os
from pathlib import Path
import pty
import select
import signal
import struct
import subprocess
import sys
import termios
import time


binary, home, fake_bin = sys.argv[1:]
environment = dict(os.environ, HOME=home, PATH=fake_bin + os.pathsep + os.environ["PATH"])


def check_terminal(scenario):
    if scenario == "blocked_resolution":
        Path(home, "block-resolver").touch()
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 120, 0, 0))
    before = termios.tcgetattr(slave)
    before_stty = subprocess.check_output(["stty", "-g"], stdin=slave).strip()
    transcript = bytearray()

    def controlling_terminal():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)

    process = subprocess.Popen(
        [binary], stdin=slave, stdout=slave, stderr=slave,
        env=environment, preexec_fn=controlling_terminal,
    )

    def read_until(marker, start=0, timeout=5):
        deadline = time.monotonic() + timeout
        while marker not in transcript[start:]:
            remaining = deadline - time.monotonic()
            assert remaining > 0, f"{scenario}: timed out waiting for {marker!r}: {transcript!r}"
            readable, _, _ = select.select([master], [], [], remaining)
            if readable:
                data = os.read(master, 65536)
                assert data, f"{scenario}: unexpected terminal EOF: {transcript!r}"
                transcript.extend(data)

    try:
        if scenario == "blocked_resolution":
            read_until(b"\x1b[?1049h")
            deadline = time.monotonic() + 5
            while not Path(home, "resolver-started").exists():
                assert time.monotonic() < deadline, "resolver did not start"
                time.sleep(0.01)
        else:
            # Crossterm positions words independently, so do not expect literal spaces.
            read_until(b"quit")
        assert not termios.tcgetattr(slave)[3] & (termios.ICANON | termios.ECHO)
        assert b"\x1b[?1049h" in transcript
        if scenario == "paste":
            start = len(transcript)
            os.write(master, b"a")
            read_until(b"save", start)
            os.write(master, b"\x1b[200~pasted\x1b[201~")
            os.write(master, b"\t")
            os.write(master, b"\x1b[200~pasted.example\x1b[201~")
            os.write(master, b"\r")
            read_until(b"Saved", start)
            assert "Host pasted\n" in Path(home, ".ssh/portico/config").read_text()
        elif scenario == "child_failure":
            start = len(transcript)
            os.write(master, b"\r")
            read_until(b"exited", start)
            child_output = transcript[start:]
            left = child_output.index(b"\x1b[?1049l")
            entered = child_output.index(b"\x1b[?1049h", left)
            assert left < entered
            assert b"\x1b[?25h" in child_output[:entered]
            assert Path(home, "child-terminal-state").read_bytes().strip() == before_stty
            assert not termios.tcgetattr(slave)[3] & (termios.ICANON | termios.ECHO)
        start = len(transcript)
        if scenario == "blocked_resolution":
            os.kill(process.pid, signal.SIGTERM)
        elif isinstance(scenario, int):
            os.kill(process.pid, scenario)
        else:
            os.write(master, b"q")
        read_until(b"\x1b[?1049l", start, 1 if scenario == "blocked_resolution" else 5)
        left = transcript.index(b"\x1b[?1049l", start)
        read_until(b"\x1b[?25h", left)
        assert process.wait(timeout=5) == 0, f"{scenario}: {transcript!r}"
        assert termios.tcgetattr(slave) == before, f"{scenario}: termios was not restored"
        assert b"\x1b[?1049l" in transcript[start:]
        print(f"PTY restored: {scenario}")
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=5)
        if scenario == "blocked_resolution":
            Path(home, "block-resolver").unlink(missing_ok=True)
            Path(home, "resolver-started").unlink(missing_ok=True)
        os.close(master)
        os.close(slave)


for scenario in ["paste", "blocked_resolution", "normal_exit", "child_failure", signal.SIGINT, signal.SIGTERM, signal.SIGHUP]:
    check_terminal(scenario)
