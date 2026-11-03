# CHV Load Testing

This document describes how to run load tests against the CHV backend-for-frontend (BFF) using [`oha`](https://github.com/hatoo/oha).

## Prerequisites

1. **CHV services are running** locally (e.g., via `docker-compose up`).
2. The **BFF is reachable** at `http://localhost:8080` (or your custom `BFF_URL`). The control plane serves the BFF on `127.0.0.1:8080` by default.
3. [`oha`](https://github.com/hatoo/oha) is installed (for example, via `cargo install oha`).
4. (Optional) [`jq`](https://jqlang.github.io/jq/) is installed for robust JSON parsing. The script falls back to `grep` if `jq` is missing.

## Quick Start

The BFF listens on port `8080`:

```bash
./scripts/load-test.sh
```

## Environment Variables

| Variable    | Default                      | Description                     |
|-------------|------------------------------|---------------------------------|
| `BFF_URL`   | `http://localhost:8080`      | Base URL of the BFF              |
| `CHV_USER`  | `admin`                      | Username for JWT login          |
| `CHV_PASS`  | *(none)*                     | Password for JWT login. On a local install, read from `/etc/chv/initial_admin_password`. |
| `DURATION`  | `30s`                        | How long each endpoint is hit   |
| `CONNECTIONS`| `50`                        | Number of concurrent connections|
| `RPS`       | `100`                        | Target requests per second      |

### Examples

Run with custom credentials and higher load:

```bash
CHV_USER=myuser CHV_PASS=mypass DURATION=60s CONNECTIONS=100 RPS=500 ./scripts/load-test.sh
```

Run against a remote instance:

```bash
BFF_URL=https://chv.example.com ./scripts/load-test.sh
```

## Interpreting `oha` Output

`oha` prints a text summary after each test. Key fields to watch:

| Metric            | What it means                                            |
|-------------------|----------------------------------------------------------|
| **Success rate**  | Percentage of non-error responses. Should be near 100%.  |
| **Total RPS**     | Actual requests per second achieved.                     |
| **Latency (p50)** | 50th percentile — typical response time.                 |
| **Latency (p99)** | 99th percentile — worst-case for most requests.          |
| **Latency (max)** | Absolute slowest response.                               |

### Red Flags

- **Success rate < 100%** → Errors (5xx, 4xx, or timeouts). Check BFF logs.
- **p99 latency spikes** → Resource contention (CPU, DB locks, thread pool exhaustion).
- **RPS << target** → The server cannot keep up; decrease target or scale out.

## Expected Baseline (Local SQLite)

On a local developer machine using the default SQLite-backed control plane:

| Endpoint      | Expected p50 | Expected p99 | Notes                                |
|---------------|--------------|--------------|--------------------------------------|
| `POST /v1/vms`      | 5–15 ms      | 20–50 ms     | Lightweight list query               |
| `POST /v1/nodes`    | 5–15 ms      | 20–50 ms     | Lightweight list query               |
| `POST /v1/volumes`  | 5–15 ms      | 20–50 ms     | Lightweight list query               |
| `POST /v1/networks` | 5–15 ms      | 20–50 ms     | Lightweight list query               |
| `POST /v1/overview` | 10–30 ms     | 50–100 ms    | Aggregated dashboard data            |

These are **rough guidelines** on modest hardware. Your exact numbers vary with CPU, disk speed, and concurrent load.

## Testing Specific Endpoints Only

The script runs all five endpoints in sequence. To test a single endpoint, copy the `run_test` invocation directly:

```bash
# 1. Obtain a token (same logic as the script)
ADMIN_PASS=$(sudo cat /etc/chv/initial_admin_password 2>/dev/null || echo "YOUR_ADMIN_PASSWORD")
TOKEN_RESPONSE=$(curl -s -X POST "http://localhost:8080/api/v1/auth/login" \
  -H "Content-Type: application/json" \
  -d "{\"username\":\"admin\",\"password\":\"${ADMIN_PASS}\"}")
TOKEN=$(echo "$TOKEN_RESPONSE" | jq -r '.token // .access_token // empty')

# 2. Run oha against one endpoint
oha --no-tui -z 30s -c 50 -q 100 -m POST \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d "{}" \
  "http://localhost:8080/v1/overview"
```

## Troubleshooting

| Problem                        | Likely Cause & Fix                                        |
|--------------------------------|-----------------------------------------------------------|
| `BFF is not reachable`         | CHV services are not running. Start `docker-compose up`.  |
| `Failed to get token`          | Wrong credentials or BFF auth service is unhealthy.       |
| `oha: command not found`       | `oha` is not on `PATH`. Install it with `cargo install oha`, or add Cargo's bin directory to `PATH`. |
| Very high latencies / timeouts | SQLite contention under heavy load. Reduce `CONNECTIONS`. |
