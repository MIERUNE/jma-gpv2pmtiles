use anyhow::Result;
use fast_mvt::{MvtGeometry, MvtLayerBuilder, MvtMultiPolygon};
use foldhash::HashMap;
use rayon::prelude::*;

use crate::{
    geo::lat_to_web_mercator_y,
    model::{CompactOptI32, TilesetSpec},
    tile::{
        BandScale, Point, PointMap, TileBounds,
        rect_union::{IntRect, union_rectangles},
    },
};

pub(super) fn render_mvt_layer(
    layer_name: &str,
    tileset_spec: &TilesetSpec,
    extent: i32,
    tile_bounds: &TileBounds,
    deduped_points: &PointMap<(u32, u32), Point>,
    band_scales: &Vec<BandScale<'_>>,
) -> Result<Option<Vec<u8>>> {
    let grouped_rectangles = make_rectangles_grouped_by_value(
        deduped_points,
        &tileset_spec.grid_spec,
        tile_bounds,
        extent,
    );
    if grouped_rectangles.is_empty() {
        return Ok(None);
    }

    // Encode geometries
    let value_geom = grouped_rectangles
        .into_par_iter()
        .map(|(values, rectangles)| {
            Ok((
                values,
                MvtGeometry::MultiPolygon(MvtMultiPolygon::new(union_rectangles(rectangles)?)),
            ))
        })
        .collect::<Result<Vec<_>>>()?;

    // The groups came out of a hash map, whose order is seeded afresh on every
    // run, so the features landed in the tile in a different order each time.
    // The tiles were equivalent but never byte-identical, which rules out
    // caching, delta distribution and signing. Ordering by the values makes the
    // output a function of the input alone.
    let mut value_geom = value_geom;
    value_geom.sort_unstable_by_key(|(values, _)| *values);

    // Encode features
    let mut layer = MvtLayerBuilder::with_capacity(layer_name, value_geom.len())?;
    layer.extent(std::num::NonZeroU32::new(extent as u32).expect("MVT extent is positive"));
    for (values, geometry) in value_geom {
        let mut feature = layer.feature(&geometry)?;
        for (band, raw_value) in band_scales.iter().zip(values) {
            let Some(value) = raw_value.get() else {
                continue;
            };
            feature.tag(band.name, band.tag_value(value))?;
        }

        // An id is only set when it can be made unique. Casting a negative
        // first band straight to `u64` sign-extended it, filling the high half
        // with ones and swallowing whatever the second band was OR-ed into; the
        // three- and four-band forms overlapped their bit ranges outright, so
        // features with different values shared an id. Two bands are the most
        // that fit exactly, and beyond that no id is better than a colliding
        // one - the specification asks for uniqueness, and consumers use it to
        // carry per-feature state.
        if let Some(feature_id) = unique_feature_id(&values[..tileset_spec.band_specs.len()]) {
            feature.id(Some(feature_id));
        }
        layer = feature.end();
    }

    Ok(Some(layer.encode()))
}

/// A feature id that no other value combination in the layer can share.
///
/// Each band is a whole `i32`, so only one or two of them fit in the 64 bits an
/// id has. A band without a value has no code left to stand for it, so those
/// features go without an id rather than share one.
fn unique_feature_id(values: &[CompactOptI32]) -> Option<u64> {
    let present = |value: &CompactOptI32| value.get().map(|value| u64::from(value as u32));
    match values {
        [first] => present(first),
        [first, second] => Some((present(first)? << 32) | present(second)?),
        _ => None,
    }
}

/// Creates polygons from data points
fn make_rectangles_grouped_by_value(
    deduped_points: &PointMap<(u32, u32), Point>,
    grid_spec: &gpv_products::model::LngLatGrid,
    tile_bounds: &TileBounds,
    extent: i32,
) -> HashMap<[CompactOptI32; 4], Vec<IntRect>> {
    let mut grouped_rectangles: HashMap<[CompactOptI32; 4], Vec<IntRect>> = HashMap::default();
    let buffer_pixels = 2;
    let buffer = buffer_pixels * extent / 256;
    let tile_width = tile_bounds.mx2 - tile_bounds.mx1;
    let w = (extent as f64) / tile_width;
    let mut projected_y = HashMap::<(u32, u32), (f64, f64)>::default();

    for ((x, y), point) in deduped_points {
        let value = point.values;
        let width = 1 << point.power;
        let lng1 = grid_spec.lng_0 + (*x as f64 - 0.5) / grid_spec.lng_denom as f64;
        let lng2 = grid_spec.lng_0 + ((*x + width) as f64 - 0.5) / grid_spec.lng_denom as f64;
        let mx1 = (lng1 + 180.0) / 360.0;
        let mx2 = (lng2 + 180.0) / 360.0;
        let (my1, my2) = *projected_y.entry((*y, width)).or_insert_with(|| {
            let lat2 = grid_spec.lat_0 + (*y as f64 - 0.5) / grid_spec.lat_denom as f64;
            let lat1 = grid_spec.lat_0 + ((*y + width) as f64 - 0.5) / grid_spec.lat_denom as f64;
            (lat_to_web_mercator_y(lat1), lat_to_web_mercator_y(lat2))
        });
        let (mx1, mx2) = if mx2 > 1. {
            (mx1 - 1., mx2 - 1.)
        } else {
            (mx1, mx2)
        };

        let tx1 =
            (((mx1 - tile_bounds.mx1) * w + 0.5).floor() as i32).clamp(-buffer, extent + buffer);
        let tx2 =
            (((mx2 - tile_bounds.mx1) * w + 0.5).floor() as i32).clamp(-buffer, extent + buffer);
        let ty1 =
            (((my1 - tile_bounds.my1) * w + 0.5).floor() as i32).clamp(-buffer, extent + buffer);
        let ty2 =
            (((my2 - tile_bounds.my1) * w + 0.5).floor() as i32).clamp(-buffer, extent + buffer);

        if ty1 < ty2 {
            if tx1 < tx2 {
                grouped_rectangles
                    .entry(value)
                    .or_default()
                    .push(IntRect::new(tx1, ty1, tx2, ty2));
            }
            // If wrap around anti-meridian
            // TODO: optimization?
            if mx1 < 0. && mx2 > 0. {
                let tx1 = (((mx1 + 1. - tile_bounds.mx1) / tile_width * (extent as f64) + 0.5)
                    .floor() as i32)
                    .clamp(-buffer, extent + buffer);
                let tx2 = (((mx2 + 1. - tile_bounds.mx1) / tile_width * (extent as f64) + 0.5)
                    .floor() as i32)
                    .clamp(-buffer, extent + buffer);
                if tx1 < tx2 {
                    grouped_rectangles
                        .entry(value)
                        .or_default()
                        .push(IntRect::new(tx1, ty1, tx2, ty2));
                }
            }
        }
    }

    grouped_rectangles
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(value: i32) -> CompactOptI32 {
        CompactOptI32::new(Some(value))
    }

    /// A negative first band used to sign-extend into the high half of the id,
    /// which both swallowed the second band and collided with every other
    /// negative value that did the same.
    #[test]
    fn a_negative_band_does_not_swallow_the_one_beside_it() {
        let left = unique_feature_id(&[value(-1), value(7)]);
        let right = unique_feature_id(&[value(-1), value(9)]);

        assert_ne!(left, right, "the second band has to reach the id");
        assert_eq!(left, Some(0xFFFF_FFFF_0000_0007));
    }

    #[test]
    fn distinct_value_pairs_get_distinct_ids() {
        let pairs = [
            [value(0), value(0)],
            [value(0), value(1)],
            [value(1), value(0)],
            [value(-1), value(0)],
            [value(0), value(-1)],
            // i32::MIN is reserved for "missing", so the extremes a band can
            // actually carry are one in from it.
            [value(i32::MIN + 1), value(i32::MAX)],
            [value(i32::MAX), value(i32::MIN + 1)],
        ];
        let ids = pairs
            .iter()
            .map(|pair| unique_feature_id(pair))
            .collect::<Vec<_>>();

        for (index, id) in ids.iter().enumerate() {
            assert!(id.is_some());
            assert!(
                !ids[index + 1..].contains(id),
                "{:?} shares an id with a later pair",
                pairs[index]
            );
        }
    }

    /// Three bands are 96 bits, which cannot be squeezed into 64 without two
    /// combinations meeting. Going without an id is the honest outcome.
    #[test]
    fn more_bands_than_fit_get_no_id() {
        assert_eq!(unique_feature_id(&[value(1), value(2), value(3)]), None);
        assert_eq!(
            unique_feature_id(&[value(1), value(2), value(3), value(4)]),
            None
        );
    }

    /// A missing band has no code of its own, so the feature cannot be told
    /// apart from one that happens to carry that code.
    #[test]
    fn a_missing_band_leaves_the_feature_without_an_id() {
        assert_eq!(unique_feature_id(&[CompactOptI32::NONE]), None);
        assert_eq!(unique_feature_id(&[value(1), CompactOptI32::NONE]), None);
    }
}
