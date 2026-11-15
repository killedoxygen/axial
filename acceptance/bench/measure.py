#!/usr/bin/env python3
"""Measure the running application API; retain each response and timing as JSONL."""
import argparse, datetime, json, pathlib, time, urllib.error, urllib.request, uuid

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--base", required=True)
parser.add_argument("--output", type=pathlib.Path, required=True)
parser.add_argument("--samples", type=int, default=40)
parser.add_argument("--instance")
parser.add_argument("--launches", type=int, default=0)
parser.add_argument("--settle-seconds", type=float, default=15)
args = parser.parse_args()
if not 1 <= args.samples <= 200 or not 0 <= args.launches <= 20:
    parser.error("samples must be 1..200; launches must be 0..20")
args.output.parent.mkdir(parents=True, exist_ok=True)
capability = None

def request(method, path, body=None):
    headers = {"Content-Type": "application/json"}
    if capability:
        headers["x-axial-capability"] = capability
    if path.endswith("transport/bootstrap"):
        headers["Origin"] = args.base
    req = urllib.request.Request(args.base + path,
        data=None if body is None else json.dumps(body).encode(),
        headers=headers, method=method)
    begin = time.perf_counter_ns()
    try:
        with urllib.request.urlopen(req, timeout=180) as response:
            code, raw = response.status, response.read()
    except urllib.error.HTTPError as error:
        code, raw = error.code, error.read()
    elapsed = (time.perf_counter_ns() - begin) / 1e6
    return code, json.loads(raw), elapsed

_, boot, _ = request("POST", "/api/v1/transport/bootstrap", {})
capability = boot["capability"]
with args.output.open("a", buffering=1) as output:
    def record(kind, index, code, body, elapsed):
        value = {"at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
            "kind": kind, "index": index, "http_status": code,
            "elapsed_ms": elapsed, "response": body}
        output.write(json.dumps(value, separators=(",", ":")) + "\n")
        print(kind, index, code, round(elapsed, 3) if elapsed is not None else "untimed", flush=True)
    for index in range(args.samples):
        code, body, elapsed = request("GET", "/api/v1/instances")
        record("instances", index, code, body, elapsed)
        if code != 200:
            raise RuntimeError("instance-list refused; evidence retained")
    if args.launches and not args.instance:
        parser.error("--launches requires --instance")
    for index in range(args.launches):
        session = None
        intent_key = str(uuid.uuid4())
        record("launch_request", index, 0, {"intent_key": intent_key, "instance_id": args.instance}, 0)
        reconciled = False
        try:
            try:
                code, state, elapsed = request("POST", "/api/v1/launch", {
                    "instance_id": args.instance, "intent_key": intent_key})
            except (OSError, TimeoutError):
                deadline = time.monotonic() + 120
                while time.monotonic() < deadline:
                    code, intent, elapsed = request("GET", f"/api/v1/launch/intents/{intent_key}")
                    record("intent_reconciliation", index, code, intent, elapsed)
                    if code == 200 and intent.get("state") == "accepted":
                        state = intent["session"]
                        session = state["session_id"]
                        reconciled = True
                        break
                    if code == 200 and intent.get("state") in ("rejected", "interrupted"):
                        raise RuntimeError("launch intent refused; evidence retained")
                    time.sleep(.25)
                else:
                    raise RuntimeError("launch outcome unknown; retain intent and do not relaunch")
            record("launch_reconciled" if reconciled else "launch_admission", index, code, state, None if reconciled else elapsed)
            if code != 200:
                raise RuntimeError("launch refused; evidence retained")
            session = state["session_id"]
            deadline = time.monotonic() + 120
            while not state.get("boot_observed") and not state.get("view_model", {}).get("terminal"):
                if time.monotonic() >= deadline:
                    raise RuntimeError("startup observation timed out")
                time.sleep(.25)
                _, state, _ = request("GET", f"/api/v1/launch/{session}/status")
            record("boot_observed_not_title_ready", index, 200, state, 0)
            if state.get("process_alive"):
                time.sleep(args.settle_seconds)
        finally:
            if session:
                try:
                    code, stopped, elapsed = request("POST", f"/api/v1/launch/{session}/kill", {})
                    record("owner_stop", index, code, stopped, elapsed)
                except (OSError, TimeoutError):
                    record("stop_response_lost", index, 0, {"session_id": session}, 0)
                deadline = time.monotonic() + 60
                while time.monotonic() < deadline:
                    _, state, _ = request("GET", f"/api/v1/launch/{session}/status")
                    if state.get("view_model", {}).get("terminal") and state.get("tree_settled") and state.get("output_drained"):
                        break
                    time.sleep(.25)
                else:
                    record("unsettled", index, 200, state, 0)
                    raise RuntimeError("original owner has not settled; stop further launches")
                record("settled", index, 200, state, 0)
                code, reports, elapsed = request("GET", f"/api/v1/launch/reports/{session}")
                record("reports", index, code, reports, elapsed)
