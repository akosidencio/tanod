# syntax=docker/dockerfile:1
#
# A static binary on `scratch`. Tanod links no libc at runtime (musl, built in
# statically) and no OpenSSL (TLS is rustls), so the image holds the binary,
# the CA bundle `https://` export verifies against, a writable /tmp for the
# PID file and upgrade socket, and a passwd entry for the unprivileged user —
# and nothing else: no shell, no package manager, nothing to patch.
#
# The tag is a floor: rust-toolchain.toml pins the exact compiler and rustup
# inside the image honours it, so this only has to be new enough not to fight
# that. Keeping the two in step avoids downloading a second toolchain on every
# build.
FROM rust:1.98-alpine AS builder

# aws-lc (rustls's crypto provider) builds with cmake and a C compiler; musl
# headers for the static link; binutils for strip.
RUN apk add --no-cache musl-dev cmake clang perl make linux-headers binutils

# Cargo features to compile in. `--build-arg FEATURES=tls` produces an image
# that can terminate TLS, speak TLS to the origin and export telemetry over
# https; the default does not, because most deployments terminate at a load
# balancer and an unused TLS stack is unused attack surface. A binary built
# without it *rejects* a config that needs it rather than leaving a port dead.
ARG FEATURES=""

WORKDIR /src
COPY . .
RUN --mount=type=cache,id=tanod-cargo-registry-musl,target=/usr/local/cargo/registry \
    --mount=type=cache,id=tanod-target-musl,target=/src/target \
    target="$(uname -m)-unknown-linux-musl" \
    && cargo build --release --locked --bin tanod --target "$target" \
      ${FEATURES:+--features "$FEATURES"} \
    && cp "/src/target/$target/release/tanod" /tmp/tanod \
    && strip /tmp/tanod

# The runtime filesystem, assembled here because `scratch` has no tools.
# uid 10001 and the name `tanod` match the previous Debian-based image, so a
# derived image's `COPY --chown=tanod:tanod` keeps working.
RUN mkdir -p /rootfs/etc/ssl/certs /rootfs/etc/tanod /rootfs/run/tanod /rootfs/tmp \
    && cp /etc/ssl/certs/ca-certificates.crt /rootfs/etc/ssl/certs/ \
    && echo 'tanod:x:10001:10001:tanod:/nonexistent:/sbin/nologin' > /rootfs/etc/passwd \
    && echo 'tanod:x:10001:' > /rootfs/etc/group \
    && chown 10001:10001 /rootfs/etc/tanod /rootfs/run/tanod \
    && chmod 0750 /rootfs/etc/tanod /rootfs/run/tanod \
    && chmod 1777 /rootfs/tmp \
    && /tmp/tanod version

FROM scratch AS runtime

COPY --from=builder /rootfs/ /
COPY --from=builder /tmp/tanod /usr/local/bin/tanod

USER tanod
# 8080 traffic, 8443 TLS (only in an image built with FEATURES=tls),
# 9090 Prometheus, 9091 the admin endpoints. The last two are operator
# surfaces: publish them to a private network, never to the internet.
EXPOSE 8080 8443 9090 9091
ENTRYPOINT ["/usr/local/bin/tanod"]
CMD ["run", "--config", "/etc/tanod/tanod.yaml"]
