#!/usr/bin/env python3
"""PROTOTYPE, throwaway: drive the two-question app through a real pty.

Ctrl-C is sent as byte 0x03 to the pty, so the line discipline delivers
SIGINT to the whole foreground process group, exactly as a terminal does.

usage: drive.py <script> -- <command...>
script is a comma list of steps: "wait:<text>", "send:<text>", "ctrlc",
"sleep:<secs>", "eof", "sigterm" (to the pty's foreground leader).
"""
import os, pty, select, signal, sys, time

def main():
    sep = sys.argv.index("--")
    steps = sys.argv[1].split(",") if sys.argv[1] else []
    cmd = sys.argv[sep + 1:]
    pid, fd = pty.fork()
    if pid == 0:
        os.execvp(cmd[0], cmd)
    out = b""

    def pump(timeout):
        nonlocal out
        r, _, _ = select.select([fd], [], [], timeout)
        if r:
            try:
                data = os.read(fd, 4096)
            except OSError:
                return False
            if not data:
                return False
            out += data
        return True

    for step in steps:
        kind, _, arg = step.partition(":")
        if kind == "wait":
            deadline = time.time() + 10
            while arg.encode() not in out and time.time() < deadline:
                if not pump(0.1):
                    break
        elif kind == "send":
            os.write(fd, arg.encode() + b"\n")
        elif kind == "ctrlc":
            os.write(fd, b"\x03")
        elif kind == "eof":
            os.write(fd, b"\x04")
        elif kind == "sigterm":
            os.kill(pid, signal.SIGTERM)
        elif kind == "sleep":
            end = time.time() + float(arg)
            while time.time() < end:
                pump(0.05)
    while pump(1):
        pass
    _, status = os.waitpid(pid, 0)
    sys.stdout.write(out.decode(errors="replace").replace("\r\n", "\n"))
    if os.WIFSIGNALED(status):
        print(f"<< shell sees: killed by {signal.Signals(os.WTERMSIG(status)).name} >>")
    else:
        print(f"<< shell sees: exit {os.WEXITSTATUS(status)} >>")

main()
