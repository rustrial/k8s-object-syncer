#!/bin/bash

export KIND=06b24764-b43a-4a68-848b-54b0956325df
export KUBECONFIG=".kube-config"

kind create cluster --name $KIND --kubeconfig $KUBECONFIG

kubectl delete deployment -n kube-system k8s-object-syncer

./.github/install.sh "$@"

./.github/e2e-tests.sh