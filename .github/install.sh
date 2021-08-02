#!/bin/bash

set -e

IMG=test/rustrial-k8s-object-syncer:latest

docker image build -t test/rustrial-k8s-object-syncer:latest .

kind load docker-image test/rustrial-k8s-object-syncer:latest --name "${KIND:-kind}"

helm upgrade k8s-object-syncer charts/k8s-object-syncer \
    --install -n kube-system \
    --create-namespace \
    --set fullnameOverride=k8s-object-syncer \
    --set image.repository=test/rustrial-k8s-object-syncer \
    --set image.tag=latest

# helm upgrade k8s-object-syncer charts/k8s-object-syncer \
#     --install -n default \
#     --create-namespace \
#     --set fullnameOverride=k8s-object-syncer \
#     --set image.repository=test/rustrial-k8s-object-syncer \
#     --set image.tag=latest
#     --set watchNamespace=default
#     --set storageNamespace=default