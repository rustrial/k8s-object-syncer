use http_body_util::Full;
use hyper::{
    body::{Bytes, Incoming},
    header::CONTENT_TYPE,
    server::conn::http1,
    service::service_fn,
    Request, Response,
};
use hyper_util::rt::TokioIo;
use prometheus::{Encoder, Registry, TextEncoder};
use std::sync::Arc;
use tokio::net::TcpListener;

async fn serve_req(
    _req: Request<Incoming>,
    prometheus_metrics_exporter: Arc<Registry>,
) -> Result<Response<Full<Bytes>>, hyper::http::Error> {
    let encoder = TextEncoder::new();
    let metric_families = prometheus_metrics_exporter.gather();
    let mut result = Vec::new();
    let x = match encoder.encode(&metric_families, &mut result) {
        Ok(_) => Response::builder()
            .status(200)
            .header(CONTENT_TYPE, encoder.format_type())
            .body(Full::new(Bytes::from(result))),
        Err(e) => {
            error!("{}", e);
            Response::builder()
                .status(500)
                .body(Full::new(Bytes::new()))
        }
    };
    x
}

pub(crate) async fn start_prometheus_metrics_server(
    listener: TcpListener,
    prometheus_metrics_exporter: Registry,
) -> anyhow::Result<()> {
    let exporter = Arc::new(prometheus_metrics_exporter);
    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let exporter = exporter.clone();
        tokio::task::spawn(async move {
            if let Err(err) = http1::Builder::new()
                .serve_connection(io, service_fn(|req| serve_req(req, exporter.clone())))
                .await
            {
                eprintln!("Error serving connection: {:?}", err);
            }
        });
    }
}
