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

# uv publishes an official multi-architecture image. Its manifest-list digest
# pins the source while Docker selects the matching target architecture.
ARG UV_IMAGE=ghcr.io/astral-sh/uv:0.11.29@sha256:eb2843a1e56fd9e30c7276ce1a52cba86e64c7b385f5e3279a0e08e02dd058fc
FROM ${UV_IMAGE} AS uv

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

COPY --from=uv /uv /uvx /usr/local/bin/
RUN uv --version

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
