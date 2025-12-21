FROM rust:1-slim-bookworm AS build

ADD . /app
WORKDIR /app
RUN apt-get update \
  && apt-get install -y --no-install-recommends pkg-config libssl-dev cmake \
  && cargo build --release

FROM debian:bookworm-slim

RUN adduser --uid 1001 --group --no-create-home --home /app lava-gitlab-runner

RUN apt update && apt install -y --no-install-recommends libssl3 ca-certificates
COPY --from=build /app/target/release/lava-gitlab-runner /usr/local/bin

USER lava-gitlab-runner

ENTRYPOINT [ "/usr/local/bin/lava-gitlab-runner" ]
