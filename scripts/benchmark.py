#!/usr/bin/env python3
"""Compare release daemons using a private, read-only Herdr API fixture."""
import argparse
import collections
import json
import os
import pathlib
import signal
import socket
import subprocess
import tempfile
import threading
import time


def measure(binary, scenario, seconds):
    with tempfile.TemporaryDirectory(prefix="triggers-bench-") as directory:
        root = pathlib.Path(directory)
        config = root / "config"
        config.mkdir()
        rules = []
        count = 100 if scenario == "dense" else 10
        if scenario != "idle":
            for i in range(count):
                scope = 'pane_id = "^absent$"' if scenario == "unmatched-scope" else (
                    'pane_title = "^monitor$"' if scenario == "title-scope" else 'pane_id = "^w1:"')
                rules.append(f'[[rules]]\nregex = "NEVER_MATCH_{i}:"\nscope = {{ {scope} }}\n'
                             'action = { type = "send_text", text = "fixture" }\n')
        settings = '[settings]\npoll_ms = 50\nlog = "off"\n'
        full_config = settings + '\n'.join(rules)
        (config / "triggers.toml").write_text(settings if scenario == "reload" else full_config)
        panes = [{"pane_id": f"w1:p{i}", "terminal_id": f"terminal-{i}",
                  "tab_id": "w1:t1", "workspace_id": "w1", "title": "shell"}
                 for i in range(32)]
        text = "ordinary fixture output without a prompt\n" * (2000 if scenario == "dense" else 200)
        replies = {"pane.list": {"panes": panes},
                   "tab.list": {"tabs": [{"tab_id": "w1:t1", "label": "[1] monitor"}]},
                   "workspace.list": {"workspaces": [{"workspace_id": "w1", "label": "workspace"}]},
                   "pane.read": {"read": {"text": text}}}
        replies = {k: json.dumps({"id": "req", "result": v}).encode() + b"\n" for k, v in replies.items()}
        counts = collections.Counter()
        errors = []
        stop = threading.Event()
        with socket.socket(socket.AF_UNIX) as server:
            endpoint = str(root / "herdr.sock")
            server.bind(endpoint)
            server.listen()
            server.settimeout(0.1)

            def serve():
                try:
                    while not stop.is_set():
                        try:
                            connection, _ = server.accept()
                        except TimeoutError:
                            continue
                        with connection:
                            connection.settimeout(2)
                            with connection.makefile("rb") as stream:
                                request = json.loads(stream.readline())
                            method = request["method"]
                            if method not in replies:
                                raise AssertionError(f"Unexpected mutating request: {method}")
                            counts[method] += 1
                            connection.sendall(replies[method])
                except Exception as error:
                    errors.append(str(error))

            worker = threading.Thread(target=serve)
            worker.start()
            env = {k: v for k, v in os.environ.items() if not k.startswith("HERDR_")}
            env.update(HERDR_PLUGIN_CONFIG_DIR=str(config), HERDR_PLUGIN_STATE_DIR=str(root / "state"))
            child = subprocess.Popen([binary, "run", "--socket", endpoint], env=env,
                                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            started = time.monotonic()
            peak_rss_kib = None
            reloaded = False
            try:
                while time.monotonic() - started < seconds:
                    if scenario == "reload" and not reloaded and time.monotonic() - started >= seconds / 2:
                        if counts:
                            raise AssertionError("Empty rule set polled the API")
                        (config / "triggers.toml").write_text(full_config)
                        child.send_signal(signal.SIGHUP)
                        reloaded = True
                    try:
                        status_text = pathlib.Path(f"/proc/{child.pid}/status").read_text()
                        if pathlib.Path(f"/proc/{child.pid}/exe").resolve() == pathlib.Path(binary):
                            for line in status_text.splitlines():
                                if line.startswith("VmHWM:"):
                                    peak_rss_kib = max(peak_rss_kib or 0, int(line.split()[1]))
                    except FileNotFoundError:
                        pass
                    time.sleep(0.02)
            finally:
                child.send_signal(signal.SIGTERM)
                _, status, usage = os.wait4(child.pid, 0)
                child.returncode = os.waitstatus_to_exitcode(status)
                stop.set()
                worker.join(timeout=3)
            if child.returncode or errors or worker.is_alive():
                raise RuntimeError(f"daemon={child.returncode}, server={errors}, thread_alive={worker.is_alive()}")
            if scenario == "reload" and not counts["pane.read"]:
                raise AssertionError("Reload did not resume screen polling")
        cpu_ms = (usage.ru_utime + usage.ru_stime) * 1000
        return {"binary": binary, "scenario": scenario, "wall_s": time.monotonic() - started,
                "cpu_ms": cpu_ms, "sampled_peak_rss_kib": peak_rss_kib,
                "cpu_ms_per_poll": cpu_ms / counts["pane.list"] if counts["pane.list"] else None,
                "requests": dict(counts)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", nargs="+")
    parser.add_argument("--samples", type=int, default=3)
    parser.add_argument("--seconds", type=float, default=2)
    parser.add_argument("--verify-reload", action="store_true",
                        help="check that an empty rule set stays idle and SIGHUP activates new rules")
    args = parser.parse_args()
    if args.samples < 1 or args.seconds <= 0:
        parser.error("samples and seconds must be positive")
    binaries = [str(pathlib.Path(p).resolve(strict=True)) for p in args.binaries]
    if args.verify_reload:
        for binary in binaries:
            print(json.dumps(measure(binary, "reload", max(2, args.seconds))), flush=True)
        return
    for sample in range(args.samples):
        for scenario in ("idle", "unmatched-scope", "title-scope", "dense"):
            for binary in (binaries if sample % 2 == 0 else binaries[::-1]):
                print(json.dumps({"sample": sample, **measure(binary, scenario, args.seconds)}), flush=True)


if __name__ == "__main__":
    main()
