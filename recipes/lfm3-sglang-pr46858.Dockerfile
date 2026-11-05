# SGLang with native LFM3 support: the 2026-11-03 nightly plus PR #46858.
# Build from the repo root:
#   docker build -f recipes/lfm3-sglang-pr46858.Dockerfile -t lmdeck/sglang-lfm3:pr46858 recipes/
# Swap the base tag and drop the overlay once the PR lands in an SGLang release.
FROM lmsysorg/sglang:nightly-dev-cu13-20261103-797af717
RUN mkdir -p /tmp/pr \
 && curl -fsSL https://codeload.github.com/sgl-project/sglang/tar.gz/a54b7b0333669ef87d689b21ee27c0c2147ff459 -o /tmp/pr.tar.gz \
 && tar xzf /tmp/pr.tar.gz -C /tmp/pr \
 && cp -a /tmp/pr/sglang-*/python/. /sgl-workspace/sglang/python/ \
 && rm -rf /tmp/pr /tmp/pr.tar.gz
RUN pip install -q "transformers==5.20.2"
