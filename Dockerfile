FROM rust:1-slim-bookworm AS build

WORKDIR /app
COPY . .

RUN apt-get update \
  && apt-get install -y --no-install-recommends pkg-config libssl-dev cmake \
  && cargo build --release

FROM debian:bookworm-slim

RUN apt-get update \
  && apt-get install -y --no-install-recommends libssl3 ca-certificates \
  && rm -rf /var/lib/apt/lists/*

RUN groupadd -g 1001 lava-gitlab-runner \
  && useradd -u 1001 -g lava-gitlab-runner -d /app -M lava-gitlab-runner

COPY --from=build /app/target/release/lava-gitlab-runner /usr/local/bin

USER lava-gitlab-runner

ENTRYPOINT [ "/usr/local/bin/lava-gitlab-runner" ]
