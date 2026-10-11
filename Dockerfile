# Binary-only packaging stage. The release binary is compiled on the CI runner
# (rust-cache incremental) before docker build; compile elsewhere with:
#   cargo build --release --features storage-debug
FROM ubuntu:24.04 AS runtime
WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        libgcc-s1 \
        libstdc++6 \
    && rm -rf /var/lib/apt/lists/*

COPY target/release/TransformVetter /app/TransformVetter
COPY artifacts /app/artifacts
COPY start.sh /app/start.sh
COPY README.md /app/README.md

EXPOSE 8000

ENTRYPOINT ["/app/TransformVetter"]
