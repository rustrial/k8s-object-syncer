use std::{fmt::Display, ops::Deref};

use crate::{
    FINALIZER, MANAGER,
    errors::{ControllerError, ExtKubeApiError},
    object_sync_modifications::ObjectSyncModifications,
};
use json_patch::diff;
use kube::{
    Api, Client, Resource, ResourceExt,
    api::{
        ApiResource, DeleteParams, DynamicObject, GroupVersionKind, ObjectMeta, Patch, PatchParams,
        TypeMeta, ValidationDirective,
    },
};

use kube_runtime::reflector::ObjectRef;
use rustrial_k8s_object_syncer_apis::{DestinationStatus, ObjectSync};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NamespacedName {
    pub name: String,
    pub namespace: String,
}

impl std::fmt::Display for NamespacedName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.namespace, self.name)
    }
}

impl From<&DynamicObject> for NamespacedName {
    fn from(o: &DynamicObject) -> Self {
        Self {
            name: o.name_any(),
            namespace: o.namespace().unwrap_or_else(|| "".to_string()),
        }
    }
}

impl From<&ObjectSync> for NamespacedName {
    fn from(o: &ObjectSync) -> Self {
        Self {
            name: o.name_any(),
            namespace: o.namespace().unwrap_or_else(|| "".to_string()),
        }
    }
}

impl NamespacedName {
    pub fn object_ref(&self, ar: &ApiResource) -> ObjectRef<DynamicObject> {
        ObjectRef::<DynamicObject>::new_with(self.name.as_str(), ar.clone())
            .within(self.namespace.as_str())
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct ObjectSyncRef(NamespacedName);

impl Display for ObjectSyncRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Deref for ObjectSyncRef {
    type Target = NamespacedName;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<&ObjectSync> for ObjectSyncRef {
    fn from(value: &ObjectSync) -> Self {
        Self(NamespacedName::from(value))
    }
}

impl From<&ObjectSyncModifications> for ObjectSyncRef {
    fn from(value: &ObjectSyncModifications) -> Self {
        Self(NamespacedName::from(&value.modified))
    }
}

impl From<&mut ObjectSyncModifications> for ObjectSyncRef {
    fn from(value: &mut ObjectSyncModifications) -> Self {
        Self(NamespacedName::from(&value.modified))
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct SourceRef(NamespacedName);

impl Display for SourceRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Deref for SourceRef {
    type Target = NamespacedName;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<&DynamicObject> for SourceRef {
    fn from(value: &DynamicObject) -> Self {
        Self(NamespacedName::from(value))
    }
}

impl From<&ObjectSyncModifications> for SourceRef {
    fn from(value: &ObjectSyncModifications) -> Self {
        Self(NamespacedName {
            name: value.spec.source.name.clone(),
            namespace: value.source_namespace().unwrap_or_else(|| "".to_string()),
        })
    }
}

/// Ensure kind and api_version are properly set
pub(crate) fn ensure_gvk(mut source: DynamicObject, gvk: &GroupVersionKind) -> DynamicObject {
    source.types = source.types.or(Some(TypeMeta {
        kind: gvk.kind.clone(),
        api_version: gvk.api_version(),
    }));
    source
}

pub(crate) async fn add_finalizer_if_missing(
    api: Api<DynamicObject>,
    source: &mut DynamicObject,
    finalizer: &str,
) -> Result<bool, ControllerError> {
    if source
        .finalizers()
        .iter()
        .find(|f| f.as_str() == finalizer)
        .is_none()
    {
        // The object-sync controller does not own the source object, so we use a
        // minimal patch for server-side apply to only take ownership of our specific
        // finalizer.
        let patch = DynamicObject {
            types: source.types.clone(),
            metadata: ObjectMeta {
                name: source.metadata.name.clone(),
                namespace: source.metadata.namespace.clone(),
                // Finalizer list is a Set (listType=set) and SSA ownership is tracked per
                // unique value, thus we can conveniently only care about our value (no
                // need to set already existing finalizers values as well).
                finalizers: Some(vec![finalizer.to_string()]),
                ..Default::default()
            },
            data: Value::Null,
        };
        api.patch(
            patch.name_any().as_str(),
            &PatchParams {
                field_manager: Some(MANAGER.to_string()),
                dry_run: false,
                force: true,
                field_validation: Some(ValidationDirective::Ignore),
            },
            &Patch::Apply(patch),
        )
        .await?;
        // Update source in-memory just to reflect the fact that we added the finalizer.
        source.finalizers_mut().push(finalizer.to_string());
        Ok(true)
    } else {
        Ok(false)
    }
}

pub(crate) async fn remove_finalizer<T>(
    api: Api<T>,
    source: &mut T,
    finalizer: &str,
) -> Result<bool, ControllerError>
where
    T: Clone + std::fmt::Debug + Serialize + DeserializeOwned + Resource,
{
    let original = source.clone();
    let finalizers = source.finalizers_mut();
    let len = finalizers.len();
    finalizers.retain(|f| f != finalizer);
    if finalizers.len() != len {
        // Use JSON Patch as server-side apply would not remove the finalizer on destination objects.
        // This is because for removal of array elements the fieldManager must be set properly, which
        // is not the case for destinations objects created by `create` or `replace` API calls which
        // do not set the fieldManager properly (as of Kubernetes 1.19).
        let patch = diff(
            &serde_json::to_value(&original)?,
            &serde_json::to_value(&source)?,
        );
        match api
            .patch(
                source.name_any().as_str(),
                &PatchParams {
                    field_manager: Some(MANAGER.to_string()),
                    dry_run: false,
                    force: false,
                    field_validation: None,
                },
                &Patch::<T>::Json(patch),
            )
            .await
        {
            Ok(_) => (),
            Err(e) if e.is_not_found() => (),
            Err(e) => Err(e)?,
        }
        Ok(true)
    } else {
        Ok(false)
    }
}

pub(crate) async fn delete_destinations(
    client: Client,
    destinations: &Vec<DestinationStatus>,
) -> Result<Vec<DestinationStatus>, ControllerError> {
    let mut errors: Vec<DestinationStatus> = Default::default();
    for d in destinations {
        // Note, each destionation might have a different GVK.
        // So do not reuse the api instance instead create a fresh one for each destination.
        let gvk = GroupVersionKind {
            group: d.group.clone(),
            version: d.version.clone(),
            kind: d.kind.clone(),
        };
        let api_resource = ApiResource::from_gvk(&gvk);
        let api: Api<DynamicObject> =
            Api::namespaced_with(client.clone(), d.namespace.as_str(), &api_resource);

        // Remove finalizer from destination object.
        match api.get(d.name.as_str()).await.map(|v| ensure_gvk(v, &gvk)) {
            Ok(mut destination) => {
                if let Err(e) = remove_finalizer(api.clone(), &mut destination, FINALIZER).await {
                    warn!(
                        "error while removing finalizer from destination object {} {}/{}:{}",
                        gvk.kind,
                        destination.namespace().as_deref().unwrap_or(""),
                        destination.name_any(),
                        e
                    );
                }
            }
            Err(e) if e.is_not_found() => (),
            Err(e) => Err(e)?,
        }

        match api.delete(d.name.as_str(), &DeleteParams::default()).await {
            Ok(_) => {
                info!(
                    "deleted destination object {}/{} of type {}/{}/{}",
                    d.group, d.version, d.kind, d.namespace, d.name
                );
            }
            Err(e) if e.is_not_found() => {
                debug!(
                    "tried to delete destination object {}/{} of type {}/{}/{}, but it does no longer exist: {}",
                    d.group, d.version, d.kind, d.namespace, d.name, e
                );
            }
            Err(e) => {
                error!(
                    "failed to delete destination object {}/{} of type {}/{}/{}: {}",
                    d.group, d.version, d.kind, d.namespace, d.name, e
                );
                errors.push(d.clone());
            }
        }
    }
    Ok(errors)
}

pub(crate) fn metric_name(name: &str) -> String {
    format!("object_syncer_{}", name)
}
