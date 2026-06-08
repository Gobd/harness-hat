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
      golang-go \
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

RUN set -eu; \
    mkdir -p "${GOPATH}/bin"; \
    chown -R coder:coder "${GOPATH}"

USER coder

ENV GOPATH=/home/coder/go
ENV PATH="/usr/local/go/bin:${GOPATH}/bin:/home/coder/.local/bin:${PATH}"

RUN set -eu; \
    go install golang.org/x/tools/gopls@latest; \
    go install github.com/go-delve/delve/cmd/dlv@latest; \
    go install honnef.co/go/tools/cmd/staticcheck@latest; \
    go install github.com/golangci/golangci-lint/cmd/golangci-lint@latest

CMD ["bash"]
