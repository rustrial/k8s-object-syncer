use std::sync::Arc;

use hyper::{
    header::CONTENT_TYPE,
    service::{make_service_fn, service_fn},
    Body, Request, Response, Server,
};
use opentelemetry_prometheus::PrometheusExporter;
use prometheus::{Encoder, TextEncoder};

async fn serve_req(
    _req: Request<Body>,
    prometheus_metrics_exporter: Arc<PrometheusExporter>,
) -> Result<Response<Body>, hyper::http::Error> {
    let encoder = TextEncoder::new();
    let metric_families = prometheus_metrics_exporter.registry().gather();
    let mut result = Vec::new();
    let x = match encoder.encode(&metric_families, &mut result) {
        Ok(_) => Response::builder()
            .status(200)
            .header(CONTENT_TYPE, encoder.format_type())
            .body(Body::from(result)),
        Err(e) => {
            error!("{}", e);
            Response::builder().status(500).body(Body::empty())
        }
    };
    x
}

pub(crate) async fn start_prometheus_metrics_server(
    prometheus_metrics_exporter: PrometheusExporter,
) {
    let addr = ([127, 0, 0, 1], 9000).into();
    debug!("Listening on http://{}", addr);
    let exporter = Arc::new(prometheus_metrics_exporter);
    let handler = make_service_fn(move |_| {
        let exporter = exporter.clone();
        async move {
            Ok::<_, hyper::http::Error>(service_fn(move |req| serve_req(req, exporter.clone())))
        }
    });
    let serve_future = Server::bind(&addr).serve(handler);
    if let Err(err) = serve_future.await {
        panic!("metrics server error: {}", err);
    }
}
