# harness-hat TypeScript / Node / Bun image
#
# Build after harness-hat-base:local:
#   docker build -t harness-hat-typescript:local -f docker/typescript.dockerfile .

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

RUN set -eu; \
    corepack enable; \
    npm install -g \
      npm@latest \
      pnpm@latest \
      typescript \
      tsx \
      vite \
      eslint \
      prettier \
      nodemon; \
    curl -fsSL https://bun.sh/install | bash; \
    chmod -R a+rX "${BUN_INSTALL}"

USER coder

ENV BUN_INSTALL=/usr/local/bun
ENV PATH="${BUN_INSTALL}/bin:/home/coder/.local/bin:${PATH}"

CMD ["bash"]
