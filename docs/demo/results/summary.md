# Tanod overload showcase — measured results

Generated from `results.json`. Run `20261030-155126` at commit `0175cb2937c4` on Intel(R) Core(TM) i5-9400F CPU @ 2.90GHz (6 logical CPUs).

Mode `delay` is a mechanism test (the origin sleeps; no CPU is saturated). Mode `cpu` does a fixed amount of CPU work per request. Successful latency covers HTTP 200 only.

| Mode | Workload | Arm | Trial | Rate (req/s) | Attempted | Dropped | 200 origin | 200 cache | 503 shed | 429 | 5xx | Timeout | p50 ms | p95 ms | p99 ms | Origin peak | Origin renders | Checks |
|---|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|
| cpu | unique | baseline | 1 | 90 | 5,401 | 633 | 1,475 | 0 | 0 | 0 | 0 | 3,293 | 6,785 | 8,183 | 8,453 | 173 | 3,749 | pass |
| cpu | unique | tanod | 1 | 90 | 5,401 | 0 | 1,892 | 0 | 3,509 | 0 | 0 | 0 | 1,137 | 1,371 | 1,465 | 6 | 1,892 | pass |
| cpu | unique | baseline | 2 | 90 | 5,401 | 681 | 1,468 | 0 | 0 | 0 | 0 | 3,252 | 6,365 | 7,347 | 8,829 | 165 | 3,807 | pass |
| cpu | unique | tanod | 2 | 90 | 5,400 | 0 | 1,962 | 0 | 3,438 | 0 | 0 | 0 | 1,140 | 1,354 | 1,432 | 6 | 1,962 | pass |
| cpu | unique | baseline | 3 | 90 | 5,401 | 613 | 1,325 | 0 | 0 | 0 | 0 | 3,463 | 5,926 | 7,229 | 8,475 | 147 | 3,729 | pass |
| cpu | unique | tanod | 3 | 90 | 5,401 | 0 | 1,900 | 0 | 3,501 | 0 | 0 | 0 | 1,164 | 1,301 | 1,373 | 6 | 1,900 | pass |

## Notes recorded during the run

- cpu/unique/baseline/t1: k6 dropped 633 scheduled iterations (all 2000 VUs busy): that load was offered but never sent
- cpu/unique/baseline/t1: sampler read failures: next-1=21, next-2=21, next-3=21
- cpu/unique/baseline/t2: k6 dropped 681 scheduled iterations (all 2000 VUs busy): that load was offered but never sent
- cpu/unique/baseline/t2: sampler read failures: container:edge=1, container:next-1=1, container:next-2=1, container:next-3=1, next-1=22, next-2=23, next-3=23
- cpu/unique/baseline/t3: k6 dropped 613 scheduled iterations (all 2000 VUs busy): that load was offered but never sent
- cpu/unique/baseline/t3: sampler read failures: next-1=22, next-2=22, next-3=22

Origin peak is the highest total of renders in progress across the three origins, from the origins' own render log (exact) or, without logs, their sampled counters (a lower bound).
