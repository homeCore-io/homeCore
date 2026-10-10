# Service startup performance

The startup target is **HTTP API access within 0.5 seconds of Docker's recorded
container start** on the benchmark machine. API access, fresh device readings,
and configured mode initialization are separate milestones. This is a service
restart benchmark, not a measurement from powering on the PC or rendering the
Flutter dashboard.

## Measured results (10 October 2026)

On this PC (Intel Core i5-8600K, WSL2 Ubuntu, two CPUs and 2 GiB per
container), every optimized API start was below 0.5 seconds across 20 measured
restarts. The median improved by 47%. Each service received an excluded warmup,
then baseline/candidate order alternated. Both used identical copied data and
the same HTTP-monitoring Shelly plugin, so the separate WebSocket upgrade did
not influence this comparison.

| Milestone (seconds) | Baseline median | Optimized median | Baseline p95 / max | Optimized p95 / max |
| --- | ---: | ---: | ---: | ---: |
| HTTP API access | 0.491 | 0.260 | 0.546 / 0.582 | 0.293 / 0.308 |
| Authenticated device access | 0.509 | 0.278 | 0.562 / 0.607 | 0.320 / 0.338 |
| Fresh state from both dimmers | 0.630 | 0.322 | 0.738 / 1.748 | 0.483 / 1.326 |
| API access from Docker command launch | 0.523 | 0.292 | 0.578 / 0.640 | 0.336 / 0.359 |

Both services passed all 20 fresh-state and virtual automation checks. After
startup, virtual automation round trips had medians of 0.094 seconds (baseline)
and 0.090 seconds (optimized). Authenticated access in this comparison includes
the Docker inspect call between health and authenticated probes, making that
measurement conservative. The maintained script below times authenticated
access before inspection; a separate two-trial-per-service smoke check passed
with an optimized API maximum of 0.261 seconds.

[Raw observations and summary](benchmarks/startup-2026-10-10.json) include the
excluded warmups, timing definitions and environment. Fresh device state is
network-dependent and did not meet the 0.5-second maximum target; API access did.

## Implementation

- Start the embedded MQTT broker on its thread without the fixed 300 ms sleep.
- On ordinary database reopen, validate existing redb tables using read
  transactions. Create missing tables with a durable write transaction; normal
  state writes retain their existing durability. Avoid ten unchanged startup
  commits and their disk synchronization work.
- Start HTTP independently of MQTT subscription and plugin recovery. Registry
  entries and config centralization are prepared before API access.
- Launch plugins only after the broker acknowledges the internal `homecore/#`
  subscription. A connection acknowledgement or queued subscription alone is
  insufficient; rejected subscriptions do not release the launch barrier.
- Retry an initial broker-bind race after 10 ms, with exponential backoff capped
  at two seconds. Established connection failures retain the two-second retry.

The first HTTP responses can arrive before plugins are connected. Device records
may contain persisted state until fresh readings arrive. The configured mode
startup delay is unchanged and is not represented by the API latency result.

## Benchmark method

Use two stopped containers with separate copies of the same existing data and
configuration. Copy databases while the source service is stopped. Preserve the
live installation and protect copied credentials. In test copies, enable only
the known virtual benchmark automation so restarts cannot trigger physical
lighting rules.

The baseline uses the published `ghcr.io/homecore-io/hc-core:0.1.74` image. The
candidate uses the same image and entrypoint, with the optimized musl release
binary mounted over `/usr/local/bin/homecore`. Both use two CPUs and 2 GiB memory.
Run on Linux; the core uses Unix-only APIs. Build with Rust 1.95:

```sh
cargo build --release -p homecore --locked
```

Create isolated containers with separate `/homecore` mounts and different HTTP
ports. Set `HC_BENCHMARK_TOKEN` privately to an API token present in both data
copies, then run:

```sh
python3 scripts/benchmark-startup.py \
  --baseline hc-boot-baseline --candidate hc-boot-optimized \
  --baseline-url http://127.0.0.1:18081 \
  --candidate-url http://127.0.0.1:18082 \
  --runs 20 --threshold 0.5 --output /tmp/homecore-startup-results \
  --fresh-device-prefix shelly_dimmer_ --expected-devices 2
```

Omit the final line when no Shelly devices are configured. The script reads state
but never sends device commands. It requires stopped test containers and stops
each after its trial. It performs one excluded warmup per container, alternates
baseline/candidate order, polls health every 3 ms, verifies authenticated device
access, and optionally waits for available device readings whose `last_seen`
postdates that container start. The threshold applies to the slowest measured
candidate API start, not just its average.

The JSON output contains per-trial measurements and median, nearest-rank p95,
minimum and maximum. HTTP request completion and polling overhead are included;
Docker CLI launch overhead is reported separately. Warm caches and the existing
database are deliberate: fresh installation/password bootstrap and cold machine
boot are not covered. Run after builds/tests finish to avoid compiler contention.

## Validation

Regression tests cover reopening existing tables while another writer holds the
write lock, creating missing tables without losing durable data, rejecting type
mismatches, waiting for MQTT SUBACK, rejected subscriptions, and a delayed broker
bind. The restart benchmark additionally verifies both Shelly devices publish
fresh readings. A separate virtual source/target switch automation is exercised
after every comparison trial; physical dimmers are only read.
