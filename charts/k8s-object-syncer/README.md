# Helm Chart Values

| Parameter                                  | Description                                                                        | Default Value                                              | Mandatory |
| ------------------------------------------ | ---------------------------------------------------------------------------------- | ---------------------------------------------------------- | --------- |
| `watchNamespaces`                          | Namespaces to watch for `ObjectSync` objects, use `["*"]` for all namespaces.      | `["kube-public"]`                                          | no        |
| `sourceNamespaces`                         | Namespaces allowed to reading source objects from, use `["*"]` for all namespaces. | `["kube-public"]`                                          | no        |
| `targetNamespaces`                         | Namespaces allowed to creating copies into, use `["*"]` for all namespaces.        | `["*"]` (all namespaces)                                   | no        |
| `allowedResources`                         | Resources (apiGroups & resources) that the controller is allowed to sync           | `[{"apiGroups": ["*"], resources: ["*"]}]` (all resources) | no        |
| `logLevel`                                 | Log level on of `error`, `info`, `debug` or `trace`                                | `info`                                                     | no        |
| `replicaCount`                             | HA is not yet supported (no leader election), so only `1` makes sense.             | `1`                                                        | no        |
| `image.repository`                         |                                                                                    | `rustrial/k8s-object-syncer`                               | no        |
| `image.pullPolicy`                         |                                                                                    | `IfNotPresent`                                             | no        |
| `image.tag`                                | Default is the chart appVersion                                                    | `""`                                                       | no        |
| `imagePullSecrets`                         |                                                                                    | `[]`                                                       | no        |
| `nodeSelector`                             |                                                                                    | `{"kubernetes.io/os": "linux"}`                            | no        |
| `tolerations`                              |                                                                                    | `[]`                                                       | no        |
| `affinity`                                 |                                                                                    | `{}`                                                       | no        |
| `topologySpreadConstraints`                |                                                                                    | `{}`                                                       | no        |
| `nameOverride`                             |                                                                                    | `""`                                                       | no        |
| `fullnameOverride`                         |                                                                                    | `""`                                                       | no        |
| `serviceAccount.create`                    | Whether to create a `ServiceAccount` or not.                                       | `true`                                                     | no        |
| `serviceAccount.annotations`               |                                                                                    | `{}`                                                       | no        |
| `serviceAccount.name`                      |                                                                                    | `""`                                                       | no        |
| `podAnnotations`                           | Extra Pod annotations                                                              | `{}`                                                       | no        |
| `podLabels`                                | Extra Pod labels                                                                   | `{}`                                                       | no        |
| `deploymentAnnotations`                    | Extra Deployment annotations                                                       | `{}`                                                       | no        |
| `deploymentLabels`                         | Extra Deployment labels                                                            | `{}`                                                       | no        |
| `podSecurityContext`                       |                                                                                    | `{}`                                                       | no        |
| `securityContext.capabilities.drop`        |                                                                                    | `["ALL"]`                                                  | no        |
| `securityContext.allowPrivilegeEscalation` |                                                                                    | `false`                                                    | no        |
| `securityContext.readOnlyRootFilesystem`   |                                                                                    | `true`                                                     | no        |
| `securityContext.runAsNonRoot`             |                                                                                    | `true`                                                     | no        |
| `runAsUser`                                |                                                                                    | `1000`                                                     | no        |
| `runAsGroup`                               |                                                                                    | `1000`                                                     | no        |
| `resources.limits.cpu`                     |                                                                                    | `100m`                                                     | no        |
| `resources.limits.memory`                  |                                                                                    | `64Mi`                                                     | no        |
| `resources.requests.cpu`                   |                                                                                    | `10m`                                                      | no        |
| `resources.requests.memory`                |                                                                                    | `32Mi`                                                     | no        |
| `extraEnv`                                 | Extra environment variables to pass to the object syncer container                 | `[]`                                                       | no        |
| `podMonitor.enabled`                       | Whether to create a `PodMonitor` or not.                                           | `false`                                                    | no        |
| `podMonitor.interval`                      | `PodMonitor` scrape interval                                                       | `60s`                                                      | no        |
| `podMonitor.scrapeTimeout`                 | `PodMonitor` scrape timeout                                                        | `10s`                                                      | no        |


## Installation

Properly set `watchNamespaces`, `sourceNamespaces`, `targetNamespaces` and `allowedResources` to match your needs and to comply with your cluster security requirements. 

**Add Helm Repository**

The controller can be installed via Helm Chart, which by default will use the prebuilt OCI Images for Linux (`amd64` and `arm64`) from [DockerHub](https://hub.docker.com/r/rustrial/k8s-object-syncer).

```shell
helm repo add k8s-object-syncer https://rustrial.github.io/k8s-object-syncer
```

**Install Helm Chart**

```shell
helm install my-k8s-object-syncer k8s-object-syncer/k8s-object-syncer \
     --version 0.1.0 
```

## Examples Configurations

Allow syncing of all resources from namespace `kube-public` to all namespaces with `ObjectSync` objects owned by `kube-public`.
This is a common seupt for cluster-operators which want to distribute some "public" resources into all namespaces. Securitywise
this is fine as long as only the cluster-operators have write access to namespace `kube-public`.

```yaml
watchNamespaces: ["kube-public"]
sourceNamespaces: ["kube-public"]
targetNamespaces: ["*"]
allowedResources:
- apiGroups: ["*"]
  resources: ["*"]
```

Depending on your namespacing scheme you might have slightly different variants, but all have in common that the `watchNamespaces` 
and the `sourceNamespaces` are limited to namespaces to which only cluster-operators have (write) access.

```yaml
watchNamespaces: ["kube-public", "kube-system"]
sourceNamespaces: ["kube-public"]
targetNamespaces: ["*"]
allowedResources:
- apiGroups: ["*"]
  resources: ["*"]
```

---

Allow syncing of all resources from namespace `kube-public` to all namespaces with `ObjectSync` objects owned by any namespace.
This configuration has some security implications as it allows any namespace to sync resources (from namespace `kube-public`) 
into other namespaces, only use it in trusted clusters it is not suitable for multi-tenancy clusters.

```yaml
watchNamespaces: ["*"]
sourceNamespaces: ["kube-public"]
targetNamespaces: ["*"]
allowedResources:
- apiGroups: ["*"]
  resources: ["*"]
```

---

Allow syncing of all resources from all namespaces to all namespaces with `ObjectSync` objects owned by any namespace.
This configuration is very likely not what you want as it completly erades your cluster security.

```yaml
watchNamespaces: ["*"]
sourceNamespaces: ["*"]
targetNamespaces: ["*"]
allowedResources:
- apiGroups: ["*"]
  resources: ["*"]
```