# Overload showcase

A reproducible A/B of NGINX → three Next.js origins against NGINX → Tanod → the
same origins, under open-model k6 load. Method, limitations and presentation
guidance: [docs/demo/README.md](../../docs/demo/README.md).

```bash
bench/showcase/run.sh                 # delay mode: a mechanism test
bench/showcase/run.sh --mode cpu      # CPU-bound origin work (opt-in)
bench/showcase/run.sh --check         # validate only, no load
python3 -m unittest discover -s bench/showcase/tests -v
```

| File | Role |
| --- | --- |
| `run.sh` | Orchestrates preflight, build, every case, warmup, drain, cleanup, analysis |
| `compose.yaml` | Origins, optional Tanod, the NGINX edge, optional Prometheus + Grafana |
| `nginx.conf.tpl` | The one NGINX template both arms are rendered from |
| `tanod.yaml.tpl` | Tanod's configuration for the Tanod arm |
| `k6/overload.js`, `k6/lib.js` | Open-model load and response classification |
| `showcase.py` | Parameters, config rendering, sampler, snapshots, analysis (stdlib only) |
| `plot.py` | Charts from measured data only (needs Matplotlib) |
| `observe/` | Prometheus scrape config and the demo Grafana dashboard |
| `tests/` | Configuration and parsing tests |

The existing benchmark scripts in `bench/` are unchanged. `bench/demo.sh` remains
the minimal admission-control check against the Rust slow origin.
