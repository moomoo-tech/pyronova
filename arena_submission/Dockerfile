FROM python:3.13-slim AS build
WORKDIR /build

# Rust toolchain + maturin for the Pyre engine build.
RUN apt-get update && apt-get install -y --no-install-recommends \
        curl ca-certificates build-essential pkg-config libssl-dev git \
    && rm -rf /var/lib/apt/lists/*
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
        sh -s -- -y --default-toolchain stable --profile minimal
ENV PATH="/root/.cargo/bin:${PATH}"
RUN pip install --no-cache-dir maturin

# Source copied from the build context — expects a `pyre_src/` subdirectory
# in this framework folder. For local iteration:
#
#   cp -r /path/to/pyre frameworks/pyre/pyre_src
#   docker build -t httparena-pyre frameworks/pyre
#
# Reference builds (Arena CI) can populate pyre_src via a submodule or
# `git clone`. The Dockerfile doesn't clone on its own so the image pins
# to exactly what's in the build context, byte-for-byte reproducible.
COPY pyre_src /build/pyre
RUN cd /build/pyre \
    && RUSTFLAGS="-C target-cpu=native" \
       maturin build --release --out /wheels --compatibility linux


FROM python:3.13-slim
WORKDIR /app

COPY --from=build /wheels /tmp/wheels
RUN pip install --no-cache-dir /tmp/wheels/*.whl && rm -rf /tmp/wheels

COPY app.py launcher.py ./

# Arena harness expectations:
#   - PORT=8080        plaintext HTTP/1.1
#   - PORT+1=8081      HTTPS HTTP/1.1 (json-tls profile)
#   - 8443             HTTPS HTTP/2 (baseline-h2, static-h2)
#   - /certs/server.{crt,key}  mounted by harness
#   - /data/dataset.json       mounted by harness
#   - /data/static             mounted by harness
#   - $DATABASE_URL            set by harness when PG profile active
EXPOSE 8080 8081 8443
CMD ["python3", "launcher.py"]
