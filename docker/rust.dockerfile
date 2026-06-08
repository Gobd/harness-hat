# harness-hat Rust image
#
# Build after harness-hat-base:local:
#   docker build -t harness-hat-rust:local -f docker/rust.dockerfile .

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

RUN set -eu; \
    curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs \
      | sh -s -- -y --profile default --default-toolchain stable; \
    rustup component add rustfmt clippy rust-src rust-analyzer; \
    cargo install --locked \
      cargo-edit \
      cargo-watch \
      cargo-nextest \
      cargo-audit \
      cargo-deny; \
    chmod -R a+rX "${RUSTUP_HOME}" "${CARGO_HOME}"

USER coder

ENV RUSTUP_HOME=/usr/local/rustup
ENV CARGO_HOME=/usr/local/cargo
ENV PATH="${CARGO_HOME}/bin:/home/coder/.local/bin:${PATH}"

CMD ["bash"]
