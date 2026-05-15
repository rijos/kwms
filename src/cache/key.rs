//! Cache key: `(layer, z, x, y, format, crs)`.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::crs::Crs;
use crate::encode::OutputFormat;
use crate::tile::TileCoord;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TileKey {
    pub layer: String,
    pub coord: TileCoord,
    pub format: OutputFormat,
    pub crs: Crs,
}

impl TileKey {
    /// Serialize as a forward-slash path suitable for OpenDAL.
    ///
    /// Layout: `<layer>/<crs-hash>/<format>/<z>/<x>/<y>.<ext>`.
    ///
    /// The CRS is reduced to a short hex hash because CRS strings can contain
    /// `:` and other characters that are awkward in object-store paths. The
    /// hash is taken with `std::collections::hash_map::DefaultHasher`, which
    /// uses fixed SipHash keys (0, 0) when constructed via `new`, so it is
    /// deterministic within a binary.
    pub fn as_path(&self) -> String {
        let mut hasher = DefaultHasher::new();
        self.crs.as_str().hash(&mut hasher);
        let crs_hash = format!("{:016x}", hasher.finish());
        // Use the first 16 hex chars (full u64) — already short and stable.
        let crs_short = &crs_hash[..16];

        let ext = self.format.extension();
        let fmt = match self.format {
            OutputFormat::Png => "png",
            OutputFormat::Jpeg => "jpeg",
            OutputFormat::Webp => "webp",
            OutputFormat::GeoTiff => "geotiff",
        };

        format!(
            "{layer}/{crs}/{fmt}/{z}/{x}/{y}.{ext}",
            layer = self.layer,
            crs = crs_short,
            fmt = fmt,
            z = self.coord.z,
            x = self.coord.x,
            y = self.coord.y,
            ext = ext,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_key() -> TileKey {
        TileKey {
            layer: "ortho2024".into(),
            coord: TileCoord::new(12, 345, 678),
            format: OutputFormat::Png,
            crs: Crs::new("EPSG:3857"),
        }
    }

    #[test]
    fn as_path_is_stable_for_known_key() {
        let key = sample_key();
        // Compute the expected CRS hash with the same hasher to keep the
        // assertion deterministic even if the SipHash output ever changes —
        // we still assert path structure explicitly.
        let mut hasher = DefaultHasher::new();
        "EPSG:3857".hash(&mut hasher);
        let crs = format!("{:016x}", hasher.finish());
        let expected = format!("ortho2024/{crs}/png/12/345/678.png");
        assert_eq!(key.as_path(), expected);
    }

    #[test]
    fn as_path_round_trips_layout() {
        let key = sample_key();
        let path = key.as_path();
        let parts: Vec<_> = path.split('/').collect();
        assert_eq!(parts.len(), 6, "expected 6 segments, got {path}");
        assert_eq!(parts[0], "ortho2024");
        assert_eq!(parts[2], "png");
        assert_eq!(parts[3], "12");
        assert_eq!(parts[4], "345");
        assert_eq!(parts[5], "678.png");
    }

    #[test]
    fn different_crs_yields_different_path() {
        let a = TileKey {
            layer: "L".into(),
            coord: TileCoord::new(0, 0, 0),
            format: OutputFormat::Png,
            crs: Crs::new("EPSG:3857"),
        };
        let b = TileKey {
            layer: "L".into(),
            coord: TileCoord::new(0, 0, 0),
            format: OutputFormat::Png,
            crs: Crs::new("EPSG:4326"),
        };
        assert_ne!(a.as_path(), b.as_path());
    }

    #[test]
    fn format_extension_matches_outputformat() {
        let mut k = sample_key();
        k.format = OutputFormat::GeoTiff;
        assert!(k.as_path().ends_with(".tif"), "got: {}", k.as_path());
        assert!(k.as_path().contains("/geotiff/"), "got: {}", k.as_path());
    }
}
