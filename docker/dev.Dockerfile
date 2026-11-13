# Development image: Postgres 16 + FUSE + Rust toolchain.
FROM postgres:16
RUN apt-get update \
 && apt-get install -y --no-install-recommends fuse3 ca-certificates curl build-essential pkg-config procps \
 && rm -rf /var/lib/apt/lists/*
ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:/usr/lib/postgresql/16/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --component clippy,rustfmt \
 && chmod -R a+rwX /opt/rustup /opt/cargo
WORKDIR /src
