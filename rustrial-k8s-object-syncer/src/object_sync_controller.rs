use crate::{
    errors::{ControllerError, ExtKubeApiError},
    object_sync_modifications::ObjectSyncModifications,
    resource_controller::{ObjectSyncHandle, ResourceController},
    utils::{delete_destinations, metric_name, remove_finalizer},
    Configuration, FINALIZER,
};

use futures::StreamExt;
use k8s_openapi::api::core::v1::Namespace;
use kube::{
    api::{ApiResource, DynamicObject, GroupVersionKind, TypeMeta},
    discovery, Api, Client, ResourceExt,
};
use kube_runtime::{
    controller::{Action, Controller},
    reflector::Store,
    watcher::Config,
};
use log::{debug, info};
use opentelemetry::{
    global,
    metrics::{Counter, Histogram, Meter},
    KeyValue,
};
use rustrial_k8s_object_syncer_apis::{Condition, ObjectSync, SourceObject};
use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Weak},
    time::Instant,
};
use tokio::{
    sync::RwLock,
    time::{sleep, Duration},
};

const READY: &'static str = "Ready";
pub(crate) const SUCCESS: &'static str = "Success";
pub(crate) const FAILURE: &'static str = "Failure";

const OBJECT_SYNC_CONTROLLER: &'static str = "object_sync_controller";

/// Opaque handle for a [`ObjectSync`] object's registration with a [`ResourceController`].
///
struct ObjectSyncInstance {
    /// Strong reference to [`ResourceController`] to make sure the corresponding
    /// controller is alive as long as at least one [`ObjectSync`] is registered
    /// with it.
    _resource_controller: Arc<ResourceController>,
    /// If async_dropped will remove the [ObjectSync] from the corresponding [ResourceController].
    _resource_controller_handle: ObjectSyncHandle,
    /// The GVK of the [ObjectSync] source object, used to track whether [ObjectSync] spec changed.
    gvk: GroupVersionKind,
}

/// The main controller which will spawn one [`ResourceController`] instance per
/// GVK (GroupVersionKind) combination.
pub(crate) struct ObjectSyncController {
    /// The namespace cache, used to pass-on to the [ResourceController]s.
    pub namespace_cache: Store<Namespace>,
    pub configuration: Configuration,
    /// Weak reference to [`ResourceController`] instances needed to register [ObjectSyncInstance] instances.
    /// We use weak references to make sure that unused [`ResourceController`] instances are dropped
    /// to make sure there is no unecessary load on the Kubernetes API servers and that there are
    /// no stale controllers for remove API resoures (e.g. deleted CustomResourceDefinitions).
    resource_controllers: Arc<RwLock<HashMap<GroupVersionKind, Weak<ResourceController>>>>,

    /// Mapping of [ObjectSync] (namespace/name) to the corresponding [ObjectSyncInstance] object.
    /// Note, we use the peristent ID `{namespasce}/{name}` instead of the object's UID as key to make
    /// sure the system is eventual consistent in case the controller should ever miss any events
    /// caused by replacement (delete/create) of the underlying object. Practically, it should see all
    /// events, but let's be defensive it can't hurt.
    instances: Arc<RwLock<HashMap<String, ObjectSyncInstance>>>,

    reconcile_object_sync_count: Counter<u64>,
    reconcile_object_sync_duration: Histogram<u64>,
}

impl ObjectSyncController {
    pub fn new(configuration: Configuration, namespace_cache: Store<Namespace>) -> Self {
        let meter: Meter = global::meter(OBJECT_SYNC_CONTROLLER);
        let reconcile_object_sync_count = meter
            .u64_counter(metric_name("reconcile_count"))
            .with_description("Count of ObjectSync reconcile invocations")
            .build();
        let reconcile_object_sync_duration = meter
            .u64_histogram(metric_name("reconcile_duration_ms"))
            .with_description("Reconcile duration of ObjectSync objects in milliseconds")
            .with_unit("ns")
            .build();
        Self {
            namespace_cache,
            configuration,
            resource_controllers: Default::default(),
            instances: Default::default(),
            reconcile_object_sync_count,
            reconcile_object_sync_duration,
        }
    }

    async fn get_api_resource(
        &self,
        source: &SourceObject,
    ) -> Result<ApiResource, ControllerError> {
        let client = self.client();
        let group = source.group.as_str();
        let kind = source.kind.as_str();
        let apigroup = discovery::group(&client, group).await.map_err(|e| {
            ControllerError::ApiDiscoveryError(format!(
                "failed to discover detail information for API Group {}: {}",
                group, e
            ))
        })?;
        let (api_resource, cap) = match &source.version {
            Some(version) => apigroup
                .recommended_resources()
                .into_iter()
                .find(|(r, _)| r.kind == kind && r.version.as_str() == version)
                .ok_or_else(|| {
                    ControllerError::ApiDiscoveryError(format!(
                        "Kind {} in API Group {} with version {} does not exist",
                        kind, group, version
                    ))
                }),
            None => apigroup.recommended_kind(kind).ok_or_else(|| {
                ControllerError::ApiDiscoveryError(format!(
                    "unable to resolve preferred version for Kind {} in API Group {}",
                    kind, group
                ))
            }),
        }?;
        match cap.scope {
            discovery::Scope::Cluster => Err(ControllerError::ClusterScopedResource(format!(
                "resource {}/{}/{} is cluster scoped, resource syncer only supports namespace scoped resources",
                api_resource.group, api_resource.version, api_resource.kind
            ))),
            discovery::Scope::Namespaced => Ok(api_resource),
        }
    }

    async fn get_gvk(&self, source: &SourceObject) -> Result<GroupVersionKind, ControllerError> {
        let api_resource = self.get_api_resource(source).await?;
        return Ok(GroupVersionKind {
            group: api_resource.group,
            version: api_resource.version,
            kind: api_resource.kind,
        });
    }

    async fn remove_finalizer(&self, event: &mut ObjectSyncModifications) -> anyhow::Result<()> {
        let finalizers: &mut Vec<String> = &mut event.finalizers_mut();
        let original_size = finalizers.len();
        finalizers.retain(|f| f.as_str() != FINALIZER);
        if finalizers.len() != original_size {
            event.patch_spec(self.configuration.client.clone()).await?;
        }
        Ok(())
    }

    async fn add_finalizer_if_missing(
        &self,
        event: &mut ObjectSyncModifications,
    ) -> anyhow::Result<()> {
        let finalizers = &mut event.finalizers_mut();
        if finalizers
            .iter()
            .find(|f| f.as_str() == FINALIZER)
            .is_none()
        {
            finalizers.push(FINALIZER.to_string());
            event.patch_spec(self.configuration.client.clone()).await?;
        }
        Ok(())
    }

    async fn remove(&self, event: &ObjectSyncModifications) -> Option<GroupVersionKind> {
        let mut instances = self.instances.write().await;
        if let Some(instance) = instances.remove(&event.id()) {
            let gvk = instance.gvk;
            debug!(
                "removing {} from ResourceController for resource {}/{}/{}",
                event.id(),
                gvk.group,
                gvk.version,
                gvk.kind
            );
            Some(gvk)
            // The opaque ObjectSyncInstance handle `instance` is dropped here and
            // its `Drop` implementation will make sure it is deregisterd from its
            // `ResourceController`.
        } else {
            warn!(
                "{} was not registered with any ResourceController",
                event.id()
            );
            None
        }
    }

    async fn add(
        &self,
        event: &ObjectSyncModifications,
        gvk: GroupVersionKind,
    ) -> Result<(), ControllerError> {
        let (resource_controller_handle, resource_controller) = {
            let mut controllers = self.resource_controllers.write().await;
            let controller = controllers.get(&gvk).map(|w| w.upgrade()).flatten();
            let controller = if let Some(controller) = controller {
                controller
            } else {
                let controller: ResourceController = ResourceController::new(
                    self.configuration.clone(),
                    self.namespace_cache.clone(),
                    gvk.clone(),
                )
                .await?;
                let controller = Arc::new(controller);
                controllers.insert(gvk.clone(), Arc::downgrade(&controller));
                controller
            };
            (controller.register(event).await, controller.clone())
        };
        let mut instances = self.instances.write().await;
        instances.insert(
            event.id(),
            ObjectSyncInstance {
                _resource_controller: resource_controller,
                _resource_controller_handle: resource_controller_handle,
                gvk,
            },
        );
        Ok(())
    }

    fn client(&self) -> Client {
        self.configuration.client.clone()
    }

    async fn delete(&self, event: &mut ObjectSyncModifications) -> Result<(), ControllerError> {
        // Remove from ResourceController
        self.remove(event).await;
        // Delete all remaining destinations
        if let Some(destinations) = event
            .status
            .as_ref()
            .map(|v| v.destinations.as_ref())
            .flatten()
        {
            let remaining = delete_destinations(self.client(), destinations).await?;
            if !remaining.is_empty() {
                let errors = remaining.len();
                event.update_destinations(remaining);
                event.replace_status(self.client()).await?;
                return Err(ControllerError::DestinationRemovalError(format!(
                    "failed to remove {} destinations of {}",
                    errors,
                    event.id()
                )));
            }
        }
        info!(
            "successfully removed all destination objects of {}",
            event.id()
        );
        // Remove finalizer from source object.
        let gvk = self.get_gvk(&event.spec.source).await?;
        let api_resource = ApiResource::from_gvk(&gvk);
        let namespace = event
            .spec
            .source
            .namespace
            .clone()
            .or(event.namespace())
            .unwrap_or_else(|| "".to_string());
        let api: Api<DynamicObject> =
            Api::namespaced_with(self.client(), namespace.as_str(), &api_resource);
        match api.get(event.spec.source.name.as_str()).await {
            Ok(mut source) => {
                source.types = source.types.or(Some(TypeMeta {
                    kind: gvk.kind.clone(),
                    api_version: gvk.api_version(),
                }));
                remove_finalizer(api, &mut source, FINALIZER).await?;
            }
            Err(e) if e.is_not_found() => (),
            Err(e) => Err(e)?,
        }
        // Remove finalizer from ObjectSync object.
        self.remove_finalizer(event).await?;
        Ok(())
    }

    async fn check(&self, event: &mut ObjectSyncModifications) -> Result<(), ControllerError> {
        // First of all make sure the finalizer is in place.
        self.add_finalizer_if_missing(event).await?;
        // Discover GroupVersionKind from SourceObject, which might fail if the
        // corresponding API (e.g. CustomResourceDefinition/CRD) is not yet installed
        // or temporarily unavailable. On failure the reconciliation for this object
        // will be retried according to the error handling policy (see error_policy below).
        let current_gvk = self.get_gvk(&event.spec.source).await?;
        // Obtain the configuration checksum and immediately release the read lock
        // guard again.
        let gvk = {
            let instances = self.instances.read().await;
            instances.get(&event.id()).map(|v| v.gvk.clone())
        };
        let condition = if let Some(gvk) = gvk {
            if gvk != current_gvk {
                // GVK changed so remove from current ResourceController
                self.remove(event).await;
                // and add it again with its GVK
                self.add(&event, current_gvk.clone()).await?;
                Some(Condition::new(
                    READY,
                    Some(true),
                    SUCCESS,
                    format!(
                        "Successfully moved from ResourceController {}/{}/{} to {}/{}/{}",
                        gvk.group,
                        gvk.version,
                        gvk.kind,
                        current_gvk.group,
                        current_gvk.version,
                        current_gvk.kind
                    ),
                ))
            } else {
                None
            }
        } else {
            // Not yet registered so add it now.
            self.add(&event, current_gvk.clone()).await?;
            Some(Condition::new(
                READY,
                Some(true),
                SUCCESS,
                format!(
                    "Successfully registered with ResourceController {}/{}/{}",
                    current_gvk.group, current_gvk.version, current_gvk.kind
                ),
            ))
        };
        if let Some(condition) = condition {
            event.update_condition(condition);
            event.replace_status(self.client()).await?;
        }
        Ok(())
    }

    /// Controller triggers this whenever our main object changed
    async fn reconcile(object: Arc<ObjectSync>, ctx: Arc<Self>) -> Result<Action, ControllerError> {
        let me = ctx.as_ref();
        let mut event = ObjectSyncModifications::new(object.as_ref().clone());
        let namespace = event.namespace().unwrap_or_else(|| "".to_string());
        if me
            .configuration
            .watch_namespaces
            .as_ref()
            .map_or(true, |v| {
                v.is_empty() || v.contains(namespace.as_str()) || v.contains("*")
            })
        {
            let start = Instant::now();
            if event.is_deleted() {
                ctx.as_ref().delete(&mut event).await?;
            } else {
                if let Err(e) = ctx.as_ref().check(&mut event).await {
                    event.update_condition(Condition::new(
                        READY,
                        Some(false),
                        FAILURE,
                        format!("{}", e),
                    ));
                    event.replace_status(ctx.as_ref().client()).await?;
                    Err(e)?
                }
            };
            let duration = Instant::now() - start;

            let labels = &[
                KeyValue::new("object_name", event.name_any()),
                KeyValue::new("object_namespace", namespace),
            ];
            me.reconcile_object_sync_count.add(1, labels);
            me.reconcile_object_sync_duration
                .record(duration.as_millis() as u64, labels);
        } else {
            debug!(
                "Ignore {} as its namespace is not in the set of namespaces to watch for ObjectSync objects",
                event.id()
            );
        }

        Ok(Action::requeue(Duration::from_secs(3600)))
    }

    /// The controller triggers this on reconcile errors
    ///
    /// Arc<K>, &ReconcilerFut::Error, Arc<Ctx>
    fn error_policy(_object: Arc<ObjectSync>, error: &ControllerError, _ctx: Arc<Self>) -> Action {
        if error.is_temporary() {
            Action::requeue(Duration::from_secs(30))
        } else {
            Action::requeue(Duration::from_secs(300))
        }
    }

    pub fn start(self) -> impl Future<Output = ()> {
        let controller =
            Controller::new(self.configuration.resource_sync.clone(), Config::default());
        let controller = controller
            .run(Self::reconcile, Self::error_policy, Arc::new(self))
            .for_each(|res| async move {
                match res {
                    Ok(o) => {
                        //counter!("reconcile_k8s_resource_sync_success", 1);
                        debug!("reconciled {:?}", o);
                    }
                    Err(e) => {
                        let meter: Meter = global::meter(OBJECT_SYNC_CONTROLLER);
                        let reconcile_object_sync_errors = meter
                            .u64_counter(metric_name("reconcile_errors"))
                            .with_description(
                                "Count of reconcile invocation errors for ObjectSync resources",
                            )
                            .build();
                        let labels = &[];
                        match e {
                            a @ kube_runtime::controller::Error::QueueError { .. } => {
                                debug!("reconcile failed: {:?}", a);
                                reconcile_object_sync_errors.add(1, labels);
                                // Slow down on errors caused by missing CRDs or permissions.
                                sleep(Duration::from_secs(30)).await;
                            }
                            a @ kube_runtime::controller::Error::ObjectNotFound { .. } => {
                                debug!("reconcile failed: {:?}", a);
                            }
                            e => {
                                warn!("reconcile failed: {:?}", e);
                                reconcile_object_sync_errors.add(1, labels);
                            }
                        };
                    }
                }
            });
        controller
    }
}
