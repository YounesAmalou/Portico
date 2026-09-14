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
    scenario_home = Path(home, "uninitialized") if scenario == "blocked_init_lock" else Path(home)
    held_lock = None
    if scenario in ["blocked_mutation_lock", "blocked_init_lock"]:
        lock_path = scenario_home / ".ssh/portico/config.lock"
        lock_path.parent.mkdir(parents=True, exist_ok=True)
        held_lock = lock_path.open("a+")
        fcntl.flock(held_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    if scenario == "blocked_resolution":
        Path(home, "block-resolver").touch()
    if scenario == "blocked_connection":
        Path(home, "block-connection").touch()
    if scenario == "launcher_failure":
        Path(home, "fail-launcher").touch()
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
        env=dict(environment, HOME=str(scenario_home)), preexec_fn=controlling_terminal,
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
        elif scenario == "blocked_init_lock":
            read_until(b"initialize")
            os.write(master, b"y")
            time.sleep(0.1)
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
        elif scenario == "paste_filter_edit":
            start = len(transcript)
            os.write(master, b"/")
            os.write(master, b"\x1b[200~beta\x1b[201~")
            os.write(master, b"\r")
            os.write(master, b"e")
            read_until(b"save", start)
            os.write(master, b"\x1b[200~x\x1b[201~")
            os.write(master, b"\r")
            read_until(b"Saved", start)
            config = Path(home, ".ssh/portico/config").read_text()
            assert "Host alpha\n" in config
            assert "Host betax\n" in config
        elif scenario == "blocked_mutation_lock":
            start = len(transcript)
            os.write(master, b"a")
            read_until(b"save", start)
            os.write(master, b"\x1b[200~locked\x1b[201~")
            os.write(master, b"\t")
            os.write(master, b"\x1b[200~locked.example\x1b[201~")
            os.write(master, b"\r")
            time.sleep(0.1)
        elif scenario == "blocked_connection":
            os.write(master, b"\r")
            deadline = time.monotonic() + 5
            while not Path(home, "connection-started").exists():
                assert time.monotonic() < deadline, "connection did not start"
                time.sleep(0.01)
            _, parent, worker, child = map(int, Path(home, "connection-started").read_text().split())
            assert str(child) not in Path(f"/proc/{parent}/task/{parent}/children").read_text().split()
            assert str(child) in Path(f"/proc/{parent}/task/{worker}/children").read_text().split()
        elif scenario == "launcher_failure":
            start = len(transcript)
            os.write(master, b"n")
            read_until(b"launcher", start)
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
        if scenario in ["blocked_resolution", "blocked_mutation_lock", "blocked_init_lock", "blocked_connection"]:
            os.kill(process.pid, signal.SIGTERM)
        elif isinstance(scenario, int):
            os.kill(process.pid, scenario)
        else:
            os.write(master, b"q")
        timeout = 1 if scenario in ["blocked_resolution", "blocked_mutation_lock", "blocked_init_lock", "blocked_connection"] else 5
        read_until(b"\x1b[?1049l", start, timeout)
        left = transcript.index(b"\x1b[?1049l", start)
        read_until(b"\x1b[?25h", left)
        assert process.wait(timeout=5) == 0, f"{scenario}: {transcript!r}"
        assert termios.tcgetattr(slave) == before, f"{scenario}: termios was not restored"
        assert b"\x1b[?1049l" in transcript[start:]
        if scenario == "blocked_connection":
            shell, parent, worker, child = map(int, Path(home, "connection-started").read_text().split())
            for pid in [shell, parent, worker, child]:
                deadline = time.monotonic() + 1
                while True:
                    try:
                        os.kill(pid, 0)
                    except ProcessLookupError:
                        break
                    assert time.monotonic() < deadline, f"connection process survived: {pid}"
                    time.sleep(0.01)
        print(f"PTY restored: {scenario}")
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=5)
        if scenario == "blocked_resolution":
            Path(home, "block-resolver").unlink(missing_ok=True)
            Path(home, "resolver-started").unlink(missing_ok=True)
        if scenario == "blocked_connection":
            marker = Path(home, "connection-started")
            if marker.exists():
                shell, parent, worker, child = map(int, marker.read_text().split())
                for pid in [shell, parent, worker, child]:
                    try:
                        os.kill(pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
            Path(home, "block-connection").unlink(missing_ok=True)
            marker.unlink(missing_ok=True)
        if scenario == "launcher_failure":
            Path(home, "fail-launcher").unlink(missing_ok=True)
        if held_lock is not None:
            held_lock.close()
        os.close(master)
        os.close(slave)


for scenario in ["paste", "paste_filter_edit", "blocked_resolution", "blocked_mutation_lock", "blocked_init_lock", "blocked_connection", "launcher_failure", "normal_exit", "child_failure", signal.SIGINT, signal.SIGTERM, signal.SIGHUP]:
    check_terminal(scenario)
