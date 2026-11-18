# Linux build + test box: Rust (glibc + static musl), its own Docker daemon, git, WireGuard tools.
FROM rust:1-bookworm@sha256:ade39a710ef85f3506cffcfec3e8a24ba090badef1dff2fb798f71d878833fea
RUN apt-get update && apt-get install -y --no-install-recommends \
      musl-tools git iptables iproute2 wireguard-tools procps curl ca-certificates jq sqlite3 dnsutils \
    && curl -fsSL https://get.docker.com | sh \
    && rm -rf /var/lib/apt/lists/*
RUN rustup target add "$(uname -m)-unknown-linux-musl" && rustup component add clippy rustfmt
COPY start.sh /usr/local/bin/start-dev
ENTRYPOINT ["/usr/local/bin/start-dev"]
