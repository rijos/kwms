//! Cache key: `(layer, grid directory, z, x, y, format)`.

use crate::crs::Crs;
use crate::encode::OutputFormat;
use crate::tile::TileCoord;

/// The cache directory of a grid: its EPSG code (`28992`, `3857`) when its
/// CRS is `EPSG:<code>`, otherwise the grid's config name. Config
/// validation rejects a layer whose grids would share a directory (two
/// grids in one CRS), so within a layer it identifies the grid.
pub fn grid_dir(grid_name: &str, crs: &Crs) -> String {
    let s = crs.as_str();
    match s.get(..5) {
        Some(prefix) if prefix.eq_ignore_ascii_case("EPSG:") => {
            let code = &s[5..];
            if !code.is_empty() && code.len() <= 9 && code.bytes().all(|b| b.is_ascii_digit()) {
                return code.to_string();
            }
            grid_name.to_string()
        }
        _ => grid_name.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TileKey {
    pub layer: String,
    /// The grid's cache directory, see [`grid_dir`].
    pub grid_dir: String,
    pub coord: TileCoord,
    pub format: OutputFormat,
}

impl TileKey {
    /// Object path in the persistent cache:
    /// `<layer>/<grid dir>/<format>/<z>/<x>/<y>.<ext>`, e.g.
    /// `2026_orthoHR/28992/jpeg/6/30/32.jpeg`.
    ///
    /// Every layer lives under its own top-level prefix, so layers never
    /// share or overwrite entries and one layer can be inspected or cleared
    /// on its own (e.g. delete `2026_orthoHR/` in the container). The same
    /// key serves WMTS, TMS and WMS (GetMap is composed from grid tiles).
    /// Layer and grid names are validated as plain path segments at config load
    /// (`config::validate`), and nothing here depends on the build (no
    /// `DefaultHasher`), so paths stay the same across releases.
    pub fn as_path(&self) -> String {
        let fmt = match self.format {
            OutputFormat::Png => "png",
            OutputFormat::Jpeg => "jpeg",
            OutputFormat::Webp => "webp",
            OutputFormat::GeoTiff => "geotiff",
        };
        format!(
            "{layer}/{grid}/{fmt}/{z}/{x}/{y}.{ext}",
            layer = self.layer,
            grid = self.grid_dir,
            z = self.coord.z,
            x = self.coord.x,
            y = self.coord.y,
            ext = self.format.extension(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(layer: &str, grid_dir: &str) -> TileKey {
        TileKey {
            layer: layer.into(),
            grid_dir: grid_dir.into(),
            coord: TileCoord::new(12, 345, 678),
            format: OutputFormat::Png,
        }
    }

    /// Pinned literally: a change here orphans every persistent cache.
    #[test]
    fn as_path_is_stable() {
        assert_eq!(key("ortho2024", "3857").as_path(), "ortho2024/3857/png/12/345/678.png");
    }

    #[test]
    fn grid_dir_is_the_epsg_code_or_the_grid_name() {
        assert_eq!(grid_dir("rd_new_quad", &Crs::new("EPSG:28992")), "28992");
        assert_eq!(grid_dir("webmercator", &Crs::new("epsg:3857")), "3857");
        for odd in ["ESRI:102100", "EPSG:", "EPSG:12a", "EPSG:1234567890", "PROJCS[...]"] {
            assert_eq!(grid_dir("my_grid", &Crs::new(odd)), "my_grid", "{odd}");
        }
    }

    #[test]
    fn layers_and_grids_get_separate_paths() {
        let a = key("a", "28992").as_path();
        assert_ne!(a, key("b", "28992").as_path());
        assert_ne!(a, key("a", "3857").as_path());
        assert!(a.starts_with("a/"), "{a}");
    }

    #[test]
    fn format_extension_matches_outputformat() {
        let mut k = key("L", "3857");
        k.format = OutputFormat::GeoTiff;
        assert!(k.as_path().ends_with(".tif"), "got: {}", k.as_path());
        assert!(k.as_path().contains("/geotiff/"), "got: {}", k.as_path());
    }
}
