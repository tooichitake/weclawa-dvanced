# Service Level Objectives

Operators running `weclawbot` in production should set SLOs that
match their commercial commitments to end-users. The numbers below
are **suggested defaults** for an operator selling "WeChat assistant
as a service" — not promises from the upstream project.

## Availability

| Metric                 | Target              | Notes                                  |
|------------------------|---------------------|----------------------------------------|
| `/healthz` 2xx         | 99.5% over 28 days  | ~3.6 h outage budget per 28-day window |
| API 5xx rate           | < 0.1% of requests  | Sustained over 5-min rolling window    |
| Inbound message loss   | < 0.01%             | Counted via inbound webhook ACK rate   |

## Latency

| Metric                          | Target (p50) | Target (p99) | Notes                          |
|---------------------------------|--------------|--------------|--------------------------------|
| Admin API request               | < 50 ms      | < 500 ms     | Locally hosted; mostly DB+log  |
| Inbound → first AI char         | < 8 s        | < 30 s       | Excludes Claude thinking time  |
| Sandbox spawn (cold)            | < 1.5 s      | < 5 s        | runsc + podman + bind mounts   |

## Error budget consumption

Burn rate alerts (per Google SRE playbook):

- **2× burn for 1 h**: page (you've burned 5% of weekly budget in
  60 min).
- **6× burn for 5 min**: page (acute incident — full outage).
- **Steady-state error budget tracker**: ticket alert on the GUI's
  Health & Metrics tab.

## Data durability

- Backups taken nightly; retention of at least 30 dailies + 12
  monthlies off-host.
- RPO (Recovery Point Objective): 24 h (one day's worth of audit log
  and message history can be lost in worst-case restore).
- RTO (Recovery Time Objective): 1 h (time to stop daemon + restore
  snapshot + restart).

## Process

- Quarterly: review SLO numbers vs. actuals.  Loosen if the daemon
  consistently outperforms, tighten if customer complaints suggest
  the target is too lax.
- Every customer-visible outage > 5 min: post-mortem note in
  `RUNBOOK.md` under a dated section.
