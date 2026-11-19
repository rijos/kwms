//! Tower-http tracing layer — one span per HTTP request, with default
//! request/response/failure callbacks. The fields populated by the layer
//! (method, uri, status, latency) flow into the structured-log JSON
//! sink configured in [`crate::subcommand::serve`].

use tower_http::classify::{ServerErrorsAsFailures, SharedClassifier};
use tower_http::trace::{DefaultMakeSpan, DefaultOnFailure, DefaultOnRequest, DefaultOnResponse, TraceLayer};

/// Build the tracing layer that wraps the whole router.
///
/// Returning the concrete type (rather than `impl Layer<S>`) keeps the
/// type signature inspectable from `mod.rs`, which makes it easier to
/// stack additional middleware later without fighting opaque types.
pub fn layer() -> TraceLayer<
    SharedClassifier<ServerErrorsAsFailures>,
    DefaultMakeSpan,
    DefaultOnRequest,
    DefaultOnResponse,
    (),
    (),
    DefaultOnFailure,
> {
    // `TraceLayer::new_for_http` builds a layer whose span includes the
    // HTTP method/URI and whose default callbacks log requests, responses,
    // and failures at sensible levels. We strip the body-chunk and EOS
    // callbacks (they're noisy and we don't need them for tile traffic).
    TraceLayer::new_for_http()
        .make_span_with(DefaultMakeSpan::new())
        .on_request(DefaultOnRequest::new())
        .on_response(DefaultOnResponse::new())
        .on_body_chunk(())
        .on_eos(())
        .on_failure(DefaultOnFailure::new())
}
