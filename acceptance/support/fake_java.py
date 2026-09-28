#!/usr/bin/env python3
"""A bounded process fixture, never a replacement for a real JVM acceptance run.

Set AXIAL_FAKE_JAVA to a JSON object. Launch arguments are otherwise ignored.
Children inherit pipes and the process group, deliberately exercising tree cleanup.
"""

import json
import os
import signal
import subprocess
import sys
import time


def integer(config, key, default, maximum):
    value = config.get(key, default)
    if type(value) is not int or not 0 <= value <= maximum:
        raise ValueError(key)
    return value


def configuration():
    raw = os.environ.get("AXIAL_FAKE_JAVA", "{}")
    if len(raw.encode()) > 65536:
        raise ValueError("configuration too large")
    config = json.loads(raw)
    allowed = {"java_version", "vendor", "arch", "events", "exit_code", "stall_ms",
               "descendant_depth", "descendant_stall_ms", "ignore_sigterm",
               "descendants_ignore_sigterm", "report_lifecycle", "echo_arguments"}
    if not isinstance(config, dict) or set(config) - allowed:
        raise ValueError("configuration fields")
    for key in ("ignore_sigterm", "descendants_ignore_sigterm", "report_lifecycle", "echo_arguments"):
        if key in config and type(config[key]) is not bool:
            raise ValueError(key)
    for key, default, maximum in (("exit_code", 0, 255), ("stall_ms", 0, 60000),
                                  ("descendant_depth", 0, 8), ("descendant_stall_ms", 30000, 60000)):
        integer(config, key, default, maximum)
    for key, default in (("java_version", "17.0.12"), ("vendor", "Fixture OpenJDK"), ("arch", "amd64")):
        value = config.get(key, default)
        if not isinstance(value, str) or not value or len(value) > 128 or any(ord(c) < 32 for c in value):
            raise ValueError(key)
    events = config.get("events", [])
    if not isinstance(events, list) or len(events) > 1024:
        raise ValueError("events")
    total_bytes = 0
    total_delay = 0
    for event in events:
        if not isinstance(event, dict) or set(event) - {"stream", "text", "delay_ms", "repeat"}:
            raise ValueError("event fields")
        if event.get("stream", "stdout") not in ("stdout", "stderr") or not isinstance(event.get("text"), str):
            raise ValueError("event output")
        total_bytes += len(event["text"].encode()) * integer(event, "repeat", 1, 1048576)
        total_delay += integer(event, "delay_ms", 0, 60000)
    if total_bytes > 64 * 1024 * 1024 or total_delay > 60000:
        raise ValueError("event limits")
    return config


def lifecycle(config, event, depth):
    if config.get("report_lifecycle", False):
        print("AXIAL_FAKE_JAVA " + json.dumps({"event": event, "pid": os.getpid(),
              "parent_pid": os.getppid(), "depth": depth}), file=sys.stderr, flush=True)


def jvm_options(arguments):
    """Read launcher options, stopping before the main class or launch target."""
    options = []
    arguments = iter(arguments)
    for argument in arguments:
        if not argument.startswith("-") or argument in ("-jar", "-m", "--module", "--") or argument.startswith("--module="):
            break
        options.append(argument)
        if argument in ("-cp", "-classpath", "--class-path", "-p", "--module-path",
                        "--upgrade-module-path", "--add-modules", "--limit-modules",
                        "--add-exports", "--add-opens", "--add-reads", "--patch-module", "--source"):
            next(arguments, None)
    return options


def run(config):
    child_mode = len(sys.argv) == 3 and sys.argv[1] == "--fixture-descendant"
    options = [] if child_mode else jvm_options(sys.argv[1:])
    version_option = next((arg for arg in options if arg in ("-version", "--version")), None)
    if version_option is not None:
        version = config.get("java_version", "17.0.12")
        vendor = config.get("vendor", "Fixture OpenJDK")
        stream = sys.stdout if version_option == "--version" else sys.stderr
        print(f'openjdk version "{version}" 2024-07-16\n{vendor} Runtime Environment\nOpenJDK 64-Bit Server VM', file=stream)
        if "-XshowSettings:properties" in options:
            print(f'Property settings:\n    java.version = {version}\n    java.vendor = {vendor}\n    os.arch = {config.get("arch", "amd64")}', file=sys.stderr)
        return 0
    depth = int(sys.argv[2]) if child_mode else config.get("descendant_depth", 0)
    if not 0 <= depth <= 8:
        raise ValueError("descendant depth")
    ignore = config.get("descendants_ignore_sigterm" if child_mode else "ignore_sigterm", False)
    if ignore:
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
    lifecycle(config, "started", depth)
    child = None
    if depth:
        child = subprocess.Popen([sys.executable, os.path.abspath(__file__), "--fixture-descendant", str(depth - 1)])
    if not child_mode:
        if config.get("echo_arguments", False):
            print(json.dumps({"arguments": sys.argv[1:]}), flush=True)
        for event in config.get("events", []):
            time.sleep(event.get("delay_ms", 0) / 1000)
            stream = sys.stdout if event.get("stream", "stdout") == "stdout" else sys.stderr
            for _ in range(event.get("repeat", 1)):
                stream.write(event["text"])
            stream.flush()
    time.sleep(config.get("descendant_stall_ms", 30000) / 1000 if child_mode else config.get("stall_ms", 0) / 1000)
    lifecycle(config, "exiting", depth)
    # An exited parent with pipe-owning descendants is an intentional scenario.
    # Reap only a child already finished; the application must own tree shutdown.
    if child is not None:
        child.poll()
    return 0 if child_mode else config.get("exit_code", 0)


if __name__ == "__main__":
    try:
        sys.exit(run(configuration()))
    except (ValueError, TypeError, KeyError, json.JSONDecodeError):
        print("Invalid synthetic Java scenario", file=sys.stderr)
        sys.exit(64)
