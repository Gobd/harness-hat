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
    @openai/codex@0.128.0 \
    @google/gemini-cli@0.41.2 \
    opencode-ai@1.14.39 \
    @anthropic-ai/claude-code@2.1.131
USER ubuntu

ENV PATH="/home/ubuntu/.local/bin:${PATH}"

CMD ["bash"]
