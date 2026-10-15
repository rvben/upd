# CI supplies the checksum-verified native Linux release binary as ./upd.
FROM debian:bookworm-slim@sha256:abd67ffcfa541b485a3dff59865ab629aa048a6c613e639d36e7456b0b229241
RUN apt-get update \
    && apt-get install --yes --no-install-recommends bash ca-certificates coreutils curl git grep tar \
    && rm -rf /var/lib/apt/lists/*
COPY --chmod=755 upd /usr/local/bin/upd
CMD ["upd", "--help"]
