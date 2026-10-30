# Rendered by bench/showcase/showcase.py for one arm. Both arms come from this
# one template, so the limiter, timeouts and proxy settings cannot drift apart.
# The upstream block is the only arm-specific part: the three Next.js origins
# directly (baseline), or the Tanod instance in front of those same origins.
worker_processes @@NGINX_WORKERS@@;

events {
  worker_connections 16384;
}

http {
  access_log off;
  server_tokens off;

  # The ordinary per-visitor rate limit a public ingress already has. All load
  # comes from one k6 host, so keying on the socket address would turn this into
  # one global limit; by default it is keyed on the synthetic visitor id k6
  # sends, which models many distinct visitors (see docs/demo/README.md).
  # Rejections are 429 so they are never confused with Tanod's 503.
  limit_req_zone @@NGINX_LIMIT_KEY@@ zone=visitors:10m rate=@@NGINX_RATE@@;
  limit_req_status 429;

  upstream app {
@@UPSTREAM_SERVERS@@
    keepalive 64;
  }

  server {
    listen 8080;

    location / {
      limit_req zone=visitors burst=@@NGINX_BURST@@ nodelay;
      proxy_http_version 1.1;
      proxy_set_header Host $http_host;
      proxy_set_header X-Forwarded-For $remote_addr;
      proxy_set_header X-Forwarded-Proto $scheme;
      proxy_set_header Connection "";
      proxy_buffering off;
      # NGINX's own defaults unless overridden, as in ops/edge/nginx.conf. A
      # short connect timeout plus the default passive health check
      # (max_fails=1, fail_timeout=10s) takes a backlogged origin out of
      # rotation, which changes what the baseline arm reports.
      proxy_connect_timeout @@NGINX_CONNECT_TIMEOUT@@;
      proxy_send_timeout @@NGINX_PROXY_TIMEOUT@@;
      proxy_read_timeout @@NGINX_PROXY_TIMEOUT@@;
      proxy_pass http://app;
    }
  }
}
