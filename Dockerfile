FROM debian:bullseye-slim
ARG DEBIAN_FRONTEND=noninteractive

RUN apt update && apt install -y libssl1.1 ca-certificates
COPY lava-gitlab-runner /usr/local/bin

ENTRYPOINT /usr/local/bin/lava-gitlab-runner
