#!/bin/bash
# Run the installed Java Datastore emulator (the differential-test oracle) in a JRE container.
#   EMU      emulator dir holding CloudDatastore.jar (default: <gcloud SDK root>/platform/cloud-datastore-emulator)
#   WORK     host dir mounted at /work for project data (default: a fresh temp dir)
#   PODMAN_ARGS  extra podman args, e.g. PODMAN_ARGS="--name jp1 -p 18100:8081"
# Example: PODMAN_ARGS="--name jp1 -p 18100:8081" tools/javaemu.sh start --host=0.0.0.0 --port=8081 --testing
EMU=${EMU:-$(gcloud info --format='value(installation.sdk_root)')/platform/cloud-datastore-emulator}
WORK=${WORK:-$(mktemp -d)}
exec podman run --rm $PODMAN_ARGS -v "$EMU":/emu:ro -v "$WORK":/work:Z docker.io/library/eclipse-temurin:21-jre \
  java -cp /emu/CloudDatastore.jar com.google.cloud.datastore.emulator.CloudDatastore "$@"
