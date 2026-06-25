#!/usr/bin/env python3
"""
canbridge.py — forward CAN frames bidirectionally between two SocketCAN interfaces.

A userspace alternative to `cangw` for environments where cangw is unavailable —
e.g. unprivileged LXC/Proxmox containers, where can-gw's netlink handler checks
CAP_NET_ADMIN against the *host* user namespace and rejects the request, even
though `ip link add ... type vcan` works inside the container.

CAN FD: enabled by default (`fd=True`), so FD frames (BRS/ESI, up to 64-byte
payloads) are forwarded too. Pass --no-fd for a classic-only bridge. Enabling FD
on a classic interface is harmless; it just means no FD frames will arrive.

Loop-safety: each interface uses a single socket with receive_own_messages=False
(the python-can default), so a frame this bridge writes to one side is not read
back from that same side. That is what prevents an infinite forwarding loop
within a single bridge. Bridging an interface to itself would defeat this, so it
is rejected up front.

Multi-bridge hazard: running *two* bridges over the same interface pair (in
either order) re-amplifies every frame forever, because each bridge's sockets do
not recognise the other's frames as their own. We take a per-pair advisory lock
(flock) and refuse to start a duplicate; pass --force to override. Note the lock
cannot catch loops formed across *different* pairs (e.g. a 0-1, 1-2, 2-0
triangle) — avoid such topologies yourself.

Usage:
    ./canbridge.py vcan0 vcan1
    ./canbridge.py vcan0 vcan1 --stats 5      # heartbeat counters every 5s

Requires: python-can  (pip install python-can)
"""

import argparse
import fcntl
import os
import signal
import sys
import tempfile
import threading
import time

try:
    import can
except ImportError:
    sys.exit(
        "error: python-can is required but not installed.\n"
        "install it with:  pip install python-can"
    )


def log(message: str) -> None:
    """Timestamped line to stderr so it interleaves cleanly with frame traffic."""
    print(f"[{time.strftime('%H:%M:%S')}] {message}", file=sys.stderr, flush=True)


class Direction:
    """Per-direction counters and edge-triggered health logging.

    Each pump thread owns exactly one Direction and is its only writer, so the
    plain int counters need no lock; the main thread only ever *reads* them for
    stats (a benign, GIL-atomic stale read).
    """

    def __init__(self, name: str) -> None:
        self.name = name
        self.forwarded = 0
        self.tx_errors = 0
        self.rx_errors = 0
        self._rx_healthy = True
        self._tx_healthy = True

    def note_rx_ok(self) -> None:
        if not self._rx_healthy:
            log(f"{self.name}: receive recovered")
            self._rx_healthy = True

    def note_rx_error(self, exc: BaseException) -> None:
        self.rx_errors += 1
        if self._rx_healthy:
            log(f"{self.name}: receive error: {exc} (suppressing until recovery)")
            self._rx_healthy = False

    def note_tx_ok(self) -> None:
        if not self._tx_healthy:
            log(f"{self.name}: send recovered")
            self._tx_healthy = True

    def note_tx_error(self, exc: BaseException) -> None:
        self.tx_errors += 1
        if self._tx_healthy:
            log(f"{self.name}: send error, frame dropped: {exc} "
                "(suppressing until recovery)")
            self._tx_healthy = False

    def summary(self) -> str:
        return (f"{self.name}: forwarded={self.forwarded} "
                f"tx_errors={self.tx_errors} rx_errors={self.rx_errors}")


def pump(src: can.BusABC, dst: can.BusABC, d: Direction,
         stop: threading.Event, send_timeout: float) -> None:
    """Forward every frame received on `src` to `dst` until `stop` is set.

    Uses a timed recv so the loop periodically re-checks `stop` instead of
    blocking forever. Operational errors (interface down, ENOBUFS, a congested
    tx queue) are logged and counted rather than allowed to kill the thread,
    which previously left one direction silently dead while the bridge looked
    healthy. A finite send timeout keeps a congested destination from wedging
    this direction (and blocking clean shutdown) indefinitely.
    """
    try:
        while not stop.is_set():
            try:
                msg = src.recv(timeout=1.0)
            except (can.CanError, OSError) as exc:
                d.note_rx_error(exc)
                stop.wait(timeout=1.0)  # back off; don't hot-loop on a dead iface
                continue
            d.note_rx_ok()
            if msg is None:
                continue
            try:
                dst.send(msg, timeout=send_timeout)
            except (can.CanError, OSError) as exc:
                d.note_tx_error(exc)
                continue
            d.forwarded += 1
            d.note_tx_ok()
    except Exception as exc:  # never die silently — make the cause visible
        log(f"{d.name}: FATAL, direction stopped: {exc!r}")


def acquire_pair_lock(iface_a: str, iface_b: str):
    """Take an advisory per-pair lock to refuse a duplicate bridge.

    Scoped by uid (avoids cross-user permission clashes on the shared lock dir);
    order-independent so vcan0<->vcan1 and vcan1<->vcan0 collide. The fd is
    returned and must be kept alive for the process lifetime; flock releases
    automatically on exit, so there is no stale-lock problem. Returns None if
    another instance already holds it.
    """
    key = "-".join(sorted((iface_a, iface_b)))
    path = os.path.join(tempfile.gettempdir(), f"canbridge-{os.getuid()}-{key}.lock")
    fd = os.open(path, os.O_CREAT | os.O_RDWR, 0o644)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError:
        os.close(fd)
        return None
    return fd


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Bidirectionally bridge two SocketCAN interfaces in userspace.",
    )
    parser.add_argument("iface_a", help="first SocketCAN interface, e.g. vcan0")
    parser.add_argument("iface_b", help="second SocketCAN interface, e.g. vcan1")
    parser.add_argument("--no-fd", dest="fd", action="store_false",
                        help="open classic-CAN sockets (default: CAN FD enabled)")
    parser.add_argument("--send-timeout", type=float, default=1.0, metavar="SEC",
                        help="per-frame send timeout before dropping (default: 1.0)")
    parser.add_argument("--stats", type=float, default=None, metavar="SEC",
                        help="print forwarding counters every SEC seconds")
    parser.add_argument("--force", action="store_true",
                        help="start even if another bridge holds this pair's lock")
    args = parser.parse_args()

    if args.iface_a == args.iface_b:
        parser.error(
            "the two interfaces must be different "
            "(bridging an interface to itself creates a forwarding loop)"
        )

    lock_fd = acquire_pair_lock(args.iface_a, args.iface_b)
    if lock_fd is None and not args.force:
        sys.exit(
            f"error: another canbridge already bridges "
            f"{args.iface_a} <-> {args.iface_b}.\n"
            "running a second one re-amplifies every frame forever.  "
            "use --force only if you are certain that is not the case."
        )

    bus_a = bus_b = None
    try:
        bus_a = can.Bus(channel=args.iface_a, interface="socketcan",
                        fd=args.fd, receive_own_messages=False)
        bus_b = can.Bus(channel=args.iface_b, interface="socketcan",
                        fd=args.fd, receive_own_messages=False)
    except (OSError, can.CanError) as exc:
        if bus_a is not None:
            bus_a.shutdown()
        sys.exit(
            f"error: could not open interface: {exc}\n"
            "is it up?  check with:  ip link show"
        )

    stop = threading.Event()
    signal.signal(signal.SIGINT, lambda *_: stop.set())
    signal.signal(signal.SIGTERM, lambda *_: stop.set())

    dir_ab = Direction(f"{args.iface_a}->{args.iface_b}")
    dir_ba = Direction(f"{args.iface_b}->{args.iface_a}")
    threads = [
        threading.Thread(target=pump, args=(bus_a, bus_b, dir_ab, stop,
                                            args.send_timeout), daemon=True),
        threading.Thread(target=pump, args=(bus_b, bus_a, dir_ba, stop,
                                            args.send_timeout), daemon=True),
    ]
    for t in threads:
        t.start()

    fd_state = "FD" if args.fd else "classic"
    print(f"bridging {args.iface_a} <-> {args.iface_b} ({fd_state}, Ctrl-C to stop)")

    # Block the main thread until a signal sets the event. The timed wait
    # guarantees we notice the flag even if a signal wakeup is missed, and
    # doubles as the heartbeat tick.
    next_stats = (time.monotonic() + args.stats) if args.stats else None
    while not stop.wait(timeout=0.5):
        if next_stats is not None and time.monotonic() >= next_stats:
            log(f"{dir_ab.summary()} | {dir_ba.summary()}")
            next_stats += args.stats

    for t in threads:
        t.join(timeout=2.0)
    bus_a.shutdown()
    bus_b.shutdown()
    print(f"stopped — {dir_ab.summary()} | {dir_ba.summary()}")


if __name__ == "__main__":
    main()
