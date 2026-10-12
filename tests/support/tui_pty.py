"""Exercise the real binary/terminal boundary with only the Python standard library."""

import fcntl
import os
from pathlib import Path
import pty
import re
import select
import signal
import struct
import subprocess
import sys
import termios
import time


class Screen:
    """The few cursor and erase sequences ratatui emits, enough to read what the user would see.

    The terminal backend only redraws changed cells, so words are not contiguous in the raw
    output and must be searched on a reconstructed screen instead.
    """

    def __init__(self, rows=24, columns=120):
        self.rows = [[" "] * columns for _ in range(rows)]
        self.row = self.column = 0
        self.pending = b""

    def feed(self, data):
        self.pending += data
        text = self.pending.decode("utf-8", errors="ignore")
        # Keep an incomplete trailing escape sequence or UTF-8 character for the next read.
        incomplete = re.search(r"\x1b(\[[0-9;?<>]*)?$", text)
        self.pending = text[incomplete.start():].encode() if incomplete else b""
        text = text[: incomplete.start()] if incomplete else text
        position = 0
        for match in re.finditer(r"\x1b\[([0-9;?<>]*)([A-Za-z])", text):
            self.print(text[position:match.start()])
            position = match.end()
            self.control(match.group(1), match.group(2))
        self.print(text[position:])

    def print(self, text):
        for character in text:
            if character in "\r\n":
                continue
            if self.row < len(self.rows) and self.column < len(self.rows[0]):
                self.rows[self.row][self.column] = character
            self.column += 1

    def control(self, arguments, command):
        numbers = [int(part) if part.isdigit() else 0 for part in arguments.split(";")]
        if command == "H":
            self.row = max(numbers[0], 1) - 1
            self.column = max(numbers[1] if len(numbers) > 1 else 1, 1) - 1
        elif command == "J" and numbers[0] == 2:
            self.rows = [[" "] * len(row) for row in self.rows]
        elif command == "K":
            self.rows[self.row][self.column:] = [" "] * (len(self.rows[0]) - self.column)

    def text(self):
        return "\n".join("".join(row) for row in self.rows)


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
    if scenario in ["blocked_resolution", "responsive_navigation"]:
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
    screen = Screen()
    # Like a terminal, answer crossterm's keyboard-protocol query; legacy terminals only answer
    # the device-attributes request that follows it.
    kitty = scenario != "legacy_keyboard"
    answered = False

    def controlling_terminal():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)

    process = subprocess.Popen(
        [binary], stdin=slave, stdout=slave, stderr=slave,
        env=dict(environment, HOME=str(scenario_home)), preexec_fn=controlling_terminal,
    )

    def answer_keyboard_query():
        nonlocal answered
        if not answered and b"\x1b[?u\x1b[c" in transcript:
            os.write(master, (b"\x1b[?0u" if kitty else b"") + b"\x1b[?62;c")
            answered = True

    def pump(deadline):
        answer_keyboard_query()
        remaining = deadline - time.monotonic()
        assert remaining > 0, f"{scenario}: timed out:\n{screen.text()}\n{transcript!r}"
        readable, _, _ = select.select([master], [], [], remaining)
        if readable:
            data = os.read(master, 65536)
            assert data, f"{scenario}: unexpected terminal EOF: {transcript!r}"
            transcript.extend(data)
            screen.feed(data)

    def read_until(marker, start=0, timeout=5):
        """Waits for raw terminal output, such as a mode switch."""
        deadline = time.monotonic() + timeout
        while marker not in transcript[start:]:
            pump(deadline)

    def read_screen(text, timeout=5):
        """Waits until `text` is visible on the reconstructed screen."""
        deadline = time.monotonic() + timeout
        while text not in screen.text():
            pump(deadline)

    try:
        if scenario in ["blocked_resolution", "responsive_navigation"]:
            read_until(b"\x1b[?1049h")
            deadline = time.monotonic() + 5
            while not Path(home, "resolver-started").exists():
                assert time.monotonic() < deadline, "resolver did not start"
                time.sleep(0.01)
        elif scenario == "blocked_init_lock":
            read_screen("y initialize")
            os.write(master, b"y")
            time.sleep(0.1)
        else:
            # Crossterm positions words independently, so do not expect literal spaces.
            read_screen("quit")
        assert not termios.tcgetattr(slave)[3] & (termios.ICANON | termios.ECHO)
        assert b"\x1b[?1049h" in transcript
        # The kitty keyboard flags are pushed only where the terminal reported support.
        if kitty:
            read_until(b"\x1b[>1u")
        if scenario == "paste":
            start = len(transcript)
            os.write(master, b"a")
            read_screen("Enter save")
            os.write(master, b"\x1b[200~pasted\x1b[201~")
            os.write(master, b"\t")
            os.write(master, b"\x1b[200~pasted.example\x1b[201~")
            os.write(master, b"\r")
            read_screen("Saved")
            assert "Host pasted\n" in Path(home, ".ssh/portico/config").read_text()
        elif scenario == "paste_filter_edit":
            start = len(transcript)
            os.write(master, b"/")
            os.write(master, b"\x1b[200~beta\x1b[201~")
            os.write(master, b"\r")
            os.write(master, b"e")
            read_screen("Enter save")
            os.write(master, b"\x1b[200~x\x1b[201~")
            os.write(master, b"\r")
            read_screen("Saved")
            config = Path(home, ".ssh/portico/config").read_text()
            assert "Host alpha\n" in config
            assert "Host betax\n" in config
        elif scenario == "blocked_mutation_lock":
            start = len(transcript)
            os.write(master, b"a")
            read_screen("Enter save")
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
            read_screen("launcher")
        elif scenario == "responsive_navigation":
            # Resolution is stuck in a slow ssh -G, yet keys are still handled and quitting is immediate.
            os.write(master, b"j")
            os.write(master, b"k")
        elif scenario == "ctrl_enter":
            # Terminals with the kitty keyboard protocol report Ctrl+Enter as CSI 13;5u.
            start = len(transcript)
            os.write(master, b"a")
            read_screen("Enter save")
            os.write(master, b"\x1b[13;5u")
            os.write(master, b"\x1b[27u")
            os.write(master, b"\x1b[13;5u")
            read_screen("requested")
            launched = Path(home, "launcher-args").read_text().split()
            assert "ssh" in launched and launched[-1] == "alpha", launched
            assert not Path(home, "child-terminal-state").exists()
        elif scenario == "legacy_keyboard":
            assert b"\x1b[>1u" not in transcript
            assert "Ctrl+Enter" not in screen.text()
        elif scenario == "child_failure":
            start = len(transcript)
            os.write(master, b"\r")
            read_screen("exited")
            child_output = transcript[start:]
            left = child_output.index(b"\x1b[?1049l")
            entered = child_output.index(b"\x1b[?1049h", left)
            assert left < entered
            # The child gets a terminal with the original key reporting, and the TUI re-arms it.
            assert child_output.index(b"\x1b[<1u") < left
            assert b"\x1b[>1u" in child_output[entered:]
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
        timeout = 1 if scenario in ["blocked_resolution", "responsive_navigation", "blocked_mutation_lock", "blocked_init_lock", "blocked_connection"] else 5
        read_until(b"\x1b[?1049l", start, timeout)
        left = transcript.index(b"\x1b[?1049l", start)
        if kitty:
            assert transcript.index(b"\x1b[<1u", start) < left, f"{scenario}: keyboard flags were not popped"
        else:
            assert b"\x1b[<1u" not in transcript
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
        if scenario in ["blocked_resolution", "responsive_navigation"]:
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
        if scenario in ["launcher_failure", "ctrl_enter"]:
            Path(home, "launcher-args").unlink(missing_ok=True)
        if held_lock is not None:
            held_lock.close()
        os.close(master)
        os.close(slave)


for scenario in ["paste", "paste_filter_edit", "blocked_resolution", "blocked_mutation_lock", "blocked_init_lock", "blocked_connection", "launcher_failure", "ctrl_enter", "legacy_keyboard", "responsive_navigation", "normal_exit", "child_failure", signal.SIGINT, signal.SIGTERM, signal.SIGHUP]:
    check_terminal(scenario)
