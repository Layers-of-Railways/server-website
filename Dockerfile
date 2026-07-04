FROM docker.io/rust:1-slim-bookworm AS backend-build

ARG pkg=railways-server-website

RUN apt-get update && apt-get install -y pkg-config libssl-dev

WORKDIR /build

COPY backend/ .

ENV SQLX_OFFLINE=true

RUN --mount=type=cache,target=/build/target \
    --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    set -eux; \
    cargo build --release; \
    objcopy --compress-debug-sections target/release/$pkg ./main

################################################################################

FROM node:24-slim AS frontend-build

ENV PNPM_HOME="/pnpm"
ENV PATH="$PNPM_HOME:$PATH"
RUN corepack enable

WORKDIR /frontend

COPY frontend/ .

RUN --mount=type=cache,id=pnpm,target=/pnpm/store pnpm install
RUN --mount=type=cache,id=pnpm,target=/pnpm/store pnpm build

################################################################################

FROM docker.io/debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

WORKDIR /app

## Copy the main binary
COPY --from=backend-build /build/main ./

## Copy frontend
COPY --from=frontend-build /frontend/build ./static

## Ensure the container listens globally on port 8080
ENV ROCKET_ADDRESS=0.0.0.0
ENV ROCKET_PORT=8080

CMD ["./main"]
