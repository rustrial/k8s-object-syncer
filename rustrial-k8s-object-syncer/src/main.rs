#[macro_use]
extern crate log;

use anyhow::{Context, anyhow};
use futures::TryStreamExt;
use k8s_openapi::api::core::v1::Namespace;
use kube::{Api, Client};
use kube_runtime::{
    WatchStreamExt,
    reflector::{reflector, store::Writer},
    watcher::{self},
};
use opentelemetry::global;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use prometheus_exporter::start_prometheus_metrics_server;
use rustls::crypto::ring::default_provider;
use rustrial_k8s_object_syncer_apis::ObjectSync;
use std::{collections::HashSet, net::SocketAddr};
use tokio::net::TcpListener;

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
        fn normalize(hs: HashSet<String>) -> Option<HashSet<String>> {
            if hs.is_empty() || hs.contains("*") || hs.contains("") {
                None
            } else {
                Some(hs)
            }
        }
        let watch_namespaces: Option<HashSet<String>> = env_var("WATCH_NAMESPACES")
            .map(|v| normalize(v.split(",").map(|v| v.to_string()).collect()))
            .flatten();
        let source_namespaces: Option<HashSet<String>> = env_var("SOURCE_NAMESPACES")
            .map(|v| normalize(v.split(",").map(|v| v.to_string()).collect()))
            .flatten();
        let target_namespaces: Option<HashSet<String>> = env_var("TARGET_NAMESPACES")
            .map(|v| normalize(v.split(",").map(|v| v.to_string()).collect()))
            .flatten();
        let mut tmp = watch_namespaces.iter().flatten();
        let object_sync_api = if let (Some(ns), None) = (tmp.next(), tmp.next()) {
            // Optimize for the use-case where exactly one watch-namespace is provided.
            info!("Controller is only watching resources in namespace {}", ns);
            Api::<ObjectSync>::namespaced(client.clone(), ns.as_str())
        } else {
            if let Some(namespaces) = &watch_namespaces {
                let namespaces: Vec<&str> = namespaces.iter().map(|v| v.as_str()).collect();
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
            watch_namespaces,
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
    rustls::crypto::CryptoProvider::install_default(default_provider())
        .map_err(|e| anyhow!("failed to set crypto provider: {:?}", e))?;
    let metrics_addr = env_var("METRICS_LISTEN_ADDR").unwrap_or_else(|| "0.0.0.0".to_string());
    let metrics_port = env_var("METRICS_LISTEN_PORT").unwrap_or_else(|| "9000".to_string());
    let metrics_addr: SocketAddr = format!("{}:{}", metrics_addr, metrics_port).parse()?;
    let registry = prometheus::Registry::new();
    let prometheus_metrics_exporter = opentelemetry_prometheus::exporter()
        .with_registry(registry.clone())
        .build()?;

    let provider = SdkMeterProvider::builder()
        .with_reader(prometheus_metrics_exporter)
        .build();
    global::set_meter_provider(provider);

    let listener = TcpListener::bind(metrics_addr.clone())
        .await
        .context(format!(
            "Failed to bind metrics endpoint to http://{}",
            metrics_addr
        ))?;
    debug!("Listening on http://{}", metrics_addr);
    let prometheus_metrics_exporter = start_prometheus_metrics_server(listener, registry);
    let client = Client::try_default().await?;
    let namespace_watcher = watcher::watcher(
        Api::<Namespace>::all(client.clone()),
        watcher::Config::default(),
    );
    let writer: Writer<Namespace> = Default::default();
    let namespace_cache = writer.as_reader();
    let namespace_reflector = reflector(writer, namespace_watcher)
        .applied_objects()
        .try_for_each(ok);
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
