#!/usr/bin/env python3
"""Compare service restart latency in two pre-created, isolated Docker containers.

Requires Python 3.10+, Docker, and HC_BENCHMARK_TOKEN for authenticated reads.
Does not issue device commands. Both containers are stopped after every trial.
"""

import argparse
import datetime
import json
import math
import os
from pathlib import Path
import statistics
import subprocess
import time
import urllib.request


def docker(*args):
    return subprocess.check_output(["docker", *args], text=True).strip()


def timestamp(value):
    return datetime.datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", required=True, help="Isolated baseline container")
    parser.add_argument("--candidate", required=True, help="Isolated candidate container")
    parser.add_argument("--baseline-url", required=True)
    parser.add_argument("--candidate-url", required=True)
    parser.add_argument("--runs", type=int, default=20)
    parser.add_argument("--output", type=Path, required=True, help="New output directory")
    parser.add_argument("--threshold", type=float, default=0.5)
    parser.add_argument("--fresh-device-prefix", help="Also wait for fresh available device readings")
    parser.add_argument("--expected-devices", type=int, default=0)
    args = parser.parse_args()
    if args.runs < 1 or args.threshold <= 0:
        parser.error("runs and threshold must be positive")
    if args.fresh_device_prefix and args.expected_devices < 1:
        parser.error("fresh-device-prefix requires a positive expected-devices count")
    token = os.environ["HC_BENCHMARK_TOKEN"]
    services = {
        "baseline": (args.baseline, args.baseline_url.rstrip("/")),
        "candidate": (args.candidate, args.candidate_url.rstrip("/")),
    }
    if args.baseline == args.candidate:
        parser.error("baseline and candidate must be different containers")
    for name, _ in services.values():
        if json.loads(docker("inspect", name))[0]["State"]["Running"]:
            parser.error(f"{name} is running; use stopped, isolated test containers")
    args.output.mkdir(mode=0o700, parents=True, exist_ok=False)
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def get(url, path, authenticated=False):
        request = urllib.request.Request(
            url + "/api/v1/" + path,
            headers={"Authorization": "Bearer " + token} if authenticated else {},
        )
        with opener.open(request, timeout=0.15) as response:
            return json.load(response)

    observations = []

    def trial(label, index):
        name, url = services[label]
        row = {"service": label, "trial": index, "warmup": index == 0}
        launch_wall = time.time()
        launch = subprocess.Popen(["docker", "start", name], stdout=subprocess.DEVNULL)
        deadline = time.monotonic() + 20
        try:
            while True:
                try:
                    get(url, "health")
                    api_wall = time.time()
                    break
                except (OSError, ValueError):
                    if time.monotonic() >= deadline:
                        raise TimeoutError("API did not become available")
                    time.sleep(0.003)
            get(url, "devices", authenticated=True)
            authenticated_wall = time.time()
            started = timestamp(json.loads(docker("inspect", name))[0]["State"]["StartedAt"])
            row.update(
                api_s=api_wall - started,
                authenticated_access_s=authenticated_wall - started,
                api_from_launch_s=api_wall - launch_wall,
            )
            if args.fresh_device_prefix:
                while time.monotonic() < deadline:
                    devices = [
                        device for device in get(url, "devices", authenticated=True)
                        if device["device_id"].startswith(args.fresh_device_prefix)
                    ]
                    if len(devices) == args.expected_devices and all(
                        device["available"] and timestamp(device["last_seen"]) >= started
                        for device in devices
                    ):
                        row["fresh_device_state_s"] = time.time() - started
                        break
                    time.sleep(0.005)
                else:
                    raise TimeoutError("Fresh device state did not arrive")
            row["success"] = True
        except Exception as error:
            row.update(success=False, error=str(error))
            raise
        finally:
            launch.wait(timeout=10)
            docker("stop", "--time", "20", name)
            observations.append(row)
            with (args.output / "observations.jsonl").open("a") as file:
                file.write(json.dumps(row) + "\n")
            print(json.dumps(row), flush=True)

    for label in services:
        trial(label, 0)
    for index in range(1, args.runs + 1):
        order = ("baseline", "candidate") if index % 2 else ("candidate", "baseline")
        for label in order:
            trial(label, index)
    summary = {}
    for label in services:
        rows = [row for row in observations if row["service"] == label and not row["warmup"]]
        summary[label] = {"runs": len(rows), "successes": sum(row["success"] for row in rows)}
        for field in ("api_s", "authenticated_access_s", "api_from_launch_s", "fresh_device_state_s"):
            values = sorted(row[field] for row in rows if field in row)
            if values:
                summary[label][field] = {
                    "median": statistics.median(values),
                    "p95": values[math.ceil(0.95 * len(values)) - 1],
                    "min": min(values), "max": max(values),
                }
    (args.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary, indent=2), flush=True)
    if summary["candidate"]["api_s"]["max"] >= args.threshold:
        raise SystemExit("Candidate exceeded the API startup threshold")


if __name__ == "__main__":
    main()
