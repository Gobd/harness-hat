# harness-hat default image
#
# Uses the shared Ubuntu base (`harness-hat-base:local`) so strict-network
# and proxy bootstrap behavior stays consistent with manager-launched images.
#
# Copy this file to create per-project variants, e.g.:
#   rust.dockerfile

FROM harness-hat-base:local

USER root
RUN npm install -g \
    @openai/codex \
    @google/gemini-cli \
    @earendil-works/pi-coding-agent \
    @anthropic-ai/claude-code \
    pnpm \
    typescript \
    tsx
USER ubuntu

ENV PATH="/home/ubuntu/.local/bin:${PATH}"

CMD ["bash"]
