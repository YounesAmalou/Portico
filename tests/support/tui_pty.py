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


binary, home, fake_bin, *requested = sys.argv[1:]
environment = dict(os.environ, HOME=home, PATH=fake_bin + os.pathsep + os.environ["PATH"])
# The developer's own agent must never be asked (a locked 1Password would prompt).
environment.pop("SSH_AUTH_SOCK", None)


def is_running(pid):
    """Whether `pid` can still run. A zombie is dead: containers whose PID 1 never reaps
    orphans (as in CI) leave killed processes behind as zombies that signals still reach."""
    try:
        stat = Path(f"/proc/{pid}/stat").read_text()
    except FileNotFoundError:
        return False
    return stat.rsplit(")", 1)[1].split()[0] != "Z"


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
    if scenario == "interrupt_connection":
        Path(home, "interrupt-connection").touch()
    if scenario == "launcher_failure":
        Path(home, "fail-launcher").touch()
    managed_before = Path(scenario_home, ".ssh/portico/config").read_text() if scenario == "live_reload" else None
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
        elif scenario == "interrupt_connection":
            # Ctrl+C while ssh connects goes to the whole foreground group: ssh ends, Portico stays.
            os.write(master, b"\r")
            deadline = time.monotonic() + 5
            while not Path(home, "interrupt-ready").exists():
                assert time.monotonic() < deadline, "connection did not start"
                time.sleep(0.01)
            os.write(master, b"\x03")
            read_screen("exited")
            assert "alpha" in screen.text()
            time.sleep(0.2)
            assert process.poll() is None, "Ctrl+C during the connection quit the TUI"
            assert b"\x1b[?1049h" in transcript[transcript.rindex(b"\x1b[?1049l"):]
        elif scenario == "input_rules":
            # Whitespace never enters a field, however it arrives; the saved host proves it.
            os.write(master, b"a")
            read_screen("Enter save")
            os.write(master, b"my host\t")
            os.write(master, b"\x1b[200~my.exa mple\x1b[201~\t")
            os.write(master, b"de ploy\t2 2\t")
            # A typed leading or trailing space is refused; a pasted inner one is kept.
            os.write(master, b" ~/.ssh/my \x1b[200~ key\x1b[201~ \t")
            os.write(master, b"\x1b[200~bas tion, jump\n\x1b[201~\r")
            read_screen("Saved")
            config = Path(home, ".ssh/portico/config").read_text()
            block = config[config.index("Host myhost\n"):]
            for line in ["HostName my.example", "User deploy", "Port 22", "ProxyJump bastion,jump"]:
                assert line in block, block
            assert 'IdentityFile "~/.ssh/my key"\n' in block, block
        elif scenario == "refresh":
            # What `ssh -G` resolves can change without any config file changing: no watch sees
            # it, so it stays invisible until `r` re-reads everything.
            read_screen("fail.example")
            Path(home, "resolved-hostname").write_text("changed.example")
            time.sleep(1.5)
            assert "changed.example" not in screen.text()
            os.write(master, b"r")
            read_screen("Refreshed: ")
            read_screen("changed.example")
            # A refresh that cannot read the inventory keeps the screen as it was.
            managed = Path(home, ".ssh/portico/config")
            managed.chmod(0)
            os.write(master, b"\x1b[15~")
            read_screen("Refresh failed")
            assert "alpha" in screen.text()
            managed.chmod(0o600)
            os.write(master, b"r")
            read_screen("Refreshed: ")
        elif scenario == "live_reload":
            config = Path(home, ".ssh/config")
            extra = Path(home, ".ssh/extra.conf")

            def wait_gone(text, timeout=5):
                deadline = time.monotonic() + timeout
                while text in screen.text():
                    pump(deadline)

            # Edits made outside Portico appear by themselves, without a key.
            read_screen("alpha")
            with config.open("a") as file:
                file.write("\nHost watched-direct\n    HostName direct.example\n")
            read_screen("watched-direct")
            read_screen("Config changed: reloaded")
            # A new Include is followed, and its file is watched from then on, also when an
            # editor saves it by renaming a new file over it.
            extra.write_text("Host watched-included\n")
            with config.open("a") as file:
                file.write("Include extra.conf\n")
            read_screen("watched-included")
            partial = Path(home, ".ssh/extra.conf.tmp")
            partial.write_text("Host watched-renamed\n")
            os.replace(partial, extra)
            read_screen("watched-renamed")
            wait_gone("watched-included")
            # Under an open form the list stays as it is and says why; leaving the form applies it.
            os.write(master, b"a")
            read_screen("Enter save")
            with config.open("a") as file:
                file.write("Host watched-later\n")
            read_screen("the list reloads when this closes")
            assert "watched-later" not in screen.text()
            os.write(master, b"\x1b[27u")
            read_screen("watched-later")
            # Portico's own save is read back at once, and the watch takes no notice of it.
            os.write(master, b"a")
            read_screen("Enter save")
            os.write(master, b"\x1b[200~own\x1b[201~\t\x1b[200~own.example\x1b[201~\r")
            read_screen("Saved own")
            time.sleep(2)
            assert "Saved own" in screen.text() and "Config changed" not in screen.text()
        elif scenario == "connection_test":
            # The fake ssh answers after a second, so the test is visibly in flight first.
            os.write(master, b"t")
            read_screen("Testing dev\u2026 (Esc to cancel)")
            read_screen("\u2713 dev")
            assert "Testing dev" not in screen.text()
        elif scenario == "tabs":
            read_screen("1 Hosts")
            # The Containers tab lists this machine only once it is shown.
            time.sleep(0.3)
            assert not Path(home, "docker-called").exists(), "listed before the tab was shown"
            os.write(master, b"2")
            read_screen("Tunnels /")
            os.write(master, b"3")
            read_screen("Target: this machine")
            deadline = time.monotonic() + 5
            while not Path(home, "docker-called").exists():
                assert time.monotonic() < deadline, "the Containers tab listed nothing"
                time.sleep(0.01)
            os.write(master, b"\t")
            read_screen("Hosts /")
            # A digit in the filter is text, not a tab switch.
            os.write(master, b"/2")
            read_screen("Hosts /2")
            os.write(master, b"\r")
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
            assert launched[-4:] == ["connect", "--hold", "--", "alpha"], launched
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
        status = process.wait(timeout=5)
        assert status == 0, f"{scenario}: exit status {status}: {transcript!r}"
        assert termios.tcgetattr(slave) == before, f"{scenario}: termios was not restored"
        assert b"\x1b[?1049l" in transcript[start:]
        if scenario == "blocked_connection":
            shell, parent, worker, child = map(int, Path(home, "connection-started").read_text().split())
            for pid in [shell, parent, worker, child]:
                deadline = time.monotonic() + 1
                while is_running(pid):
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
        if scenario == "interrupt_connection":
            Path(home, "interrupt-connection").unlink(missing_ok=True)
            Path(home, "interrupt-ready").unlink(missing_ok=True)
        if scenario == "refresh":
            Path(home, "resolved-hostname").unlink(missing_ok=True)
            Path(home, ".ssh/portico/config").chmod(0o600)
        if scenario == "live_reload":
            config = Path(home, ".ssh/config")
            config.write_text(config.read_text().split("\nHost watched-direct\n")[0])
            Path(home, ".ssh/extra.conf").unlink(missing_ok=True)
            Path(home, ".ssh/extra.conf.tmp").unlink(missing_ok=True)
            Path(home, ".ssh/portico/config").write_text(managed_before)
        if scenario == "launcher_failure":
            Path(home, "fail-launcher").unlink(missing_ok=True)
        if scenario in ["launcher_failure", "ctrl_enter"]:
            Path(home, "launcher-args").unlink(missing_ok=True)
        if held_lock is not None:
            held_lock.close()
        os.close(master)
        os.close(slave)


scenarios = requested or ["paste", "paste_filter_edit", "blocked_resolution", "blocked_mutation_lock", "blocked_init_lock", "blocked_connection", "interrupt_connection", "input_rules", "launcher_failure", "ctrl_enter", "refresh", "live_reload", "legacy_keyboard", "responsive_navigation", "normal_exit", "child_failure", signal.SIGINT, signal.SIGTERM, signal.SIGHUP]
for scenario in scenarios:
    check_terminal(scenario)
