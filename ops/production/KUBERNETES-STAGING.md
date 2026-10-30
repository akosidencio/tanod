# Validating the Kubernetes example on a staging cluster

`kubernetes.yaml` is schema-checked in CI. That says nothing about how a real
cluster replaces its pods, routes around a not-ready one, or enforces the
NetworkPolicy. These checks do. Run them in a disposable namespace on a
cluster whose CNI enforces NetworkPolicy, with a staging origin you may load.
Record every command's output; a step that could not run is a gap, not a pass.

**Status for 0.4.0: NOT EXECUTED.** No cluster belonging to this project was
available; the only kubeconfig on the validation machine belongs to another
project and was not used. The manifest and configuration were checked
statically (see `PHASE4.2-DEPLOYMENT-VALIDATION`), which certifies nothing about
a cluster's behaviour.

Use a dedicated kubeconfig for the staging cluster and nothing else, so no
command below can reach another cluster:

```sh
export KUBECONFIG=$PWD/staging.kubeconfig
kubectl config current-context        # must name the Tanod staging cluster
```

Record cluster events and endpoint changes with timestamps for the whole run,
in a second terminal, and keep the files:

```sh
kubectl -n "$NS" get events -w -o json > events.jsonl &
kubectl -n "$NS" get endpointslices -l kubernetes.io/service-name=tanod -w -o json > endpoints.jsonl &
kubectl -n "$NS" get pods -l app=tanod -w -o wide --output-watch-events > pods.txt &
```

Throughout: `NS=tanod-staging`, `kubectl` pointed at the staging cluster, and an
origin Service `next-origin:3000` whose renders count themselves. `slow-origin`
from this repository is suitable: its `/__stats` reports `in_flight` and
`peak`, which are the witnesses below.

## 0. Deploy

```sh
kubectl create namespace "$NS"
kubectl -n "$NS" create configmap tanod-config \
  --from-file=tanod.yaml=ops/production/kubernetes-tanod.yaml
kubectl -n "$NS" create secret generic tanod-admin \
  --from-literal=purge-token="$(openssl rand -hex 24)"
sed "s#tanod:review-candidate#$TANOD_IMAGE_DIGEST#" ops/production/kubernetes.yaml \
  | kubectl -n "$NS" apply -f -
kubectl -n "$NS" rollout status statefulset/tanod --timeout=5m
```

**Pass:** `tanod-0` then `tanod-1` become Ready, in that order.

## 1. Capacity never overlaps during a rollout

The two identities share an eight-unit origin budget, four each. A rollout
must never run a new pod beside the old one it replaces, and an old pod's
admitted work — including responses being drained for disconnected clients —
must end before its replacement starts admitting.

```sh
# Load that saturates both partitions and abandons a share of requests:
kubectl -n "$NS" run load --image=curlimages/curl --restart=Never -- sh -c '
  while true; do
    for i in $(seq 1 24); do
      curl -s -o /dev/null --max-time 0.05 http://tanod:8080/bigstream/4?u=$RANDOM &
      curl -s -o /dev/null http://tanod:8080/cold/$RANDOM &
    done; wait
  done'
curl -s http://next-origin:3000/__reset        # from a pod in the namespace
kubectl -n "$NS" rollout restart statefulset/tanod
kubectl -n "$NS" get pods -w -l app=tanod      # in another terminal
kubectl -n "$NS" rollout status statefulset/tanod --timeout=10m
curl -s http://next-origin:3000/__stats
```

**Pass:** `peak` ≤ 8 across the whole rollout; at no point are three `tanod-*`
pods Running or Terminating at once; each pod's last log lines show it drained
(`tanod_origin_disconnect_drains_active` at 0 before exit) rather than being
killed at `terminationGracePeriodSeconds`.

`shutdown_timeout` (35s) must exceed `timeouts.origin` (30s): a pod killed
mid-drain leaves origin work running that nothing accounts for.

## 2. Readiness withdrawal reaches the Service before listeners close

```sh
kubectl -n "$NS" exec tanod-1 -- kill -TERM 1
kubectl -n "$NS" get endpointslices -l kubernetes.io/service-name=tanod -w
```

**Pass:** `tanod-1` leaves the endpoint slice within the 5-second
`drain_period`, before Pingora stops its listener; the load generator records
no connection-refused errors during the replacement.

## 3. Service routing

**Pass:** with both pods Ready, `tanod_requests_total` grows on both
(`kubectl exec` each pod's `curl localhost:9090/metrics`); with one not-ready,
only on the other.

## 4. NetworkPolicy

From a pod in an unlabelled namespace:

```sh
curl -m 3 http://tanod-0.tanod-pods.$NS:9091/status   # admin
curl -m 3 http://tanod-0.tanod-pods.$NS:9090/metrics  # metrics
curl -m 3 http://tanod.$NS:8080/                      # traffic
```

**Pass:** all three time out. Repeat from namespaces labelled
`tanod-operators`, `tanod-monitoring` and `tanod-ingress`: each reaches exactly
its own port. Then confirm kubelet probes still succeed (pods stay Ready); on
some CNIs probe traffic is subject to the policy.

## 5. CPU and memory limits

```sh
kubectl -n "$NS" logs tanod-0 | grep -m1 -i 'worker'       # 1 worker at a 1-CPU limit
kubectl -n "$NS" top pod -l app=tanod                       # under sustained load
kubectl -n "$NS" get pod -l app=tanod \
  -o jsonpath='{range .items[*]}{.metadata.name} {.status.containerStatuses[0].restartCount}{"\n"}{end}'
```

**Pass:** one worker thread at the 1-CPU limit; working-set memory stays below
256 MiB with the cache full; zero restarts and no `OOMKilled`.

## 6. Replacement with origin work in flight

Each case below runs with the load from §1 and ends with `curl -s
http://next-origin:3000/__stats`. **Pass** for all: `peak` ≤ 8, no pod killed
at the grace period (`kubectl get events` shows no `Killing` after the
termination grace), and the pod's final `/metrics` scrape shows
`tanod_origin_disconnect_drains_active 0` and `tanod_origin_in_flight 0`.

```sh
# 6a. A pod terminating with uncached drains: hang up on /stream while it renders.
kubectl -n "$NS" run hangup --image=curlimages/curl --restart=Never -- sh -c '
  while true; do for i in $(seq 1 16); do
    curl -s -o /dev/null --max-time 0.05 "http://tanod:8080/stream/8?d=$RANDOM" &
  done; wait; done'
kubectl -n "$NS" delete pod tanod-1          # graceful; StatefulSet recreates it
# 6b. A pod terminating with active cache fills: the same with /bigstream/4.
# 6c. A new image: the rolling update in §1 with an image digest change.
kubectl -n "$NS" set image statefulset/tanod tanod="$TANOD_IMAGE_DIGEST_NEXT"
kubectl -n "$NS" rollout status statefulset/tanod --timeout=10m
```

**Also pass:** in `pods.txt`, the new `tanod-1` is created only after the old
one's `DELETED` event; at no time do three `tanod-*` pods exist.

## 7. Forced deletion (the documented unsafe case)

Do this only to measure the overlap, with load running:

```sh
kubectl -n "$NS" delete pod tanod-1 --grace-period=0 --force
```

**Expected, not a pass:** the replacement can start while the old process is
still running on its node, so `peak` may exceed 8 by up to the old pod's
partition (4). Record the observed peak; it is the reason the manifest says
never to force-delete an unfenced pod. Confirm the cluster recovers: two pods
Ready, `peak` back within 8 after `__reset`.

## 8. CPU throttling and memory pressure

```sh
kubectl -n "$NS" exec tanod-0 -- cat /sys/fs/cgroup/cpu.stat   # nr_throttled before
# run §1's load for 5 minutes
kubectl -n "$NS" exec tanod-0 -- cat /sys/fs/cgroup/cpu.stat   # and after
kubectl -n "$NS" exec tanod-0 -- cat /sys/fs/cgroup/memory.events
```

**Pass:** throttling may rise (one CPU under load); `peak` ≤ 8; `oom_kill 0`
in `memory.events`; no restart. Repeat with the cache budget filled
(`/big/1?u=$RANDOM` for 2 minutes): working set below the 256 MiB limit.

## 9. Origin failure during rollout

Start §1's rollout, then make the origin fail for 20 seconds
(`curl http://next-origin:3000/__fail`, later `/__heal`).

**Pass:** Tanod pods stay Ready (unless `require_healthy_upstream` is set),
clients see 502/503 rather than hangs, the rollout completes, `peak` ≤ 8, and
the breaker counters (`tanod_upstream_breaker_trips_total`) and
`tanod_upstream_failures_total` account for the failure.

## 10. Capacity allocation

**Pass:** `kubectl exec` each pod's `/status` reports `concurrency.max` 4 and
the `capacity` group 8/2; under saturating load both pods' `tanod_origin_in_flight`
reach 4 and the origin's `peak` is 8, not more.

## 11. Clean up

```sh
kubectl delete namespace "$NS"
```
