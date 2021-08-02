#[macro_use]
extern crate log;

use std::{collections::HashSet, iter::FromIterator};

use futures::TryStreamExt;
use k8s_openapi::api::core::v1::Namespace;
use kube::{api::ListParams, Api, Client};
use kube_runtime::{
    reflector::{reflector, store::Writer},
    utils::try_flatten_applied,
    watcher,
};

use prometheus_exporter::start_prometheus_metrics_server;
use rustrial_k8s_object_syncer_apis::ObjectSync;

mod object_sync_controller;
use object_sync_controller::*;
mod errors;
mod object_sync_modifications;
mod prometheus_exporter;
mod resource_controller;
mod utils;

/// The K8s field manager name.
const MANAGER: &'static str = "rustrial-object-syncer";

/// The K8s finalizer name.
///
/// Note, changing the finalizer name is a breaking change and needs
/// additional code to remove the old finalizer (name) from all affected
/// K8s objects. So, think twice before you rename it, otherwise users might
/// be stuck with K8s objects which cannot be deleted as they have a finalizer
/// set which is not automatically removed.
const FINALIZER: &'static str = "sync.rustrial.org/rustrial-object-syncer";

#[derive(Clone)]
struct Configuration {
    client: Client,
    resource_sync: Api<ObjectSync>,
    watch_namespaces: Option<HashSet<String>>,
    source_namespaces: Option<HashSet<String>>,
    target_namespaces: Option<HashSet<String>>,
}

impl Configuration {
    pub fn new(client: Client) -> Self {
        let watch_namespaces: Option<Vec<String>> =
            env_var("WATCH_NAMESPACES").map(|v| v.split(",").map(|v| v.to_string()).collect());
        let source_namespaces: Option<HashSet<String>> =
            env_var("SOURCE_NAMESPACES").map(|v| v.split(",").map(|v| v.to_string()).collect());
        let target_namespaces: Option<HashSet<String>> =
            env_var("TARGET_NAMESPACES").map(|v| v.split(",").map(|v| v.to_string()).collect());
        let object_sync_api = if let Some([ns]) = &watch_namespaces.as_deref() {
            // Optimize for the use-case where exactly one watch-namespace is provided.
            info!("Controller is only watching resources in namespace {}", ns);
            Api::<ObjectSync>::namespaced(client.clone(), ns.as_str())
        } else {
            if let Some(namespaces) = &watch_namespaces {
                info!(
                    "Controller is watching resources in namespaces: {}",
                    namespaces.join(",")
                );
            } else {
                info!("Controller is watching resources in all namespaces");
            }
            Api::<ObjectSync>::all(client.clone())
        };
        Configuration {
            client: client,
            resource_sync: object_sync_api,
            watch_namespaces: watch_namespaces.map(HashSet::from_iter),
            source_namespaces,
            target_namespaces,
        }
    }
}

fn env_var(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

async fn ok<T, E>(_: T) -> Result<(), E> {
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();
    let prometheus_metrics_exporter = opentelemetry_prometheus::exporter().init();
    let prometheus_metrics_exporter = start_prometheus_metrics_server(prometheus_metrics_exporter);
    let client = Client::try_default().await?;
    let namespace_watcher = watcher(Api::<Namespace>::all(client.clone()), ListParams::default());
    let writer: Writer<Namespace> = Default::default();
    let namespace_cache = writer.as_reader();
    let namespace_reflector =
        try_flatten_applied(reflector(writer, namespace_watcher)).try_for_each(ok);
    // ObjectSync controller
    let configuration = Configuration::new(client);
    let controller = ObjectSyncController::new(configuration, namespace_cache).start();
    info!("start controllers ...");
    tokio::select! {
       _ = controller => (),
       _ = namespace_reflector => (),
       _ = prometheus_metrics_exporter => (),
    };
    Ok(())
}
