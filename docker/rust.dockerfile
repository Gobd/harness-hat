# harness-hat Rust image
#
# Build after harness-hat-base:local:
#   docker build -t harness-hat-rust:local -f docker/rust.dockerfile .

# Rust's official manifest is pinned and selects the matching target
# architecture. The copied rustup/cargo trees retain side-by-side toolchain
# management without manually downloading rustup-init per architecture.
ARG RUST_IMAGE=rust:1.97.0-bookworm@sha256:8fa55b2f3ddf97471ab6a767bfa3f37e6bad0986ba823e75fea57e2a2a5c3073
FROM ${RUST_IMAGE} AS rust

FROM harness-hat-base:local

USER root

ENV RUSTUP_HOME=/usr/local/rustup
ENV CARGO_HOME=/usr/local/cargo
ENV PATH="${CARGO_HOME}/bin:${PATH}"

RUN set -eu; \
    apt-get update -o APT::Update::Error-Mode=any; \
    apt-get install -y --no-install-recommends \
      build-essential \
      make \
      cmake \
      pkg-config \
      clang \
      lld \
      mold \
      gdb \
      lldb \
      protobuf-compiler \
      sqlite3 \
      libsqlite3-dev \
      libssl-dev \
      jq \
      shellcheck \
      direnv; \
    rm -rf /var/lib/apt/lists/*

# Rust installs as root into the shared RUSTUP_HOME/CARGO_HOME. The container
# runs as `coder` (uid 1000), so hand ownership of both trees to that user in
# this same layer — otherwise `cargo build` (registry/cache writes), `cargo
# install` (writes to cargo/bin), and `rustup` updates all fail on root-owned
# paths. Doing the chown here (not a later layer) avoids duplicating the large
# toolchain with new ownership. a+rX keeps it readable if run under another uid.
# Pinned versions (H5): the official multi-architecture Rust image is pinned
# by manifest digest; rustup verifies component signatures/hashes internally.
# The cargo tools remain exact-version pins. Bump the image tag and digest
# together when updating the toolchain.
ARG RUST_TOOLCHAIN=1.97.0
COPY --from=rust /usr/local/cargo /usr/local/cargo
COPY --from=rust /usr/local/rustup /usr/local/rustup
RUN set -eu; \
    rustup toolchain install "${RUST_TOOLCHAIN}" --profile default; \
    rustup default "${RUST_TOOLCHAIN}"; \
    rustup component add rustfmt clippy rust-src rust-analyzer; \
    cargo install --locked \
      cargo-edit@0.13.11 \
      cargo-watch@8.5.3 \
      cargo-nextest@0.9.140 \
      cargo-audit@0.22.2 \
      cargo-deny@0.20.2; \
    chmod -R a+rX "${RUSTUP_HOME}" "${CARGO_HOME}"; \
    chown -R coder:coder "${RUSTUP_HOME}" "${CARGO_HOME}"

USER coder

ENV RUSTUP_HOME=/usr/local/rustup
ENV CARGO_HOME=/usr/local/cargo
ENV PATH="${CARGO_HOME}/bin:/home/coder/.local/bin:${PATH}"

CMD ["bash"]
