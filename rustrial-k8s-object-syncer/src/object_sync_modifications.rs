use crate::utils::ObjectSyncRef;
use crate::{MANAGER, errors::ControllerError};
use json_patch::diff;
use kube::ResourceExt;
use kube::api::PostParams;
use kube::{
    Api, Client,
    api::{Patch, PatchParams},
};
use rustrial_k8s_object_syncer_apis::ObjectSync;
use std::ops::DerefMut;

/// Helper construct to simplify updating and patching [`ObjectSync`] objects.
pub(crate) struct ObjectSyncModifications {
    original: ObjectSync,
    pub modified: ObjectSync,
}

impl std::ops::Deref for ObjectSyncModifications {
    type Target = ObjectSync;

    fn deref(&self) -> &Self::Target {
        &self.modified
    }
}

impl DerefMut for ObjectSyncModifications {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.modified
    }
}

impl ObjectSyncModifications {
    pub fn id(&self) -> ObjectSyncRef {
        ObjectSyncRef::from(self)
    }

    pub(crate) fn new(original: ObjectSync) -> Self {
        let modified = original.clone();
        Self { original, modified }
    }

    pub(crate) fn is_deleted(&self) -> bool {
        self.metadata.deletion_timestamp.is_some()
    }

    fn api(&self, client: Client) -> Api<ObjectSync> {
        if let Some(ns) = self.original.namespace() {
            Api::<ObjectSync>::namespaced(client, ns.as_str())
        } else {
            Api::<ObjectSync>::all(client)
        }
    }

    async fn latest(&mut self, client: &Client) -> kube::Result<ObjectSync, ControllerError> {
        let api = self.api(client.clone());
        let name = self.modified.name_any();
        Ok(api.get_status(name.as_str()).await?)
    }

    fn status_has_changed(&self) -> Result<bool, ControllerError> {
        Ok(self.get_status_patch(&self.original)?.is_some())
    }

    fn spec_has_changed(&self) -> Result<bool, ControllerError> {
        Ok(self.get_spec_patch(&self.original)?.is_some())
    }

    async fn _replace_spec(&mut self, client: Client, latest: &ObjectSync) -> kube::Result<()> {
        let api = self.api(client);
        let name = self.modified.name_any();
        self.modified.metadata.resource_version = latest.metadata.resource_version.clone();
        let mut pp = PostParams::default();
        pp.field_manager = Some(MANAGER.to_string());
        self.modified = api.replace(name.as_str(), &pp, &self.modified).await?;
        self.original = self.modified.clone();
        Ok(())
    }

    pub(crate) async fn replace_status(&mut self, client: Client) -> Result<(), ControllerError> {
        if self.status_has_changed()? {
            let latest: ObjectSync = self.latest(&client).await?;
            Ok(self._replace_status(client, &latest).await?)
        } else {
            Ok(())
        }
    }

    async fn _replace_status(&mut self, client: Client, latest: &ObjectSync) -> kube::Result<()> {
        let api = self.api(client);
        let name = self.modified.name_any();
        self.modified.metadata.resource_version = latest.metadata.resource_version.clone();
        let mut pp = PostParams::default();
        pp.field_manager = Some(MANAGER.to_string());
        self.modified = api
            .replace_status(
                name.as_str(),
                &pp,
                serde_json::to_vec(&self.modified).map_err(|e| kube::Error::SerdeError(e))?,
            )
            .await?;
        self.original = self.modified.clone();
        Ok(())
    }

    pub(crate) async fn patch_spec(&mut self, client: Client) -> Result<(), ControllerError> {
        if self.spec_has_changed()? {
            Ok(self._patch_spec(client).await?)
        } else {
            Ok(())
        }
    }

    fn get_spec_patch(
        &self,
        latest: &ObjectSync,
    ) -> Result<Option<json_patch::Patch>, ControllerError> {
        let spec_patch = {
            let mut latest = latest.clone();
            let mut mspec = self.modified.clone();
            latest.status = None;
            mspec.status = None;
            let patch = diff(
                &serde_json::to_value(&latest)?,
                &serde_json::to_value(&mspec)?,
            );
            if patch.0.is_empty() {
                None
            } else {
                Some(patch)
            }
        };
        Ok(spec_patch)
    }

    fn get_status_patch(
        &self,
        latest: &ObjectSync,
    ) -> Result<Option<json_patch::Patch>, ControllerError> {
        let patch = diff(
            &serde_json::to_value(&latest.status)?,
            &serde_json::to_value(&self.modified.status)?,
        );
        if patch.0.is_empty() {
            Ok(None)
        } else {
            Ok(Some(patch))
        }
    }

    async fn _patch_spec(&mut self, client: Client) -> Result<(), ControllerError> {
        let name = self.modified.name_any();
        let namespace = self.original.namespace().unwrap_or("".to_string());
        let api = self.api(client);
        let latest = api.get(name.as_str()).await?;
        let spec_patch = self.get_spec_patch(&latest)?;
        if let Some(patch) = spec_patch {
            let patch_txt = serde_json::to_string(&patch).unwrap();
            let response = api
                .patch(
                    self.original.name_any().as_str(),
                    &PatchParams {
                        field_manager: Some(MANAGER.to_string()),
                        dry_run: false,
                        force: false,
                        field_validation: None,
                    },
                    &Patch::<json_patch::Patch>::Json(patch),
                )
                .await;
            debug!(
                "Patch object {}/{} ({:?}) with {} -> {:?}",
                namespace,
                self.original.name_any(),
                self.original.resource_version(),
                patch_txt,
                response
            );
            match response {
                Ok(new) => {
                    self.original = new.clone();
                    self.modified = new;
                }
                Err(e) => Err(e)?,
            }
        }
        Ok(())
    }
}
