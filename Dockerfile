# datastore-oxide: a drop-in replacement for the Cloud Datastore emulator.
#
#   podman build -t datastore-oxide .
#   podman run --rm -p 8081:8081 datastore-oxide                           # on disk in /data/project
#   podman run --rm -p 8081:8081 datastore-oxide start --host=0.0.0.0 --port=8081 --testing
FROM docker.io/library/rust:1.93-slim-bookworm AS build
RUN apt-get update \
    && apt-get install -y --no-install-recommends protobuf-compiler libprotobuf-dev make \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p oxide-server

FROM docker.io/library/debian:bookworm-slim
COPY --from=build /src/target/release/datastore-oxide /usr/local/bin/datastore-oxide
# The Java emulator's command name, for scripts that call it directly.
RUN ln -s datastore-oxide /usr/local/bin/cloud_datastore_emulator \
    && useradd --system --uid 10001 --create-home --home-dir /data oxide
USER oxide
WORKDIR /data
EXPOSE 8081
HEALTHCHECK --interval=10s --timeout=2s CMD ["/bin/bash", "-c", "exec 3<>/dev/tcp/127.0.0.1/8081 && printf 'GET / HTTP/1.0\\r\\n\\r\\n' >&3 && grep -q Ok <&3"]
ENTRYPOINT ["datastore-oxide"]
CMD ["start", "--host=0.0.0.0", "--port=8081", "/data/project"]
