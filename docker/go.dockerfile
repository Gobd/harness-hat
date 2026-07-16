# harness-hat Go image
#
# Build after harness-hat-base:local:
#   docker build -t harness-hat-go:local -f docker/go.dockerfile .

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

# Pinned Go toolchain (H5): the official tarball, verified against the sha256
# published on go.dev/dl, replaces Ubuntu's golang-go (whose Go is too old for
# the pinned tool versions below). Bump by editing the ARGs and rebuilding.
ARG GO_VERSION=1.26.5
ARG GO_SHA256_AMD64=5c2c3b16caefa1d968a94c1daca04a7ca301a496d9b086e17ad77bb81393f053
ARG GO_SHA256_ARM64=fe4789e92b1f33358680864bbe8704289e7bb5fc207d80623c308935bd696d49
ARG TARGETARCH
RUN set -eu; \
    case "${TARGETARCH:-$(dpkg --print-architecture)}" in \
      amd64|x86_64) go_arch="amd64"; go_sha="${GO_SHA256_AMD64}" ;; \
      arm64|aarch64) go_arch="arm64"; go_sha="${GO_SHA256_ARM64}" ;; \
      *) echo "unsupported Go architecture: ${TARGETARCH:-$(dpkg --print-architecture)}" >&2; exit 1 ;; \
    esac; \
    curl -fsSL -o /tmp/go.tar.gz \
      "https://go.dev/dl/go${GO_VERSION}.linux-${go_arch}.tar.gz"; \
    echo "${go_sha}  /tmp/go.tar.gz" | sha256sum -c -; \
    tar -C /usr/local -xzf /tmp/go.tar.gz; \
    rm -f /tmp/go.tar.gz; \
    /usr/local/go/bin/go version

RUN set -eu; \
    mkdir -p "${GOPATH}/bin"; \
    chown -R coder:coder "${GOPATH}"

# gofumpt: stricter gofmt, pinned release binary verified against sha256.
ARG GOFUMPT_VERSION=0.10.0
ARG GOFUMPT_SHA256_AMD64=48ee398ec72afdaca6accb70f5ed741349d5dcccb52c5bb4c4469d698980b186
ARG GOFUMPT_SHA256_ARM64=5d239f93b1ed2dfe74d39b25112bb770a04f6581f986d4b4f5d380521e12ca61
RUN set -eu; \
    case "${TARGETARCH:-$(dpkg --print-architecture)}" in \
        amd64|x86_64)  gf_arch="amd64"; gf_sha="${GOFUMPT_SHA256_AMD64}" ;; \
        arm64|aarch64) gf_arch="arm64"; gf_sha="${GOFUMPT_SHA256_ARM64}" ;; \
        *) echo "unsupported gofumpt architecture: ${TARGETARCH:-$(dpkg --print-architecture)}" >&2; exit 1 ;; \
    esac; \
    curl -fsSL \
        -o /tmp/gofumpt \
        "https://github.com/mvdan/gofumpt/releases/download/v${GOFUMPT_VERSION}/gofumpt_v${GOFUMPT_VERSION}_linux_${gf_arch}"; \
    echo "${gf_sha}  /tmp/gofumpt" | sha256sum -c -; \
    install -m 0755 /tmp/gofumpt /usr/local/bin/gofumpt; \
    rm -f /tmp/gofumpt; \
    gofumpt --version

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
