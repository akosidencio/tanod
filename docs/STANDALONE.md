# Standalone server

Tanod can run as one Linux binary under systemd. Docker and Kubernetes are
optional. For one application on one server, use this network path:

```text
DNS -> Caddy :443 -> Tanod 127.0.0.1:8080 -> application 127.0.0.1:3000
```

Caddy owns the public domain and renews its TLS certificate. Tanod stays on
loopback and governs origin work. The application also stays on loopback, so
traffic cannot bypass Tanod.

## 1. Install the release binary

Download the archive and checksum from the GitHub release. Replace the version
with the release you want to install.

```bash
VERSION=0.3.0
TARGET=x86_64-unknown-linux-gnu
BASE="https://github.com/akosidencio/tanod/releases/download/v${VERSION}"

curl -fsSLO "${BASE}/tanod-${VERSION}-${TARGET}.tar.gz"
curl -fsSLO "${BASE}/SHA256SUMS"
sha256sum --ignore-missing -c SHA256SUMS
tar xzf "tanod-${VERSION}-${TARGET}.tar.gz"
cd "tanod-${VERSION}-${TARGET}"

sudo ./install.sh
```

The installer adds the binary, a restricted service account, the systemd unit,
and `/etc/tanod/tanod.yaml`. It preserves that config when upgrading and
does not start Tanod before you review it.

## 2. Create `tanod.yaml`

The generated defaults fit a single server with a local application on port
3000. The catch-all treats every response as private, so the first deployment
bounds origin work without sharing user data.

```bash
sudo -u tanod tanod check
```

Use flags when the local ports or initial origin ceiling differ:

```bash
sudo tanod init \
  --config /etc/tanod/tanod.yaml \
  --upstream 127.0.0.1:4000 \
  --listen 127.0.0.1:8080 \
  --concurrency 24 \
  --force
sudo chown root:tanod /etc/tanod/tanod.yaml
sudo chmod 0640 /etc/tanod/tanod.yaml
```

The initial concurrency value is a starting point. Measure the application and
tune it before relying on the limit in production. Add reviewed public routes
above `default-private` when caching or request coalescing is safe for them.

`tanod run` and `tanod check` find `/etc/tanod/tanod.yaml`
automatically. An explicit `--config` flag or `TANOD_CONFIG` overrides the
default search.

## 3. Run the application and Tanod

Run the application with its own supervisor and bind it to loopback:

```bash
next start -H 127.0.0.1 -p 3000
```

Then enable Tanod:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now tanod
sudo systemctl status tanod
curl http://127.0.0.1:9091/health/live
```

## 4. Point a domain at the server

Create DNS `A` and, when available, `AAAA` records for the domain pointing to
the server. Install Caddy, then add this site block to its Caddyfile:

```caddyfile
app.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

Reload Caddy after replacing `app.example.com`. Caddy accepts public traffic on
ports 80 and 443, obtains the certificate, and forwards the original host,
client address, and scheme to Tanod. The generated config trusts those
forwarded values only from loopback.

Keep ports 8080, 3000, and the admin port 9091 closed to the public network.
Only ports 80 and 443 need to be reachable. If a cloud load balancer or another
edge proxy replaces Caddy, update `server.trusted_proxies.from` to that proxy's
private address range.

Tanod can terminate TLS itself when built with the `tls` feature, but it does
not obtain or renew certificates. A small edge proxy is the simpler public
server setup and provides a quick path around Tanod during an incident.

## Everyday commands

```bash
tanod check                              # validate the discovered config
sudo systemctl reload tanod              # apply reloadable policy changes
sudo systemctl restart tanod             # restart after listener changes
journalctl -u tanod -f                    # follow logs
curl http://127.0.0.1:9091/status           # inspect current state
```
