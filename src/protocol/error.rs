//! `OgcError` — a request error in OGC terms: an exception code, the
//! offending parameter (the OGC "locator"), and a message for the client.
//!
//! **No client input ever appears in an `OgcError`.** This is enforced by
//! the types, not by convention:
//!
//! * `message` is a `&'static str` — it can only be a literal written in
//!   this crate, never a string built from request data;
//! * `locator` is a [`Param`] — one of the parameter names this server
//!   defines, never a key the client chose.
//!
//! So an exception document can't reflect hostile input (markup, control
//! characters, bidi tricks, huge values), whatever the client sends.

use std::fmt;

/// OGC exception codes used by kwms. Names follow OWS Common / WMS 1.3.0 /
/// WMTS 1.0.0; [`OgcCode::as_str`] gives the wire spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OgcCode {
    /// A required parameter is absent.
    MissingParameterValue,
    /// A parameter is present but malformed or out of range.
    InvalidParameterValue,
    /// The requested CRS is not offered for the layer (`InvalidSRS` in WMS 1.1.1).
    InvalidCrs,
    /// The requested layer does not exist.
    LayerNotDefined,
    /// The requested style does not exist.
    StyleNotDefined,
    /// The requested output format is not offered.
    InvalidFormat,
    /// WMTS: TileMatrix/TileRow/TileCol outside the TileMatrixSet.
    TileOutOfRange,
    /// The operation is known but not implemented.
    OperationNotSupported,
}

impl OgcCode {
    pub fn as_str(self) -> &'static str {
        match self {
            OgcCode::MissingParameterValue => "MissingParameterValue",
            OgcCode::InvalidParameterValue => "InvalidParameterValue",
            OgcCode::InvalidCrs => "InvalidCRS",
            OgcCode::LayerNotDefined => "LayerNotDefined",
            OgcCode::StyleNotDefined => "StyleNotDefined",
            OgcCode::InvalidFormat => "InvalidFormat",
            OgcCode::TileOutOfRange => "TileOutOfRange",
            OgcCode::OperationNotSupported => "OperationNotSupported",
        }
    }
}

/// Every request parameter kwms knows, across WMS, WMTS and TMS. Used both
/// to look parameters up in a [`super::kvp::Kvp`] and as error locators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Param {
    Service,
    Request,
    Version,
    Layers,
    Layer,
    Styles,
    Style,
    Crs,
    Srs,
    Bbox,
    Width,
    Height,
    Format,
    Transparent,
    BgColor,
    TileMatrixSet,
    TileMatrix,
    TileRow,
    TileCol,
    InfoFormat,
    /// A REST/TMS URL path as a whole (wrong shape).
    Path,
    /// TMS zoom level (`{z}`).
    Z,
    /// TMS column (`{x}`).
    X,
    /// TMS row (`{y}`).
    Y,
}

impl Param {
    const ALL: [Param; 24] = [
        Param::Service,
        Param::Request,
        Param::Version,
        Param::Layers,
        Param::Layer,
        Param::Styles,
        Param::Style,
        Param::Crs,
        Param::Srs,
        Param::Bbox,
        Param::Width,
        Param::Height,
        Param::Format,
        Param::Transparent,
        Param::BgColor,
        Param::TileMatrixSet,
        Param::TileMatrix,
        Param::TileRow,
        Param::TileCol,
        Param::InfoFormat,
        Param::Path,
        Param::Z,
        Param::X,
        Param::Y,
    ];

    /// The name as written in requests and locators (`TILEMATRIXSET`).
    pub fn name(self) -> &'static str {
        match self {
            Param::Service => "SERVICE",
            Param::Request => "REQUEST",
            Param::Version => "VERSION",
            Param::Layers => "LAYERS",
            Param::Layer => "LAYER",
            Param::Styles => "STYLES",
            Param::Style => "STYLE",
            Param::Crs => "CRS",
            Param::Srs => "SRS",
            Param::Bbox => "BBOX",
            Param::Width => "WIDTH",
            Param::Height => "HEIGHT",
            Param::Format => "FORMAT",
            Param::Transparent => "TRANSPARENT",
            Param::BgColor => "BGCOLOR",
            Param::TileMatrixSet => "TILEMATRIXSET",
            Param::TileMatrix => "TILEMATRIX",
            Param::TileRow => "TILEROW",
            Param::TileCol => "TILECOL",
            Param::InfoFormat => "INFOFORMAT",
            Param::Path => "PATH",
            Param::Z => "Z",
            Param::X => "X",
            Param::Y => "Y",
        }
    }

    /// The known parameter a decoded, lower-cased query key names, if any.
    pub fn from_key(lowercase_key: &str) -> Option<Param> {
        Param::ALL
            .into_iter()
            .find(|p| p.name().eq_ignore_ascii_case(lowercase_key))
    }
}

/// A client-correctable request error. See the module docs: it carries no
/// client input by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OgcError {
    pub code: OgcCode,
    pub locator: Option<Param>,
    pub message: &'static str,
}

impl OgcError {
    pub const fn new(code: OgcCode, locator: Option<Param>, message: &'static str) -> Self {
        Self { code, locator, message }
    }

    /// `MissingParameterValue` for `param`.
    pub const fn missing(param: Param) -> Self {
        Self::new(
            OgcCode::MissingParameterValue,
            Some(param),
            "a required parameter is missing",
        )
    }

    /// `InvalidParameterValue` for `param`.
    pub const fn invalid(param: Param, message: &'static str) -> Self {
        Self::new(OgcCode::InvalidParameterValue, Some(param), message)
    }

    /// The locator as written on the wire.
    pub fn locator_name(&self) -> Option<&'static str> {
        self.locator.map(Param::name)
    }
}

impl fmt::Display for OgcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message)
    }
}

impl std::error::Error for OgcError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructors_and_locators() {
        let e = OgcError::missing(Param::Bbox);
        assert_eq!(e.code, OgcCode::MissingParameterValue);
        assert_eq!(e.locator_name(), Some("BBOX"));
        assert_eq!(OgcCode::InvalidCrs.as_str(), "InvalidCRS");
    }

    #[test]
    fn from_key_only_knows_our_parameters() {
        assert_eq!(Param::from_key("tilematrixset"), Some(Param::TileMatrixSet));
        assert_eq!(Param::from_key("bbox"), Some(Param::Bbox));
        assert_eq!(Param::from_key("<script>"), None);
        for p in Param::ALL {
            assert_eq!(Param::from_key(&p.name().to_ascii_lowercase()), Some(p));
        }
    }
}
