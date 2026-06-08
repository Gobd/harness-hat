# harness-hat default image
#
# Uses the shared Ubuntu base (`harness-hat-base:local`) so strict-network
# and proxy bootstrap behavior stays consistent with manager-launched images.
#
# Copy this file to create per-project variants, e.g.:
#   rust.dockerfile

FROM harness-hat-base:local

USER root

ENV BUN_INSTALL=/usr/local/bun
ENV PATH="${BUN_INSTALL}/bin:${PATH}"

RUN npm install -g \
    pnpm \
    typescript \
    tsx
RUN curl -fsSL https://bun.sh/install | bash \
    && chmod -R a+rX "${BUN_INSTALL}"
USER coder

ENV BUN_INSTALL=/usr/local/bun
ENV PATH="${BUN_INSTALL}/bin:/home/coder/.local/bin:${PATH}"

CMD ["bash"]
