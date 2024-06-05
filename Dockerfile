FROM rust:1-slim-bookworm AS build
ARG DEBIAN_FRONTEND=noninteractive

ADD . /app
WORKDIR /app
RUN apt-get update \
  && apt-get install -y pkg-config libssl-dev cmake \
  && cargo build --release

FROM debian:bookworm-slim
ARG DEBIAN_FRONTEND=noninteractive

RUN adduser --uid 1001 --group --no-create-home --home /app lava-gitlab-runner

RUN apt update && apt install -y libssl3 ca-certificates
COPY --from=build /app/target/release/lava-gitlab-runner /usr/local/bin

USER lava-gitlab-runner

ENTRYPOINT [ "/usr/local/bin/lava-gitlab-runner" ]
