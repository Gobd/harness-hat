# harness-hat Python / uv image
#
# Build (context: docker/ directory):
#   docker build -t harness-hat-python:local -f docker/python.dockerfile docker/
#
# Inside a session:
#   uv init myproject && cd myproject
#   uv add requests pandas          # downloads from PyPI; resolves + locks
#   uv run python main.py           # runs in the managed venv
#   uv sync                         # reinstall from existing lockfile

FROM harness-hat-base:local
USER root

RUN apt-get update -o APT::Update::Error-Mode=any \
    && apt-get install -y --no-install-recommends \
       build-essential \
       make \
       pkg-config \
       libssl-dev \
       zlib1g-dev \
       libbz2-dev \
       libreadline-dev \
       libsqlite3-dev \
       libffi-dev \
       jq \
       shellcheck \
       direnv \
    && rm -rf /var/lib/apt/lists/*

# Pinned uv (H5): downloaded as a release tarball and verified against sha256.
# Bump by updating ARGs and re-running sha256sum on the new tarballs.
ARG UV_VERSION=0.11.29
ARG UV_SHA256_X64=04f8b82f5d47f0512dcd32c67a4a6f16a0ea27c81537c338fd0ad6b23cebe829
ARG UV_SHA256_AARCH64=94500fb064ae3c971a873cba64d94694c50677e0a4dbf78735c80509e7429919
ARG TARGETARCH

RUN set -eu; \
    case "${TARGETARCH:-$(dpkg --print-architecture)}" in \
        amd64|x86_64)   uv_arch="x86_64-unknown-linux-gnu";  uv_sha="${UV_SHA256_X64}" ;; \
        arm64|aarch64)  uv_arch="aarch64-unknown-linux-gnu"; uv_sha="${UV_SHA256_AARCH64}" ;; \
        *) echo "unsupported architecture for uv: ${TARGETARCH:-$(dpkg --print-architecture)}" >&2; exit 1 ;; \
    esac; \
    curl -fsSL \
        -o /tmp/uv.tar.gz \
        "https://github.com/astral-sh/uv/releases/download/${UV_VERSION}/uv-${uv_arch}.tar.gz"; \
    echo "${uv_sha}  /tmp/uv.tar.gz" | sha256sum -c -; \
    tar -xzf /tmp/uv.tar.gz -C /tmp; \
    install -m 0755 "/tmp/uv-${uv_arch}/uv"  /usr/local/bin/uv; \
    install -m 0755 "/tmp/uv-${uv_arch}/uvx" /usr/local/bin/uvx; \
    rm -rf /tmp/uv.tar.gz "/tmp/uv-${uv_arch}"; \
    uv --version

# Install a recent CPython via uv so the version is consistent and managed.
# uv stores it in /usr/local/share/uv/python; the `uv run` / `uv sync`
# commands pick it up automatically when no .python-version is present.
ARG PYTHON_VERSION=3.13
RUN uv python install "${PYTHON_VERSION}"

# Disable uv's self-update mechanism inside containers — the binary is pinned
# above and auto-update would hit the network unexpectedly.
ENV UV_NO_UPDATE_CHECK=1
# Cache lives in the workspace volume, not ephemeral container storage.
ENV UV_CACHE_DIR=/workspace/.uv-cache

USER coder
ENV PATH="/home/coder/.local/bin:${PATH}"
CMD ["/bin/bash"]
