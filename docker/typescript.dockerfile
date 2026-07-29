# harness-hat TypeScript / Node / Bun image
#
# Build after harness-hat-base:local:
#   docker build -t harness-hat-typescript:local -f docker/typescript.dockerfile .

# Bun's official manifest is pinned and selects the correct amd64 or arm64
# image without CPU-feature detection during the Docker build.
ARG BUN_IMAGE=oven/bun:1.3.14@sha256:e10577f0db68676a7024391c6e5cb4b879ebd17188ab750cf10024a6d700e5c4
FROM ${BUN_IMAGE} AS bun

FROM harness-hat-base:local

USER root

ENV BUN_INSTALL=/usr/local/bun
ENV PATH="${BUN_INSTALL}/bin:${PATH}"

RUN set -eu; \
    apt-get update -o APT::Update::Error-Mode=any; \
    apt-get install -y --no-install-recommends \
      build-essential \
      make \
      python3 \
      python3-pip \
      pkg-config \
      jq \
      shellcheck \
      direnv; \
    rm -rf /var/lib/apt/lists/*

# Pinned versions (H5): npm packages are integrity-checked against registry
# metadata. Bun comes from its pinned official multi-architecture image.
# Bump by updating the image tag and manifest-list digest together.
ARG TYPESCRIPT_VERSION=7.0.2
ARG TSX_VERSION=4.23.0
ARG VITE_VERSION=8.1.4
ARG ESLINT_VERSION=10.6.0
ARG PRETTIER_VERSION=3.9.5
ARG NODEMON_VERSION=3.1.14
RUN set -eu; \
    corepack enable; \
    npm install -g \
      "typescript@${TYPESCRIPT_VERSION}" \
      "tsx@${TSX_VERSION}" \
      "vite@${VITE_VERSION}" \
      "eslint@${ESLINT_VERSION}" \
      "prettier@${PRETTIER_VERSION}" \
      "nodemon@${NODEMON_VERSION}"

COPY --from=bun /usr/local/bin/bun ${BUN_INSTALL}/bin/bun
RUN ln -sf bun "${BUN_INSTALL}/bin/bunx" \
    && "${BUN_INSTALL}/bin/bun" --version

USER coder

ENV BUN_INSTALL=/usr/local/bun
ENV PATH="${BUN_INSTALL}/bin:/home/coder/.local/bin:${PATH}"

CMD ["bash"]
