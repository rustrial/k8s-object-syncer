[![Artifact HUB](https://img.shields.io/endpoint?url=https://artifacthub.io/badge/repository/k8s-object-syncer)](https://artifacthub.io/packages/search?repo=k8s-object-syncer)

# Object Syncer

[Kubernetes Controller](https://kubernetes.io/docs/concepts/architecture/controller/) to synchronize (copy)
objects between namespaces. Common use-cases for this controller are:

- Copying a *public* `ConfigMap` to all namespaces, making sure it exists in all namespaces and 
  is kept in-sync with the original (source) `ConigMap`.
- Copying image-pull `Secrets` to all namespaces, making sure they exist in all namespaces and 
  are kept in-sync with the original (source) `Secret`.

The *Object Syncer* controller supports all namespace scoped built-in Kubernetes API resources and
*CustomResourceDefinitions* (CRD). Thus it can be used to sync arbitrary API objects and not just 
`ConfigMap` or `Secret` objects.

## Background & Motivation for this project

As Kubernetes cluster operators, we often need to make sure that certain Kubernetes objects
exists in some or all namespaces. And we do not know all namespaces up-front respectively namespaces might 
come and go over the course of time. Searching for an existing tool to solve this kind of problem, we tried
[Kyverno](https://kyverno.io/) and [Kubed](https://github.com/kubeops/kubed). Unfortunatelly those two tools
did not work for our use-cases and we eventually decided to implement a (new) controller to cover our needs.

- Kyverno's [Generating Resources feature](https://kyverno.io/docs/writing-policies/generate/#generating-resources-into-existing-namespaces) 
  does not cover the use-case of already existing namespaces and will only generate (sync)
  resources into newly created namespaces.
- Kubed [only supports `ConfigMap` and `Secret` resources](https://github.com/kubeops/kubed/tree/release-0.13/docs/guides/config-syncer).

---

## How it works

Let's assume you have the follwoing `ConfigMap` and you need to make sure it exists in all namespaces 
and that all copies are kept in sync with the original (source) `ConfigMap`.

```yaml
kind: ConfigMap
apiVersion: v1
metadata:
  name: my-test-config-map
  namespace: default
data:
  someKey: someValue
```

This can be achieved by deploying the following `ObjectSync` (custom resource) object:

```yaml
kind: ObjectSync
apiVersion: sync.rustrial.org/v1alpha1
metadata:
  name: my-test-config-map-distributor
  namespace: default
spec:
  source: # Reference to the original (source) object
    group: ""
    kind: ConfigMap 
    name: my-test-config-map
    namespace: default # (optional) defaults to the namespace of the ObjectSync object.
  destinations: # List of destinations
  - namespace: "*" # empty string or wildcard "*" means "all namespaces"
    name: my-test-config-map # (optional) defaults to the name of the source object.
```

The *Object Syncer* controller will watch all `ObjectSync` objects and dynamically creates
resource specific sub-controllers. In the above example, it will create a sub-controller for
`ConfigMap` resources and will make sure the following invariants hold true: 

- While the source object exists and has no `deletionTimestamp` set:
  - Ensure it is replicated according to the `destinations` configuration of the `ObjectSync` object.
  - Ensure that changes to the source object are applied to all of its copies (the sync part). If 
    there is any drift detected, then the affected copy is replaced with the original (source).
  - Ensure that deleted copies are re-created.
- If the source object is deleted (or its `deletionTimestamp` is set), delete all synced copies.
- If the corresponding `ObjectSync` objcect is deleted (or its `deletionTimestamp` is set), delete all 
  synced copies.

If the last `ObjectSync` object referencing a specific resource (type) is deleted, then the 
corresponding resource specific sub-controller will be removed to prevent any resource-leaks and 
to reduce load on the Kubernetes API servers.

### Examples

**Sync a `ConfigMap` to all namespaces**:

```yaml
kind: ObjectSync
apiVersion: sync.rustrial.org/v1alpha1
metadata:
  name: my-test-config-map-distributor
  namespace: default
spec:
  source:
    group: ""
    kind: ConfigMap 
    name: my-test-config-map
  destinations:
  - namespace: "*"
```

**Sync a `Secret` to all namespaces and rename it**:

```yaml
kind: ObjectSync
apiVersion: sync.rustrial.org/v1alpha1
metadata:
  name: my-test-config-map-distributor
  namespace: default
spec:
  source:
    group: ""
    kind: Secret 
    name: image-pull-secrets
  destinations:
  - namespace: "*"
    name: global-image-pull-secrets
```

**Sync a `CronJob` to some namespaces**:

```yaml
kind: ObjectSync
apiVersion: sync.rustrial.org/v1alpha1
metadata:
  name: my-test-config-map-distributor
  namespace: default
spec:
  source:
    group: "batch"
    kind: CronJob 
    name: my-audit-scanner
  destinations:
  - namespace: "kube-system"
  - namespace: "linkerd"
```

**Sync a FluxCD `HelmRelease` to all namespaces**:

```yaml
kind: ObjectSync
apiVersion: sync.rustrial.org/v1alpha1
metadata:
  name: my-test-config-map-distributor
  namespace: default
spec:
  source:
    group: "helm.toolkit.fluxcd.io"
    kind: HelmRelease 
    name: my-namespace-agent
  destinations:
  - namespace: "*"
```

---

## Security Considerations

As with all controllers that can read and create Kubernetes resources the RBAC configuration
of the controller's associated ServiceAccount must be designed carefully to meet your cluster's
security requirements.

The accompanying [Helm Chart](charts/k8s-object-syncer/README.md) provides 4 parameters to 
configure those RBAC rules:

- `watchNamespaces`: The set of namespaces the controller will be entitled to read, watch and write `ObjectSync` objects.
- `sourceNamespaces`: The set of namespaces the controller will be entitled to read, watch all objects whose resource types are part of `allowedResources`.
- `targetNamespaces`: The set of namespaces the controller will be entitled to write objects whose resource types are part of `allowedResources`.
- `allowedResources`: The set of Kubernetes resources (`apiGropus` & `resources`) the controller will be entitled to sync (read/write).



---

## Getting Started

Check the [Helm Chart Readme](charts/k8s-object-syncer/README.md) for instructions on
how to install this controller.

---

## Resource Lifecycle & Garbage Collection

The controller uses [finalizer](https://kubernetes.io/docs/tasks/extend-kubernetes/custom-resources/custom-resource-definitions/#finalizers) 
pattern to make sure stale destination objects are removed whenever the corresponding 
source object or `ObjectSync` object is deleted.

Due do some limitations in the rust [kube-rs](https://github.com/kube-rs/kube-rs) Kubernetes library used
by this controller, it has to add additional finalizers to all tracked source and destination objects to 
make sure it obtains all `delete` events.

---

## License

Licensed under either of

- Apache License, Version 2.0
  ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license
  ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)
- The Unlicense
  ([UNLICENSE](LUNLICENSE) or https://opensource.org/licenses/unlicense)

at your option.

## Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
triple licensed as above, without any additional terms or conditions. See the
[WAIVER](WAIVER) and [CONTRIBUTING.md](CONTRIBUTING.md) files for more information.
