#!/usr/bin/bash
# Build the frr-bootc bootc image, convert it to a qcow2 disk with
# bootc-image-builder, and wrap that disk into a containerDisk image that
# can be referenced from a KubeVirt/OpenShift Virtualization VirtualMachine.
#
# Usage: ./build.sh [image-tag]
set -euo pipefail

cd "$(dirname "$0")"

IMAGE_NAME=${IMAGE_NAME:-frr-bootc}
IMAGE_TAG=${1:-${IMAGE_TAG:-latest}}
CONTAINERDISK_NAME=${CONTAINERDISK_NAME:-frr-bootc-containerdisk}
OUTPUT_DIR=${OUTPUT_DIR:-output}

echo "==> Building bootc image ${IMAGE_NAME}:${IMAGE_TAG}"
podman build -t "${IMAGE_NAME}:${IMAGE_TAG}" -f Containerfile .

echo "==> Converting bootc image to a qcow2 disk via bootc-image-builder"
mkdir -p "${OUTPUT_DIR}"
podman run --rm -it --privileged \
    --security-opt label=type:unconfined_t \
    -v "$(pwd)/${OUTPUT_DIR}:/output" \
    -v /var/lib/containers/storage:/var/lib/containers/storage \
    quay.io/centos-bootc/bootc-image-builder:latest \
    --type qcow2 \
    --local \
    "${IMAGE_NAME}:${IMAGE_TAG}"

echo "==> Building containerDisk image ${CONTAINERDISK_NAME}:${IMAGE_TAG}"
podman build -t "${CONTAINERDISK_NAME}:${IMAGE_TAG}" -f containerdisk/Containerfile "${OUTPUT_DIR}"

cat <<EOF

Done.

  bootc image:   ${IMAGE_NAME}:${IMAGE_TAG}
  containerDisk: ${CONTAINERDISK_NAME}:${IMAGE_TAG}

Push the containerDisk image to a registry your OpenShift cluster can reach,
then reference it from the VirtualMachine manifest, e.g.:

  podman push ${CONTAINERDISK_NAME}:${IMAGE_TAG} quay.io/<you>/${CONTAINERDISK_NAME}:${IMAGE_TAG}

See manifests/ and README.md for the rest of the deployment.
EOF
