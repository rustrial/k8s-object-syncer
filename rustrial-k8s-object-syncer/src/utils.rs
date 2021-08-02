use json_patch::diff;
use kube::{
    api::{
        ApiResource, DeleteParams, DynamicObject, GroupVersionKind, Patch, PatchParams, TypeMeta,
    },
    Api, Client, Resource, ResourceExt,
};
use rustrial_k8s_object_syncer_apis::DestinationStatus;
use serde::{de::DeserializeOwned, Serialize};

use crate::{
    errors::{ControllerError, ExtKubeApiError},
    FINALIZER, MANAGER,
};

pub(crate) async fn add_finalizer_if_missing<T>(
    api: Api<T>,
    source: &mut T,
    finalizer: &str,
) -> Result<bool, ControllerError>
where
    T: Clone + std::fmt::Debug + Serialize + DeserializeOwned + Resource,
{
    source.meta_mut().managed_fields = Default::default();
    let finalizers = source.finalizers_mut();
    if finalizers
        .iter()
        .find(|f| f.as_str() == finalizer)
        .is_none()
    {
        finalizers.push(finalizer.to_string());
        api.patch(
            source.name().as_str(),
            &PatchParams {
                field_manager: Some(MANAGER.to_string()),
                dry_run: false,
                force: true,
            },
            &Patch::Apply(source),
        )
        .await?;
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
                source.name().as_str(),
                &PatchParams {
                    field_manager: Some(MANAGER.to_string()),
                    dry_run: false,
                    force: false,
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
        match api.get(d.name.as_str()).await {
            Ok(mut source) => {
                source.types = source.types.or(Some(TypeMeta {
                    kind: gvk.kind.clone(),
                    api_version: gvk.api_version(),
                }));
                if let Err(e) = remove_finalizer(api.clone(), &mut source, FINALIZER).await {
                    warn!(
                        "error while removing finalizer from destination object {} {}/{}:{}",
                        gvk.kind,
                        source.namespace().as_deref().unwrap_or(""),
                        source.name(),
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
