use std::collections::HashSet;

use k8s_openapi::chrono::{SecondsFormat, Utc};
use kube::{CustomResource, ResourceExt, api::ObjectMeta};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const API_GROUP: &'static str = "sync.rustrial.org";

pub const SOURCE_OBJECT_ANNOTATION: &'static str = "sync.rustrial.org/source-object";

/// We maintain our own copy of Condition as the one from k8s_openapi does not implement JsonSchema.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, JsonSchema)]
pub struct Condition {
    /// lastTransitionTime is the last time the condition transitioned from one status to another. This should be when the underlying condition changed.  If that is not known, then using the time when the API field changed is acceptable.
    #[serde(rename = "lastTransitionTime", skip_serializing_if = "Option::is_none")]
    pub last_transition_time: Option<String>,

    /// message is a human readable message indicating details about the transition. This may be an empty string.
    pub message: String,

    /// observedGeneration represents the .metadata.generation that the condition was set based upon. For instance, if .metadata.generation is currently 12, but the .status.conditions\[x\].observedGeneration is 9, the condition is out of date with respect to the current state of the instance.
    #[serde(rename = "observedGeneration", skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,

    /// reason contains a programmatic identifier indicating the reason for the condition's last transition. Producers of specific condition types may define expected values and meanings for this field, and whether the values are considered a guaranteed API. The value should be a CamelCase string. This field may not be empty.
    pub reason: String,

    /// status of the condition, one of True, False, Unknown.
    pub status: String,

    /// type of condition in CamelCase or in foo.example.com/CamelCase.
    #[serde(rename = "type")]
    pub type_: String,
}

impl Condition {
    pub fn new(tpe: &str, status: Option<bool>, reason: &str, message: String) -> Self {
        Self {
            last_transition_time: None,
            message,
            reason: reason.to_string(),
            status: status
                .map(|v| if v { "True" } else { "False" })
                .unwrap_or("Unknown")
                .to_string(),
            type_: tpe.to_string(),
            observed_generation: None,
        }
    }
}

/// Kubernetes object synchronization specification, defining how a single source
/// object should be replicated and synced to multiple destination objects.
#[derive(CustomResource, Debug, Clone, Deserialize, Serialize, JsonSchema, PartialEq)]
#[kube(
    group = "sync.rustrial.org",
    version = "v1alpha1",
    kind = "ObjectSync",
    derive = "PartialEq",
    status = "ObjectSyncStatus",
    namespaced,
    printcolumn = r#"{
        "name":"Ready",
        "type": "string",
        "jsonPath": ".status.conditions[?(@.type==\"Ready\")].status",
        "description": "Whether ObjectSync is ready or not. It is considered ready if there were no errors while synchronizing the resource."
    }"#,
    printcolumn = r#"{
        "name":"InSync",
        "type": "string",
        "jsonPath": ".status.conditions[?(@.type==\"InSync\")].status",
        "description": "Whether all destination objects are in sync or not."
    }"#
)]
pub struct ObjectSyncSpec {
    /// The "original" source object to be synced.
    pub source: SourceObject,
    /// The destinations to sync the source object to.
    pub destinations: Vec<Destination>,
}

impl ObjectSyncSpec {
    /// Get unique set of [`Destination`]s. This helper is needed as the
    /// JSON/YAML representation uses [`Vec`] which might contain duplicate
    /// entries.
    pub fn destinations(&self) -> HashSet<&Destination> {
        let mut tmp: HashSet<&Destination> = Default::default();
        tmp.extend(self.destinations.iter());
        tmp
    }
}

/// Object reference to a source object.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema)]
pub struct SourceObject {
    /// The Kubernetes API Group name (without the version part).
    pub group: String,
    /// The Kubernetes API version, defaults to the preferred version of the API Group.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The Kubernetes API Kind name.
    pub kind: String,
    /// The Kubernetes object's name (`metadata.name`)
    pub name: String,
    /// The Kubernetes object's namespace (`metadata.namespace`) defaults to the namespace
    /// of the `ObjectSync` object.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

/// Destination object synchronization strategy.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, JsonSchema)]
pub enum SyncStrategy {
    /// Use [server side apply](https://kubernetes.io/docs/reference/using-api/server-side-apply/) to
    /// manage (sync) destination objects with a shared ownership model
    /// (only overwriting changes in fields present in source object).
    #[serde(rename = "apply")]
    Apply,
    /// Use replace to manage (sync) destination objects with exclusive ownership
    /// (overwriting all changes made by others).
    #[serde(rename = "replace")]
    Replace,
}

impl Default for SyncStrategy {
    fn default() -> Self {
        Self::Apply
    }
}

/// Synchronization target configuration.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Hash, Clone, JsonSchema)]
pub struct Destination {
    /// Optional new name for the destination object, defaults to the name
    /// of the source object.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The destination (target) namespace, if empty `""` or `"*"` the source object
    /// is synced to all namespaces.
    pub namespace: String,
    /// The sync strategy to use for this destination, defaults to "apply" (server side apply).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strategy: Option<SyncStrategy>,
}

impl Destination {
    pub fn strategy(&self) -> SyncStrategy {
        self.strategy.unwrap_or_default()
    }

    pub fn applies_to_all_namespaces(&self) -> bool {
        self.namespace.as_str() == "" || self.namespace.as_str() == "*"
    }

    fn applies_to_namespace(&self, namespace: &str) -> bool {
        self.namespace.as_str() == namespace || self.applies_to_all_namespaces()
    }

    /// Check whether this [`Destination`] applies to given namespace and if so
    /// returns a tuple `(namespace,name)`.
    pub fn applies_to(&self, obj: &ObjectSync, dst_namespace: &str) -> Option<(String, String)> {
        let spec: &ObjectSyncSpec = &obj.spec;
        let src_name = spec.source.name.as_str();
        let src_namespace = spec
            .source
            .namespace
            .as_deref()
            .or(obj.metadata.namespace.as_deref())
            .unwrap_or("");
        let dst_name = self.name.as_deref().unwrap_or(spec.source.name.as_str());
        if self.applies_to_namespace(dst_namespace)
            // make sure destination does not point to source
            && (src_name != dst_name || src_namespace != dst_namespace)
        {
            Some((dst_namespace.to_string(), dst_name.to_string()))
        } else {
            None
        }
    }
}

/// Specific Kubernetes object revision.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, JsonSchema)]
pub struct ObjectRevision {
    /// The Kubernetes object's `UID`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
    /// The Kubernetes object's `resourceVersion`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_version: Option<String>,
}

impl From<&ObjectMeta> for ObjectRevision {
    fn from(m: &ObjectMeta) -> Self {
        Self {
            uid: m.uid.clone(),
            resource_version: m.resource_version.clone(),
        }
    }
}

/// Synchronization target status, used to track synchronized objects.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, JsonSchema)]
pub struct DestinationStatus {
    /// Kubernetes object's name of the synchronization target.
    pub name: String,
    /// Kubernetes object's namespace of the synchronization target.
    pub namespace: String,
    /// Kubernetes API Group name of the synchronization target.
    pub group: String,
    /// Kubernetes API Group version of the synchronization target.
    pub version: String,
    /// Kubernetes API Kind name of the synchronization target.
    pub kind: String,
    /// The last source version observed.
    #[serde(skip_serializing_if = "Option::is_none", rename = "sourceVersion")]
    pub source_version: Option<ObjectRevision>,
    /// The last source version syced, `None` if not yet synced.
    #[serde(skip_serializing_if = "Option::is_none", rename = "syncedVersion")]
    pub synced_version: Option<ObjectRevision>,
    /// The [`SyncStrategy`] applied to this destination.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strategy: Option<SyncStrategy>,
}

impl DestinationStatus {
    pub fn strategy(&self) -> SyncStrategy {
        self.strategy.unwrap_or_default()
    }
}

impl PartialOrd for DestinationStatus {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DestinationStatus {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match self.namespace.cmp(&other.namespace) {
            std::cmp::Ordering::Equal => self.name.cmp(&other.name),
            x => x,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema)]
pub struct ObjectSyncStatus {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conditions: Option<Vec<Condition>>,
    /// As Kubernetes does not support cross-namespace OwnerReferences and automatic
    /// garbage collection of owned objects, we keep track of all synced destination
    /// objects in the status sub-resource.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destinations: Option<Vec<DestinationStatus>>,
}

impl Default for ObjectSyncStatus {
    fn default() -> Self {
        Self {
            conditions: Default::default(),
            destinations: Default::default(),
        }
    }
}

impl ObjectSync {
    pub fn id(&self) -> String {
        format!(
            "{}/{}",
            self.metadata.namespace.as_deref().unwrap_or(""),
            self.metadata.name.as_deref().unwrap_or(""),
        )
    }

    pub fn versioned_id(&self) -> String {
        format!(
            "{}@{}",
            self.id(),
            self.metadata.resource_version.as_deref().unwrap_or("")
        )
    }

    pub fn status_destinations(&self) -> Option<&Vec<DestinationStatus>> {
        self.status
            .as_ref()
            .map(|v| v.destinations.as_ref())
            .flatten()
    }

    pub fn update_condition(&mut self, c: Condition) {
        let mut status = self
            .status
            .take()
            .unwrap_or_else(|| ObjectSyncStatus::default());
        status.update_condition(c);
        self.status = Some(status);
    }

    pub fn update_destinations(&mut self, destinations: Vec<DestinationStatus>) {
        let mut status = self
            .status
            .take()
            .unwrap_or_else(|| ObjectSyncStatus::default());
        status.update_destinations(destinations);
        self.status = Some(status);
    }

    pub fn source_name(&self) -> &str {
        &self.spec.source.name
    }

    /// Return the namespace of the source object, which defaults
    /// to the namespace of the ObjectSync object.
    pub fn source_namespace(&self) -> Option<String> {
        self.spec
            .source
            .namespace
            .clone()
            .or_else(|| self.namespace())
    }
}

impl ObjectSyncStatus {
    pub fn update_condition(&mut self, mut c: Condition) {
        let time = Utc::now();
        c.last_transition_time = Some(time.to_rfc3339_opts(SecondsFormat::Secs, true));
        let mut conditions: Vec<Condition> = self.conditions.take().unwrap_or_else(|| vec![]);
        if let Some(existing) = conditions.iter().find(|c| c.type_ == c.type_) {
            if existing.status != c.status
                || existing.reason != c.reason
                || existing.message != c.message
                || existing.observed_generation != c.observed_generation
            {
                conditions.retain(|v| v.type_ != c.type_);
                conditions.push(c);
            }
        } else {
            conditions.push(c);
        };
        self.conditions = Some(conditions);
    }

    pub fn update_destinations(&mut self, destinations: Vec<DestinationStatus>) {
        self.destinations = Some(destinations);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        let p = ObjectSyncSpec {
            source: SourceObject {
                group: "".to_string(),
                version: Default::default(),
                kind: "ConfigMap".to_string(),
                name: "x".to_string(),
                namespace: None,
            },
            destinations: vec![],
        };
        assert_eq!(
            r#"{"source":{"group":"","kind":"ConfigMap","name":"x"},"destinations":[]}"#,
            serde_json::to_string(&p).unwrap()
        );
    }

    #[test]
    fn sync_strategy() {
        assert_eq!(
            r#""apply""#,
            serde_json::to_string(&SyncStrategy::Apply).unwrap()
        );
        assert_eq!(
            r#""replace""#,
            serde_json::to_string(&SyncStrategy::Replace).unwrap()
        );
    }

    #[test]
    fn destination() {
        let source_namespace = Destination {
            name: None,
            namespace: "source_namespace".to_string(),
            strategy: None,
        };

        let source_namespace_other_name = Destination {
            name: Some("you".to_string()),
            namespace: "source_namespace".to_string(),
            strategy: None,
        };

        let other_namespace = Destination {
            name: None,
            namespace: "other_namespace".to_string(),
            strategy: None,
        };

        let all_namespaces = Destination {
            name: None,
            namespace: "*".to_string(),
            strategy: None,
        };

        let all_namespaces_other_name = Destination {
            name: Some("you".to_string()),
            namespace: "*".to_string(),
            strategy: None,
        };

        let obj = ObjectSync {
            metadata: ObjectMeta {
                name: Some("me".to_string()),
                namespace: Some("source_namespace".to_string()),
                ..Default::default()
            },
            spec: ObjectSyncSpec {
                source: SourceObject {
                    group: "".to_string(),
                    version: None,
                    kind: "ConfigMap".to_string(),
                    name: "me".to_string(),
                    namespace: None,
                },
                destinations: Default::default(),
            },
            status: None,
        };

        // Do not overwrite source (same name & namespace)
        assert!(
            all_namespaces
                .applies_to(&obj, "source_namespace")
                .is_none()
        );
        assert!(
            source_namespace
                .applies_to(&obj, "source_namespace")
                .is_none()
        );
        // Apply if not source but same namespace
        assert!(
            all_namespaces_other_name
                .applies_to(&obj, "source_namespace")
                .is_some()
        );
        assert!(
            source_namespace_other_name
                .applies_to(&obj, "source_namespace")
                .is_some()
        );
        // Apply if different namespace
        assert!(all_namespaces.applies_to(&obj, "other_namespace").is_some());
        assert!(
            other_namespace
                .applies_to(&obj, "other_namespace")
                .is_some()
        );
        assert!(
            all_namespaces_other_name
                .applies_to(&obj, "other_namespace")
                .is_some()
        );
        // Refuse if destination namespace does not match
        assert!(
            source_namespace_other_name
                .applies_to(&obj, "other_namespace")
                .is_none()
        );
        assert!(
            other_namespace
                .applies_to(&obj, "source_namespace")
                .is_none()
        );
    }
}
