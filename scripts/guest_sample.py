"""Read-only two-snapshot Linux CPU probe. Executed in the identified VM.

No installation, sudo, command-line arguments, environment reads or file writes.
CPU percentages use one logical CPU = 100%. Exited/new processes without two
matching start-time snapshots are not assigned guessed interval CPU usage.
"""
import json
import os
import re
import time
from pathlib import Path


def stat_line(text):
    end = text.rfind(")")
    fields = text[end + 2:].split()
    return {"name": text[text.find("(") + 1:end], "ticks": int(fields[11]) + int(fields[12]),
            "start": int(fields[19]), "rss_bytes": max(0, int(fields[21])) * os.sysconf("SC_PAGE_SIZE"),
            "ppid": int(fields[1])}


def snapshot():
    with open("/proc/stat") as f:
        total = [int(v) for v in f.readline().split()[1:9]]
    procs = {}
    for path in Path("/proc").iterdir():
        if not path.name.isdigit():
            continue
        try:
            procs[int(path.name)] = stat_line((path / "stat").read_text())
        except (OSError, ValueError, IndexError):
            continue
    return time.monotonic(), total, procs


def sample(seconds=2):
    first_t, first_cpu, first = snapshot()
    time.sleep(seconds)
    last_t, last_cpu, last = snapshot()
    dt = last_t - first_t
    hz = os.sysconf("SC_CLK_TCK")
    total = sum(b - a for a, b in zip(first_cpu, last_cpu))
    idle = (last_cpu[3] - first_cpu[3]) + (last_cpu[4] - first_cpu[4])
    rows = []
    missing_cgroups = 0
    for pid, p in last.items():
        old = first.get(pid)
        if not old or old["start"] != p["start"] or p["ticks"] < old["ticks"]:
            continue
        cpu = (p["ticks"] - old["ticks"]) / hz / dt * 100
        try:
            cgroup = Path(f"/proc/{pid}/cgroup").read_text().strip()
        except OSError:
            cgroup = None
            missing_cgroups += 1
        match = re.search(r"(?:^|[/:-])([0-9a-f]{64})(?:\.scope)?(?:/|$)", cgroup or "", re.M)
        rows.append({"pid": pid, "ppid": p["ppid"], "start_ticks": p["start"], "name": p["name"],
                     "cpu_percent": cpu, "rss_bytes": p["rss_bytes"], "container_id": match[1] if match else None,
                     "cgroup": cgroup, "probe": pid == os.getpid()})
    rows.sort(key=lambda p: p["cpu_percent"], reverse=True)
    return {"interval_s": dt, "logical_cpus": os.cpu_count(),
            "busy_percent": 100 * (total - idle) / total if total > 0 else None,
            "observed_processes": len(last), "measured_processes": len(rows),
            "unreadable_cgroups": missing_cgroups, "processes": rows}


if __name__ == "__main__":
    print(json.dumps(sample()))
