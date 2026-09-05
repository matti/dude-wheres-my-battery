# Dude, where's my battery?

A recording-first battery investigation tool for Apple Silicon macOS. Launch it,
use your Mac normally, and see both the current load and the consumers that have
accumulated CPU energy during this recording. Every sample is saved before the
screen updates. No sudo, server, browser, or background daemon.

```sh
./battery                         # build if needed; record + live terminal view
./battery --headless --duration 300
./battery report                  # summarize the latest recording
./battery report recordings/session-….jsonl.gz
./battery sessions
./battery inspect 12345           # 3-second stack sample of a chosen process
```

Requires an Apple Silicon Mac, Xcode Command Line Tools and Rust (edition 2024).
Tested on Apple M5 Pro / macOS 26. The launcher builds an optimized binary into
`target/release/battery`; `cargo install --path .` also works. With the launcher,
relative paths are relative to this repository; an installed binary uses your
current directory. `--dir PATH` changes the recording directory.

## How to actually find the drain

1. Run `./battery` **on battery**. Give it a few minutes of normal use. The 60s
   battery average and session Wh quantify the problem. `s` switches between
   individual processes now and accumulated CPU energy grouped by app/executable.
2. Follow sustained consumers, not only momentary CPU spikes. Arrow keys select
   a row; Enter shows its path, parent PID, owner, GPU hint and I/O context.
   `./battery inspect PID` in a second terminal captures stacks for further
   investigation. Sampling may be denied by macOS; the command reports failure.
3. Change **one thing**. Press `m`, describe it (for example “dimmed display” or
   “closed Chrome”), press Enter and immediately make that change. The recorder
   keeps running while you type. Wait 75 seconds before making another change.
4. The comparison uses 60s before the marker, skips 15s for settling and measures
   60s after that. It requires sufficient on-battery coverage on both sides.
   Reverse the change and repeat. A consistent wattage reduction is much more
   useful evidence than a guessed per-app percentage. Background activity can
   still confound any single comparison; this is an association, not proof.
5. Press `q` or Ctrl-C. The recording stays on disk and the summary prints.
   `./battery report` reconstructs the same totals and comparisons later.

If CPU energy is low but total drain is high, test display brightness, external
peripherals, rendering/video and network workloads individually. If the Mac
should be sleeping, inspect the displayed sleep assertions and their owners.
An assertion identifies who requested that the Mac remain awake; it does not
measure the energy that request caused.

## What the numbers mean

- **Battery W / Wh**: pack voltage × signed current from AppleSmartBattery;
  watts are integrated with the trapezoidal rule between observed endpoints.
  This is sampled battery drain, not an exact coulomb-counter energy total.
  The first sample is a baseline. AC, charging, missing readings and intervals
  over max(10s, 3× sampling period) are excluded. A sleep/wake gap is not filled
  with invented watts. This version does not account for energy used asleep.
- **CPU W / CPU Wh**: deltas of `ri_energy_nj` for readable processes. This is
  **CPU energy, not all energy attributable to an application**. GPU, display,
  radios and indirect work in other processes are not included. Session totals
  start at zero; they never import energy consumed before recording started.
  Exited processes remain in the accumulated totals. Processes entirely between
  samples, and activity before a new process's first baseline, can be missed.
- **Coverage**: number of processes with a readable interval CPU-energy counter
  out of all listed processes. Other users' processes are generally unreadable.
  `—` means unknown, not zero. A supported counter may also remain zero on some
  OS/hardware combinations. CPU time remains a separate corroborating signal.
- **Chip model**: IOReport energy-model channels; CPU/GPU and the aggregate are
  model estimates. Channel names and coverage depend on the chip/OS. The tool
  does not force them to sum to battery power and does not label the difference
  as “screen watts.” **SMC system W** is an independent sensor cross-check, also graphed in yellow
  while on AC (battery drain is cyan).
- **GPU last submitter**: one snapshot per sample, only a hint about a recent GPU
  client. It is not a measure of that process's GPU utilization or wattage.
- **Wake/s, RAM, I/O**: interval wakeups, physical memory footprint and disk
  counters provide context, not conversions into watts. System network counters
  can include virtual interfaces; do not interpret them as radio energy.
- **Grouping**: first `.app` bundle in the executable path, otherwise executable
  name. Separate CLI jobs with the same name are combined; the current-process
  view and raw recording retain individual identities. PID start time protects
  interval energy deltas against PID reuse. Short-lived cross-uid identities
  and exec/reparenting remain best-effort.

Default interval: 2s (`--interval 1` through `--interval 60`). The screen redraws
only on samples or input. The recorder itself is labelled in the process table;
its observed CPU energy is included. `sampler ms` measures the collection's wall
time, not the total overhead including rendering and writes.

## Recordings

`recordings/session-TIMESTAMP-PID.jsonl.gz` is gzip-compressed, versioned newline-delimited JSON:
`session`, `sample`, `context`, `marker`, `notice`, and `end` events. Every sample
contains all observed process rows, subsystem readings, elapsed time and gap
status. Sleep assertions are captured every 30s. Each event is stored as an independent gzip member, flushed and
synced; disk-write failure stops the program visibly. `report` can read a live
file and ignores an incomplete final line/gzip member left by an interrupted write; corrupt
complete records and unknown schema versions are errors.

Data stays local. Records contain process names, executable paths and sleep
assertion descriptions. Stack captures may include additional process details.
There is no automatic retention/deletion. A test with ~830 processes used about
80 MB/hour at the default interval; size varies with process count and activity.
Both gzip and uncompressed JSONL recordings can be read by `report`. Standard
`gzip -dc FILE` extracts the JSONL for other tools.
No process is terminated, suspended or automatically sampled by the recorder.

## Development and provenance

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --release
```

The low-level sampler modules, shared types and host helpers were adapted from
Matti Paksula's MIT-licensed `~/dev/power`, revision
`234179c8084d1ad222a6c1c451f5f191e5af7e4e`. They are included here so the tool does
not depend on a sibling checkout. Recording, interval accounting, experiments,
reports and UI are separate. The grouping approach also follows `~/dev/memtop`.
The IOReport bindings in `power` derive from the MIT-licensed macmon bindings.

CPU-energy semantics: [Apple XNU Recount documentation](https://github.com/apple-oss-distributions/xnu/blob/main/doc/observability/recount.md).
Private IOReport/SMC interfaces and the inherited `kinfo_proc` layout can change;
readings on other hardware need validation. See [LICENSE](LICENSE).

## Inside a virtual machine

When a VM appears, the tool starts its deep probe automatically after the first
host sample. The main process list stays visible: the VM row expands in place
with indented containers and Linux processes. You do not need to find a row or
press Enter. The recording continues while a worker identifies the VM from its
open disk images, then samples the VM's own Docker socket and Linux `/proc`
counters. The view refreshes 15s after each completed probe. **Tab** switches between
Linux processes; **Esc** returns to the host. Container rows include CPU, memory,
image and Compose project/service. Linux rows show CPU over a two-second interval
and the owning container, joined by the full cgroup container ID. An unmapped or
unreadable cgroup is labelled explicitly. The probe itself is labelled in reports.
The inline tree shows active guest processes (at least 1% CPU) and replaces the
idle remainder with one collapsed count, so sleeping Linux kernel threads cannot
bury the useful rows.

`./battery inspect 1298` performs the same probe once and saves `recordings/vm-*.json`.
For ordinary processes, `inspect` still captures host stacks. Completed interactive
probes are stored as `vm` events in the session; `report` includes the latest probe
for each inspected VM. Docker and Linux intervals overlap but are not identical,
so their percentages should not be subtracted to calculate virtualization overhead.
No per-container battery watts are invented from CPU shares.

Supported: conventional Colima profiles and Lima instances (requires `limactl`
and Python 3 already in the guest); Docker Desktop container metrics via its
local user socket. No guest packages are installed. Unrecognized/ambiguous VM
identities, missing tools, unavailable Docker sockets and guest permission errors
are reported. The global Docker context is never used as a fallback; it might
refer to a completely different machine. Probes query existing instances only,
use no sudo, and do not start/stop containers. Network/disk figures from Docker
are lifetime I/O totals, not rates. Stopping a recording cancels outstanding
probe commands; a cancelled probe can be recorded with an availability error.

Reference: [Docker stats](https://docs.docker.com/reference/cli/docker/container/stats/),
[Lima shell](https://lima-vm.io/docs/reference/limactl_shell/).
