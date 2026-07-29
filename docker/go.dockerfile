# harness-hat Go image
#
# Build after harness-hat-base:local:
#   docker build -t harness-hat-go:local -f docker/go.dockerfile .

# Go's official manifest is pinned and selects the matching target
# architecture. This avoids maintaining separate release URLs and hashes.
ARG GO_IMAGE=golang:1.26.5-bookworm@sha256:1ecb7edf62a0408027bd5729dfd6b1b8766e578e8df93995b225dfd0944eb651
FROM ${GO_IMAGE} AS go

FROM harness-hat-base:local

USER root

ENV GOPATH=/home/coder/go
ENV PATH="/usr/local/go/bin:${GOPATH}/bin:${PATH}"

RUN set -eu; \
    apt-get update -o APT::Update::Error-Mode=any; \
    apt-get install -y --no-install-recommends \
      build-essential \
      make \
      cmake \
      pkg-config \
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

COPY --from=go /usr/local/go /usr/local/go
RUN go version

RUN set -eu; \
    mkdir -p "${GOPATH}/bin"; \
    chown -R coder:coder "${GOPATH}"

# gofumpt: stricter gofmt. `go install` verifies the exact module version via
# the public checksum database and builds it for the target architecture.
ARG GOFUMPT_VERSION=0.10.0
RUN set -eu; \
    GOBIN=/usr/local/bin go install "mvdan.cc/gofumpt@v${GOFUMPT_VERSION}"; \
    gofumpt --version; \
    chown -R coder:coder "${GOPATH}"

USER coder

ENV GOPATH=/home/coder/go
ENV PATH="/usr/local/go/bin:${GOPATH}/bin:/home/coder/.local/bin:${PATH}"

# Pinned tool versions (H5). `go install` verifies module contents against the
# public checksum database (sum.golang.org), so an exact version is also a
# content pin. Bump by editing the versions and rebuilding.
RUN set -eu; \
    go install golang.org/x/tools/gopls@v0.23.0; \
    go install github.com/go-delve/delve/cmd/dlv@v1.27.0; \
    go install honnef.co/go/tools/cmd/staticcheck@v0.7.0; \
    go install github.com/golangci/golangci-lint/v2/cmd/golangci-lint@v2.12.2

CMD ["bash"]
