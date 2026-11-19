//! Image pyramid — a grid + a max level resolved from `max_resolution`.

use std::sync::Arc;

use crate::crs::Bbox;

use super::TileGrid;

pub struct Pyramid {
    pub grid: Arc<dyn TileGrid>,
    pub max_level: u32,
}

/// The resolution a (bbox, output size) request asks for: the coarser of
/// the x and y CRS-units-per-pixel.
pub fn requested_resolution(bbox: &Bbox, size: (u32, u32)) -> f64 {
    let width_px = f64::from(size.0.max(1));
    let height_px = f64::from(size.1.max(1));
    ((bbox.maxx - bbox.minx).abs() / width_px).max((bbox.maxy - bbox.miny).abs() / height_px)
}

impl Pyramid {
    /// Select the pyramid level whose resolution is the coarsest that still
    /// covers the requested (bbox, output size) without oversampling.
    ///
    /// The requested resolution is the coarser of `bbox.width() / size.0`
    /// and `bbox.height() / size.1`, in the grid's CRS units per pixel.
    /// Using the coarser axis bounds the number of covering tiles by the
    /// output size on *both* axes (a narrow but very tall bbox can't force a
    /// deep level). The deepest level `k` whose
    /// `grid.resolution(k) >= requested_res` is returned, capped by
    /// `self.max_level`. Returns `0` if even level 0 oversamples (i.e.
    /// the request is coarser than what level 0 can deliver).
    pub fn select_level(&self, bbox: &Bbox, size: (u32, u32)) -> u32 {
        let requested = requested_resolution(bbox, size);

        // Walk from level 0 upwards while the level's resolution is still
        // coarser-or-equal to the requested resolution — that's the deepest
        // level that does NOT oversample.
        let cap = self.max_level.min(self.grid.level_count().saturating_sub(1));
        let mut best: u32 = 0;
        for level in 0..=cap {
            if self.grid.resolution(level) >= requested {
                best = level;
            } else {
                break;
            }
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tile::grid::WebMercator;

    #[test]
    fn select_level_amsterdam_111km_square() {
        let grid: Arc<dyn TileGrid> = Arc::new(WebMercator::new());
        let pyramid = Pyramid { grid: grid.clone(), max_level: 20 };

        // ~1° × 1° area near Amsterdam, in EPSG:3857. ~111 km wide.
        // (The corners don't need to be geographically exact for this test —
        // the level pick only depends on bbox width.)
        let bbox = Bbox {
            minx: 545_000.0,
            miny: 6_811_000.0,
            maxx: 656_000.0, // ~111 km wide
            maxy: 6_922_000.0,
        };
        let level = pyramid.select_level(&bbox, (256, 256));
        let res = grid.resolution(level);
        let requested = (bbox.maxx - bbox.minx) / 256.0;

        // Plausible range for 111 km / 256 px ≈ 433 m/px → between L8 and L10.
        assert!(
            (8..=10).contains(&level),
            "expected level in 8..=10 for Amsterdam-ish 1°×1° square, got {level}"
        );
        // Must not oversample.
        assert!(
            res >= requested,
            "level {level} oversamples: grid res {res} < requested {requested}"
        );
        // And the next level *would* oversample (we picked the deepest).
        if level < pyramid.max_level {
            assert!(
                grid.resolution(level + 1) < requested,
                "level {} is not the deepest non-oversampling level (next still fits)",
                level
            );
        }
    }

    #[test]
    fn select_level_world_picks_zero() {
        let grid: Arc<dyn TileGrid> = Arc::new(WebMercator::new());
        let pyramid = Pyramid { grid, max_level: 20 };
        let bbox = Bbox {
            minx: -20037508.34,
            miny: -20037508.34,
            maxx: 20037508.34,
            maxy: 20037508.34,
        };
        // Whole-world bbox at 256 px requests ~156543 m/px — exactly level 0.
        assert_eq!(pyramid.select_level(&bbox, (256, 256)), 0);
    }

    #[test]
    fn select_level_oversample_floors_to_zero() {
        let grid: Arc<dyn TileGrid> = Arc::new(WebMercator::new());
        let pyramid = Pyramid { grid, max_level: 20 };
        // Way larger than the world → request is coarser than level 0.
        let bbox = Bbox {
            minx: -1e9,
            miny: -1e9,
            maxx: 1e9,
            maxy: 1e9,
        };
        assert_eq!(pyramid.select_level(&bbox, (256, 256)), 0);
    }

    #[test]
    fn select_level_capped_by_max_level() {
        let grid: Arc<dyn TileGrid> = Arc::new(WebMercator::new());
        let pyramid = Pyramid { grid, max_level: 3 };
        // 1 m/px request would naturally pick a very deep level; cap at 3.
        let bbox = Bbox { minx: 0.0, miny: 0.0, maxx: 256.0, maxy: 256.0 };
        assert_eq!(pyramid.select_level(&bbox, (256, 256)), 3);
    }
}
