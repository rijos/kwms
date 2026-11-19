//! Error classification for request handlers.
//!
//! Pipeline functions return `anyhow::Error`. Handlers decide what the client
//! sees with [`classify`]:
//!
//! * [`ClientError`] — the request itself is wrong (unknown layer, size over
//!   the limit, tile out of range, …). The message is written for the client
//!   and is safe to return verbatim.
//! * [`Busy`] — the render queue is full; respond 429 so clients back off.
//! * anything else — an internal failure (GDAL, source I/O, cache backend).
//!   Its message can contain file paths, source URIs or backend details, so
//!   handlers log it and return only a generic message plus the request id.

use std::fmt;

/// A request error the client can fix. The message is a `&'static str`:
/// a literal from this crate, never built from request data.
#[derive(Debug)]
pub struct ClientError(pub &'static str);

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for ClientError {}

/// Shorthand: `Err(client_error("..."))?`.
pub fn client_error(msg: &'static str) -> anyhow::Error {
    ClientError(msg).into()
}

/// The render pool stayed saturated for longer than the queue timeout.
#[derive(Debug)]
pub struct Busy;

impl fmt::Display for Busy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("server is busy, retry later")
    }
}

impl std::error::Error for Busy {}

/// How a handler should report an error.
#[derive(Debug, PartialEq, Eq)]
pub enum Disposition {
    /// Return this (static) message to the client.
    Client(&'static str),
    /// Return 429 (capacity, not failure).
    Busy,
    /// Log the error, return a generic message.
    Internal,
}

/// Classify an error by looking for [`ClientError`] / [`Busy`] anywhere in
/// its cause chain (so `.context(...)` wrapping doesn't hide them).
pub fn classify(err: &anyhow::Error) -> Disposition {
    for cause in err.chain() {
        if let Some(c) = cause.downcast_ref::<ClientError>() {
            return Disposition::Client(c.0);
        }
        if cause.downcast_ref::<Busy>().is_some() {
            return Disposition::Busy;
        }
    }
    Disposition::Internal
}

#[cfg(test)]
mod tests {
    use anyhow::Context;

    use super::*;

    #[test]
    fn classify_sees_through_context() {
        let e = Err::<(), _>(client_error("bad layer")).context("outer").unwrap_err();
        assert_eq!(classify(&e), Disposition::Client("bad layer"));

        let e = Err::<(), _>(anyhow::Error::from(Busy)).context("outer").unwrap_err();
        assert_eq!(classify(&e), Disposition::Busy);

        let e = anyhow::anyhow!("failed to open /secret/path.tif");
        assert_eq!(classify(&e), Disposition::Internal);
    }
}
