use crate::{
    Configuration, FINALIZER, MANAGER,
    errors::{ControllerError, ExtKubeApiError},
    object_sync_controller::{FAILURE, ObjectSyncController, SUCCESS},
    object_sync_modifications::ObjectSyncModifications,
    utils::{
        NamespacedName, ObjectSyncRef, SourceRef, add_finalizer_if_missing, delete_destinations,
        ensure_gvk, metric_name, remove_finalizer,
    },
};
use futures::{
    SinkExt, StreamExt,
    channel::mpsc::{Receiver, Sender, channel},
};
use k8s_openapi::{api::core::v1::Namespace, chrono::Utc};
use kube::{
    Api, Client, ResourceExt,
    api::{ApiResource, DynamicObject, GroupVersionKind, Patch, PatchParams, PostParams},
};
use kube_runtime::{
    controller::{Action, Controller},
    reflector::{ObjectRef, Store},
    watcher::{self, Config},
};
use log::{debug, error, info};
use opentelemetry::{
    KeyValue,
    global::{self},
    metrics::{Counter, Histogram, Meter},
};
use rustrial_k8s_object_syncer_apis::{
    Condition, DestinationStatus, ObjectRevision, ObjectSync, ObjectSyncSpec,
    SOURCE_OBJECT_ANNOTATION, SyncStrategy,
};
use std::{
    borrow::BorrowMut,
    collections::{HashMap, HashSet},
    future::Future,
    ops::Deref,
    sync::Arc,
    time::Instant,
};
use tokio::{
    spawn,
    sync::{Mutex, RwLock},
    task::JoinHandle,
    time::{Duration, sleep},
};

pub(crate) const IN_SYNC: &'static str = "InSync";

pub struct ObjectSyncHandle {
    sources: Arc<RwLock<HashMap<SourceRef, HashSet<ObjectSyncRef>>>>,
    pub src: SourceRef,
    crd: ObjectSyncRef,
}

impl ObjectSyncHandle {
    /// Basically, that is the AsyncDrop impl, but Rust does not yet support
    /// AsyncDrop and we will call this from Drop impl.
    async fn async_drop(&mut self) {
        let mut guard = self.sources.write().await;
        if let Some(crds) = guard.get_mut(&self.src) {
            crds.remove(&self.crd);
            if crds.is_empty() {
                guard.remove(&self.src);
            }
        }
    }
}

impl Drop for ObjectSyncHandle {
    fn drop(&mut self) {
        futures::executor::block_on(self.async_drop())
    }
}

/// [`ResourceController`] tracks objects of a specific GVK
struct ResourceControllerImpl {
    configuration: Configuration,
    api_resource: ApiResource,
    gvk: GroupVersionKind,
    namespace_cache: Store<Namespace>,
    /// src-ref -> ObjectSync-ref
    sources: Arc<RwLock<HashMap<SourceRef, HashSet<ObjectSyncRef>>>>,
    resource_reconcile_count: Counter<u64>,
    resource_reconcile_duration: Histogram<u64>,
}

const RESOURCE_CONTROLLER: &'static str = "resource_controller";

impl ResourceControllerImpl {
    pub fn new(
        configuration: Configuration,
        namespace_cache: Store<Namespace>,
        gvk: GroupVersionKind,
        sources: Arc<RwLock<HashMap<SourceRef, HashSet<ObjectSyncRef>>>>,
    ) -> Self {
        let api_resource = ApiResource::from_gvk(&gvk);
        let meter: Meter = global::meter(RESOURCE_CONTROLLER);
        let resource_reconcile_count = meter
            .u64_counter(metric_name("resource_reconcile_count"))
            .with_description("Count of resources specific reconcile invocations for objects managed by at least one ObjectSync instance")
            .build();
        let resource_reconcile_duration = meter
            .u64_histogram(metric_name("resource_reconcile_duration_ms"))
            .with_description("Resource specific reconciliation duration in milliseconds")
            .with_unit("ns")
            .build();
        Self {
            configuration,
            api_resource,
            gvk,
            namespace_cache,
            sources,
            resource_reconcile_count,
            resource_reconcile_duration,
        }
    }

    fn client(&self) -> Client {
        self.configuration.client.clone()
    }

    fn namespaced_api(&self, namespace: &str) -> Api<DynamicObject> {
        Api::namespaced_with(self.client(), namespace, &self.api_resource)
    }

    async fn source_deleted(
        &self,
        rs: &mut ObjectSyncModifications,
    ) -> Result<(), ControllerError> {
        ObjectSyncController::delete_destinations(self.client(), rs).await?;
        Ok(())
    }

    /// Get an iterator over all expected destinations
    fn expected_destinations<'a>(
        &self,
        event: &'a ObjectSyncModifications,
    ) -> impl Iterator<Item = (String, String, Option<SyncStrategy>)> + 'a {
        let spec: &ObjectSyncSpec = &event.spec;
        let cache = self.namespace_cache.state();
        spec.destinations.iter().flat_map(move |d| {
            let mut tmp: Vec<(String, String, Option<SyncStrategy>)> = Default::default();
            if d.applies_to_all_namespaces() {
                for ns in cache.iter() {
                    // Make sure we skip deleted namespaces, as otherwise the finalizers on the synced
                    // destination objects will prevent the namespace from being deleted.
                    if ns.metadata.deletion_timestamp.is_none() {
                        if let Some((ns, name)) = d.applies_to(event, ns.name_any().as_str()) {
                            tmp.push((ns, name, d.strategy));
                        }
                    }
                }
            } else if let Some((namespace, name)) = d.applies_to(event, d.namespace.as_str()) {
                if let Some(ns) = cache.iter().find(|ns| ns.name_any() == d.namespace) {
                    // Make sure we skip deleted namespaces, as otherwise the finalizers on the synced
                    // destination objects will prevent the namespace from being deleted.
                    if ns.metadata.deletion_timestamp.is_none() {
                        tmp.push((namespace, name, d.strategy));
                    }
                }
            }
            tmp
        })
    }

    async fn upsert_destinations(
        &self,
        source: &DynamicObject,
        destinations: &mut Vec<DestinationStatus>,
        stale_remnants: &Vec<DestinationStatus>,
    ) -> Result<(bool, usize, usize), ControllerError> {
        let src_version = ObjectRevision {
            uid: source.uid(),
            resource_version: source.resource_version(),
        };
        let mut template = source.clone();
        template.labels_mut().insert(
            "app.kubernetes.io/managed-by".to_string(),
            MANAGER.to_string(),
        );
        template.annotations_mut().insert(
            SOURCE_OBJECT_ANNOTATION.to_string(),
            format!(
                "{}/{}",
                source.namespace().as_deref().unwrap_or(""),
                source.name_any()
            ),
        );
        let mut changed = false;
        let mut pp = PostParams::default();
        pp.field_manager = Some(MANAGER.to_string());

        let expected_success = destinations.len();
        let mut observed_success = 0usize;
        for d in destinations {
            // Skip over remnants (stale destinations for which deletion failed).
            if stale_remnants
                .iter()
                .find(|v| Self::is_same_destination(v, d))
                .is_some()
            {
                continue;
            }
            template.metadata.namespace = Some(d.namespace.clone());
            template.metadata.name = Some(d.name.clone());
            template.metadata.generation = Default::default();
            template.metadata.generate_name = Default::default();
            template.metadata.managed_fields = Default::default();
            template.metadata.owner_references = Default::default();
            template.metadata.self_link = Default::default();
            template.metadata.creation_timestamp = Default::default();
            template.metadata.finalizers = Some(vec![FINALIZER.to_string()]);
            let api = self.namespaced_api(d.namespace.as_str());
            let mut retry_attempts = 3i32;
            while retry_attempts > 0 {
                template.metadata.uid = Default::default();
                template.metadata.resource_version = Default::default();
                retry_attempts -= 1;
                match api.get(d.name.as_str()).await {
                    Ok(mut current) => {
                        if current.metadata.deletion_timestamp.is_some() {
                            // If a destination object has been deleted, remove the finalizer to make sure
                            // it gets properly removed by the API server and that we can recreate and sync
                            // it.
                            match remove_finalizer(api.clone(), &mut current, FINALIZER).await {
                                Ok(true) => {
                                    debug!(
                                        "removed finalizer from deleted object {} {}/{}",
                                        self.gvk.kind, d.namespace, d.name
                                    );
                                    continue;
                                }
                                Err(e) => warn!(
                                    "failed to remove finalizer from deleted object {} {}/{}: {}",
                                    self.gvk.kind, d.namespace, d.name, e
                                ),
                                _ => (),
                            }
                        }
                        let dst_version = ObjectRevision {
                            uid: current.uid(),
                            resource_version: current.resource_version(),
                        };
                        if &Some(dst_version) != &d.synced_version
                            || &Some(&src_version) != &d.source_version.as_ref()
                        {
                            template.metadata.uid = current.uid();
                            template.metadata.resource_version = current.resource_version();
                            let result = match &d.strategy() {
                                SyncStrategy::Replace => {
                                    api.replace(d.name.as_str(), &pp, &template).await
                                }
                                SyncStrategy::Apply => {
                                    let mut pp = PatchParams::default();
                                    pp.field_manager = Some(MANAGER.to_string());
                                    pp.force = true;
                                    api.patch(d.name.as_str(), &pp, &Patch::Apply(&template))
                                        .await
                                }
                            };
                            match result {
                                Ok(updated) => {
                                    d.synced_version = Some(ObjectRevision {
                                        uid: updated.uid(),
                                        resource_version: updated.resource_version(),
                                    });
                                    d.source_version = Some(src_version.clone());
                                    changed = true;
                                    observed_success += 1;
                                    break;
                                }
                                Err(e) if e.is_not_found() || e.is_conflict() => {
                                    debug!(
                                        "temporarily failed to update destination object {} {}/{}: {}",
                                        self.gvk.kind, d.namespace, d.name, e
                                    );
                                    continue;
                                }
                                Err(e) => {
                                    error!(
                                        "failed to update destination object {} {}/{}: {}",
                                        self.gvk.kind, d.namespace, d.name, e
                                    );
                                    break;
                                }
                            }
                        } else {
                            observed_success += 1;
                            break;
                        }
                    }
                    Err(e) if e.is_not_found() => match api.create(&pp, &template).await {
                        Ok(created) => {
                            d.synced_version = Some(ObjectRevision {
                                uid: created.uid(),
                                resource_version: created.resource_version(),
                            });
                            d.source_version = Some(src_version.clone());
                            changed = true;
                            observed_success += 1;
                            break;
                        }
                        Err(e) if e.is_conflict() => {
                            debug!(
                                "temporarily failed to create destination object {}/{}: {}",
                                d.namespace, d.name, e
                            );
                            continue;
                        }
                        Err(e) => {
                            error!(
                                "failed to create destination object {}/{}: {}",
                                d.namespace, d.name, e
                            );
                            break;
                        }
                    },
                    Err(e) => {
                        error!(
                            "failed to create/update destination object {}/{}: {}",
                            d.namespace, d.name, e
                        );
                        break;
                    }
                }
            }
        }
        Ok((changed, expected_success, observed_success))
    }

    fn is_same_destination(me: &DestinationStatus, other: &DestinationStatus) -> bool {
        me.name == other.name
            && me.namespace == other.namespace
            && me.group == other.group
            && me.kind == other.kind
    }

    async fn reconcile_source(
        &self,
        event: &mut ObjectSyncModifications,
        source: &DynamicObject,
    ) -> Result<(), ControllerError> {
        let mut stale_destinations = event
            .status
            .as_ref()
            .map(|v| v.destinations.clone())
            .flatten()
            .unwrap_or_default();
        let mut expected_destinations: Vec<DestinationStatus> = Default::default();
        for (dst_namespace, dst_name, strategy) in self.expected_destinations(event) {
            let mut expected_dst = DestinationStatus {
                name: dst_name,
                namespace: dst_namespace,
                source_version: None,
                synced_version: None,
                group: self.gvk.group.clone(),
                version: self.gvk.version.clone(),
                kind: self.gvk.kind.clone(),
                strategy,
            };
            if let Some(status) = stale_destinations
                .iter()
                .find(|d| Self::is_same_destination(d, &expected_dst))
            {
                // If strategy (sync config) changed do not set version to make sure the destination
                // object is update. Note, this is required as we cannot track the ObjectSync's resourceVersion
                // in its own status as this would lead to an infinit reconciliation cycle.
                if status.strategy() == expected_dst.strategy() {
                    expected_dst.source_version = status.source_version.clone();
                    expected_dst.synced_version = status.synced_version.clone();
                }
                // As destination is in set of expected destinations, remove it from the set of
                // stale destinations.
                stale_destinations.retain(|d| !Self::is_same_destination(d, &expected_dst));
            }
            expected_destinations.push(expected_dst);
        }
        // 1. Remove stale destinations.
        let stales = stale_destinations.len();
        let stale_remnants = delete_destinations(self.client(), &stale_destinations).await?;
        let removed_count = stales - stale_remnants.len();
        // 2. Update status sub-resources with active destinations.
        //    Deterministically sort destinations to avoid unnecessary updates.
        //    Note, it is important that we update the status sub-resource before
        //    we effectivley create new synced objects, to make sure they are
        //    tracked for garbage collection.
        expected_destinations.extend(stale_remnants.clone());
        expected_destinations.sort();
        event.update_destinations(expected_destinations);
        //    If updating the status sub-resource fails, bail out.
        event.replace_status(self.client()).await?;
        // 3. Create / Update destination objects
        let condition = if let Some(status) = &mut event.status {
            if let Some(destinations) = &mut status.destinations {
                let (updated, expected, observed) = self
                    .upsert_destinations(&source, destinations, &stale_remnants)
                    .await?;
                if updated || removed_count > 0 {
                    let errors = expected - observed;
                    Some(Condition::new(
                        IN_SYNC,
                        Some(errors == 0),
                        if errors == 0 { SUCCESS } else { FAILURE },
                        format!("{} out of {} destinations are in sync", observed, expected),
                    ))
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        if let Some(condition) = condition {
            // 4. if any destinations objects were created or updated, also
            //    update the status sub-resource.
            event.update_condition(condition);
            event.replace_status(self.client()).await?;
        }
        Ok(())
    }

    async fn get_object_sync(
        &self,
        namespaced_name: &NamespacedName,
        sync_configuration: &NamespacedName,
    ) -> Option<ObjectSyncModifications> {
        let rs_api =
            Api::<ObjectSync>::namespaced(self.client(), sync_configuration.namespace.as_str());
        match rs_api.get(sync_configuration.name.as_str()).await {
            Ok(rs) => Some(ObjectSyncModifications::new(rs)),
            Err(e) => {
                if e.is_not_found() {
                    // Note, ObjectSync deletion is handled in the main controller not here in the
                    // ResourceController.
                    debug!(
                        "ObjectSync {} references by {} does no longer exist: {}",
                        sync_configuration, namespaced_name, e
                    );
                    None
                } else {
                    error!(
                        "Error while retrieving ObjectSync {} for {}: {}",
                        sync_configuration, namespaced_name, e
                    );
                    None
                }
            }
        }
    }

    async fn lookup_object_syncs_for_source(
        &self,
        source_ref: &SourceRef,
    ) -> Option<Vec<ObjectSyncModifications>> {
        let is_source_namespace = self
            .configuration
            .source_namespaces
            .as_ref()
            .map_or(true, |v| {
                v.is_empty() || v.contains(source_ref.namespace.as_str()) || v.contains("*")
            });
        // Get ObjectSync object names that reference this object if
        // the objct resides in on of the tracked source namespaces.
        let refs = if is_source_namespace {
            let guard = self.sources.read().await;
            guard.get(&source_ref).cloned()
        } else {
            None
        }
        .filter(|c| !c.is_empty());
        if let Some(refs) = refs {
            let mut objs = vec![];
            for r in &refs {
                // Load ObjectSync by name
                if let Some(rs) = self.get_object_sync(&source_ref, &r).await
                    // but only use it if its source still matches
                    && SourceRef::from(&rs) == *source_ref
                {
                    objs.push(rs);
                }
            }
            Some(objs)
        } else {
            None
        }
    }

    /// Controller triggers this whenever our main object or our children changed
    async fn reconcile(
        source: Arc<DynamicObject>,
        ctx: Arc<Self>,
    ) -> Result<Action, ControllerError> {
        let start = Instant::now();
        let me = ctx.as_ref();
        let source_ref = SourceRef::from(source.as_ref());

        let source_id = format!(
            "{}/{}/{} {}",
            me.gvk.group, me.gvk.version, me.gvk.kind, source_ref
        );
        // Get ObjectSync object names that reference this object if
        // the objct resides in on of the tracked source namespaces.
        let sync_configurations = me.lookup_object_syncs_for_source(&source_ref).await;
        // Add type information required by server-side apply.
        let mut source = ensure_gvk(source.as_ref().clone(), &me.gvk);
        if let Some(sync_configurations) = sync_configurations {
            let labels = &[
                KeyValue::new("group", me.gvk.group.clone()),
                KeyValue::new("version", me.gvk.version.clone()),
                KeyValue::new("kind", me.gvk.kind.clone()),
                KeyValue::new("object_name", source.name_any()),
                KeyValue::new("object_namespace", source_ref.namespace.clone()),
            ];
            if source.metadata.deletion_timestamp.is_some() {
                for mut sync_configuration in sync_configurations {
                    let mut errors = 0;
                    if let Err(_) = me.source_deleted(&mut sync_configuration).await {
                        errors += 1;
                    }

                    if errors == 0 {
                        if let Err(e) = remove_finalizer(
                            me.namespaced_api(source_ref.namespace.as_str()),
                            &mut source,
                            FINALIZER,
                        )
                        .await
                        {
                            error!("failed to remove finalizer from {}: {}", source_id, e);
                        }
                    }
                }
            } else {
                if let Err(e) = add_finalizer_if_missing(
                    me.namespaced_api(source_ref.namespace.as_str()),
                    &mut source,
                    FINALIZER,
                )
                .await
                {
                    error!("failed to add finalizer to {}: {}", source_id, e);
                }
                for mut sync_configuration in sync_configurations {
                    if let Err(e) = me.reconcile_source(&mut sync_configuration, &source).await {
                        error!(
                            "failed to reconcile {} for {}: {}",
                            source_id,
                            sync_configuration.id(),
                            e
                        );
                    }
                }
            }
            // Only update metrics
            let duration = Instant::now() - start;
            me.resource_reconcile_count.add(1, labels);
            me.resource_reconcile_duration
                .record(duration.as_millis() as u64, labels);
            // Requeue objects tracked by ObjectSync configuration to make sure any
            // downstream destination drift is elminitated in an eventual consistent manner.
            Ok(Action::requeue(Duration::from_secs(300)))
        } else {
            debug!(
                "ignoring {} as it is not references by any ObjectSync instance",
                source_id,
            );
            // Remove finalizer from source objects, but not from target objects unless they are
            // marked for deletion (target objects have the SOURCE_OBJECT_ANNOTATION annotation set).
            if source.annotations().get(SOURCE_OBJECT_ANNOTATION).is_none()
                || source.metadata.deletion_timestamp.is_some()
            {
                if let Err(e) = remove_finalizer(
                    me.namespaced_api(source_ref.namespace.as_str()),
                    &mut source,
                    FINALIZER,
                )
                .await
                {
                    error!(
                        "failed to remove finalizer from no longer referenced source {}: {}",
                        source_id, e
                    );
                    return Ok(Action::requeue(Duration::from_secs(5)));
                }
            }
            // No need to requeue objects not tracked by any ObjectSync configuration.
            Ok(Action::await_change())
        }
    }

    /// The controller triggers this on reconcile errors
    fn error_policy(
        _object: Arc<DynamicObject>,
        error: &ControllerError,
        _ctx: Arc<Self>,
    ) -> Action {
        if error.is_temporary() {
            Action::requeue(Duration::from_secs(30))
        } else {
            Action::requeue(Duration::from_secs(300))
        }
    }

    /// Get an optimized API instance.
    fn api(&self, namespaces: &Option<HashSet<String>>) -> Api<DynamicObject> {
        match namespaces {
            Some(namespaces) if namespaces.len() == 1 => match namespaces.iter().next() {
                Some(ns) => Api::namespaced_with(self.client(), ns.as_str(), &self.api_resource),
                _ => Api::all_with(self.client(), &self.api_resource),
            },
            _ => Api::all_with(self.client(), &self.api_resource),
        }
    }

    pub async fn start(
        self,
        reload: Receiver<()>,
    ) -> Result<impl Future<Output = ()>, ControllerError> {
        let target_namespaces = self.configuration.target_namespaces.clone();
        let target_namespaces2 = target_namespaces.clone();
        let api_resource = self.api_resource.clone();
        let api_resource2 = self.api_resource.clone();
        let api_resource3 = self.api_resource.clone();
        let src_api = self.api(&self.configuration.source_namespaces);
        let dst_api = self.api(&self.configuration.target_namespaces);
        let config = Config::default();
        let controller = Controller::new_with(src_api, config, self.api_resource.clone());
        let sources = self.sources.clone();
        let sources2 = sources.clone();
        let mut lp_dst = watcher::Config::default();
        lp_dst.label_selector = Some(format!("app.kubernetes.io/managed-by={}", MANAGER));
        let controller = controller
            .reconcile_all_on(reload)
            // Watch namespaces to track newly created namespaces.
            .watches(
                Api::<Namespace>::all(self.client()),
                watcher::Config::default(),
                move |namespace| {
                    let is_target_namespace = target_namespaces2
                        .as_ref()
                        .map_or(true, |v| v.contains(namespace.name_any().as_str()));
                    if let Some(ct) = &namespace.metadata.creation_timestamp {
                        let age = Utc::now() - ct.0;
                        if is_target_namespace && age.num_seconds() < 300 {
                            // reconcile all source if namespace has been created in the last 5 minutes.
                            let guard = futures::executor::block_on(sources.read());
                            let tmp: Vec<ObjectRef<DynamicObject>> = guard
                                .values()
                                .flatten()
                                .map(|v| v.object_ref(&api_resource))
                                .collect();
                            return tmp;
                        }
                    }
                    vec![]
                },
            )
            // Watch ObjectSync objects, to track destination changes.
            .watches(
                self.configuration.resource_sync.clone(),
                watcher::Config::default(),
                move |rs| {
                    let object_sync_ref = ObjectSyncRef::from(&rs);
                    let guard = futures::executor::block_on(sources2.read());
                    let affected: Vec<ObjectRef<DynamicObject>> = guard
                        .values()
                        .flatten()
                        .filter(|v| *v == &object_sync_ref)
                        .map(|v| v.object_ref(&api_resource2))
                        .collect();
                    affected
                },
            )
            // Watch destinations objects, to track destination drift. Note, this only works realiably if
            // the `app.kubernetes.io/managed-by` label and the `sync.rustrial.org/source-object` annotation
            // are still set properly. If those are changed, drift will be removed in an eventual
            // consistent approach by setting `Action::requeue_after`.
            .watches_with(
                dst_api,
                self.api_resource.clone(),
                lp_dst,
                move |rs: DynamicObject| {
                    let namespace = rs.namespace().unwrap_or_else(|| "".to_string());
                    let is_target_namespace = target_namespaces
                        .as_ref()
                        .map_or(true, |v| v.contains(namespace.as_str()));
                    if let Some(annotation) = rs.annotations().get(SOURCE_OBJECT_ANNOTATION) {
                        let parts: Vec<&str> = annotation.split("/").collect();
                        match parts.as_slice() {
                            [ns, name] if is_target_namespace => Some(
                                ObjectRef::<DynamicObject>::new_with(name, api_resource3.clone())
                                    .within(ns),
                            ),
                            _ => None,
                        }
                    } else {
                        None
                    }
                },
            )
            .run(Self::reconcile, Self::error_policy, Arc::new(self))
            .for_each(|res| async move {
                match res {
                    Ok(_o) => {}
                    Err(e) => {
                        let meter: Meter = global::meter(RESOURCE_CONTROLLER);
                        let reconcile_kind_errors = meter
                            .u64_counter(metric_name("resource_reconcile_errors"))
                            .with_description(
                                "Count of reconcile invocation errors for generic resources",
                            )
                            .build();
                        match e {
                            a @ kube_runtime::controller::Error::QueueError { .. } => {
                                debug!("reconcile failed: {:?}", a);
                                reconcile_kind_errors.add(1, &[]);
                                // Slow down on errors caused by missing CRDs or permissions.
                                sleep(Duration::from_secs(30)).await;
                            }
                            a @ kube_runtime::controller::Error::ObjectNotFound { .. } => {
                                debug!("reconcile failed: {:?}", a);
                            }
                            e => {
                                warn!("reconcile failed: {:?}", e);
                                reconcile_kind_errors.add(1, &[]);
                            }
                        };
                    }
                }
            });
        Ok(controller)
    }
}

/// [`ResourceController`] tracks objects of a specific GVK
pub(crate) struct ResourceController {
    gvk: GroupVersionKind,
    /// Mapping from GVK source object name to names of [`ObjectSync`] objects, which
    /// reference that source object.
    sources: Arc<RwLock<HashMap<SourceRef, HashSet<ObjectSyncRef>>>>,
    /// The handle to the effective controller, needed to stop (abort) it.
    join_handle: JoinHandle<()>,
    reload_sender: Mutex<Sender<()>>,
}

impl Drop for ResourceController {
    fn drop(&mut self) {
        info!(
            "stopping resource controller for {}/{}/{} as it is no longer used",
            self.gvk.group, self.gvk.version, self.gvk.kind
        );
        self.join_handle.abort();
    }
}

impl ResourceController {
    pub async fn new(
        config: Configuration,
        namespace_cache: Store<Namespace>,
        gvk: GroupVersionKind,
    ) -> Result<Self, ControllerError> {
        let (reload_sender, reload_receiver) = channel(0);
        let sources: Arc<RwLock<HashMap<SourceRef, HashSet<ObjectSyncRef>>>> = Default::default();
        let inner =
            ResourceControllerImpl::new(config, namespace_cache, gvk.clone(), sources.clone());
        let join_handle = spawn(inner.start(reload_receiver).await?);
        let me = Self {
            gvk,
            sources,
            join_handle,
            reload_sender: Mutex::new(reload_sender),
        };
        Ok(me)
    }

    pub async fn register(&self, event: &ObjectSyncModifications) -> ObjectSyncHandle {
        let src_name = SourceRef::from(event);
        let crd_name = ObjectSyncRef::from(event.deref());
        {
            let mut guard = self.sources.write().await;
            match guard.get_mut(&src_name) {
                Some(s) => {
                    s.insert(crd_name.clone());
                }
                None => {
                    let mut s = HashSet::new();
                    s.insert(crd_name.clone());
                    guard.insert(src_name.clone(), s);
                }
            }
        }
        // Now trigger reconciliation
        if let Err(e) = self.reload_sender.lock().await.borrow_mut().send(()).await {
            error!("{}", e)
        }
        ObjectSyncHandle {
            sources: self.sources.clone(),
            src: src_name,
            crd: crd_name,
        }
    }
}
