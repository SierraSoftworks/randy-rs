use std::{pin::Pin, task::{Context, Poll}};

use actix_web::{Error, http::HeaderMap};
use actix_service::*;
use actix_web::dev::*;
use futures::{Future, future::{ok, Ready}};
use opentelemetry::{propagation::{Extractor, TextMapPropagator}, sdk::propagation::TraceContextPropagator};
use tracing::{Instrument, Span};
use tracing_opentelemetry::OpenTelemetrySpanExt;

pub struct TracingLogger;

impl<S, B> Transform<S> for TracingLogger
where
    S: Service<Request = ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: 'static,
{
    type Request = ServiceRequest;
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Transform = TracingLoggerMiddleware<S>;
    type InitError = ();
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ok(TracingLoggerMiddleware { service })
    }
}

#[doc(hidden)]
pub struct TracingLoggerMiddleware<S> {
    service: S,
}

impl<S, B> Service for TracingLoggerMiddleware<S>
where
    S: Service<Request = ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: 'static,
{
    type Request = ServiceRequest;
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, req: ServiceRequest) -> Self::Future {
        let propagator = TraceContextPropagator::new();


        let user_agent = req
            .headers()
            .get("User-Agent")
            .map(|h| h.to_str().unwrap_or(""))
            .unwrap_or("");

            

        let span = tracing::info_span!(
            "request",
            "otel.kind" = "server",
            "net.transport" = "IP.TPC",
            "net.peer.ip" = %req.connection_info().realip_remote_addr().unwrap_or(""),
            "http.target" = %req.path(),
            "http.user_agent" = %user_agent,
            "http.status_code" = tracing::field::Empty,
            "http.method" = %req.method(),
            "http.url" = %req.uri(),
        );

        // Propagate OpenTelemetry parent span context information
        let context  = propagator.extract(&HeaderMapExtractor { headers: req.headers() });
        span.set_parent(context);

        let fut = self.service.call(req);
        Box::pin(
            async move {
                let outcome = fut.await;
                let status_code = match &outcome {
                    Ok(response) => response.response().status(),
                    Err(error) => error.as_response_error().status_code(),
                };
                Span::current().record("http.status_code", &status_code.as_u16());
                outcome
            }
            .instrument(span),
        )
    }
}

struct HeaderMapExtractor<'a> {
    headers: &'a HeaderMap
}

impl<'a> Extractor for HeaderMapExtractor<'a> {
    fn get(&self, key: &str) -> Option<&'a str> {
        self.headers.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.headers.keys().map(|v| v.as_str()).collect()
    }
}