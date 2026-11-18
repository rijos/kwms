//! Route handlers.

pub mod health;
pub mod metrics;
pub mod tms;
pub mod wms;
pub mod wmts;

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;

use crate::pipeline::error::{classify, Disposition};
use crate::server::middleware::request_id::RequestId;

/// What a protocol handler should send back for a failed operation.
pub(crate) struct ErrorReply {
    pub status: StatusCode,
    /// OGC exception code.
    pub code: &'static str,
    /// Client-safe message.
    pub message: String,
}

/// Map a pipeline error to a client-safe reply. Client errors keep their
/// message; internal errors are logged in full (with the request id) and
/// replaced by a generic message, because they can contain file paths,
/// source URIs, or backend details.
pub(crate) fn error_reply(err: &anyhow::Error, operation: &str, rid: &RequestId) -> ErrorReply {
    match classify(err) {
        Disposition::Client(message) => ErrorReply {
            status: StatusCode::BAD_REQUEST,
            code: "InvalidParameterValue",
            message: message.to_string(),
        },
        // 429, not 503: a full render queue is a capacity signal the client
        // should back off from, not a server failure (we alert on 5xx).
        Disposition::Busy => ErrorReply {
            status: StatusCode::TOO_MANY_REQUESTS,
            code: "NoApplicableCode",
            message: "server is busy, retry later".into(),
        },
        Disposition::Internal => {
            tracing::error!(
                request_id = rid.as_str(),
                upstream_request_id = rid.upstream(),
                operation,
                error = %format_args!("{err:#}"),
                "request failed"
            );
            ErrorReply {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: "NoApplicableCode",
                message: format!("{operation} failed (request id {})", rid.as_str()),
            }
        }
    }
}

/// Add `Retry-After` to busy (429) replies so clients know when to retry.
pub(crate) fn with_retry_after(status: StatusCode, mut resp: Response) -> Response {
    if status == StatusCode::TOO_MANY_REQUESTS {
        resp.headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::error::Busy;

    #[test]
    fn busy_maps_to_429_not_5xx() {
        let rid = RequestId::for_tests("r1");
        let reply = error_reply(&anyhow::Error::from(Busy), "GetMap", &rid);
        assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
        let resp = with_retry_after(reply.status, Response::new(Default::default()));
        assert_eq!(resp.headers()[header::RETRY_AFTER], "1");
    }
}
