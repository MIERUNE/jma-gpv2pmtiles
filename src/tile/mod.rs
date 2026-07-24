mod mvt;

use gpv_products::model::Aggregation;
use itertools::Itertools;
use rayon::prelude::*;
use tinymvt::{vector_tile::tile::Layer, webmercator::web_mercator_to_lnglat};

use crate::{
    geo::{LngLat, LngLatBox},
    hilbert::{hilbert_to_xy, xy_to_hilbert},
    model::{CompactOptI32, PreparedProduct},
};

pub(crate) type Zxy = (u8, u32, u32);
type IndexMap<K, V> = indexmap::IndexMap<K, V, foldhash::fast::RandomState>;

#[derive(Debug, Clone, Copy)]
struct TileBounds {
    mx1: f64,
    my1: f64,
    mx2: f64,
    my2: f64,
}

pub(crate) struct TileContext {
    zxy: Zxy,
    bounds: TileBounds,
    geographic_bounds: LngLatBox,
    wrapped_geographic_bounds: LngLatBox,
}

impl TileContext {
    pub(crate) fn new(zxy: Zxy) -> Self {
        let (z, x, y) = zxy;
        let bounds = TileBounds {
            mx1: x as f64 / (1 << z) as f64,
            my1: y as f64 / (1 << z) as f64,
            mx2: (x + 1) as f64 / (1 << z) as f64,
            my2: (y + 1) as f64 / (1 << z) as f64,
        };
        let buffer = (bounds.mx2 - bounds.mx1) * (2.0 / 256.0);
        let (lng1, lat1) = web_mercator_to_lnglat(bounds.mx1 - buffer, bounds.my2 + buffer);
        let (lng2, lat2) = web_mercator_to_lnglat(bounds.mx2 + buffer, bounds.my1 - buffer);

        Self {
            zxy,
            bounds,
            geographic_bounds: LngLatBox::new(LngLat::new(lng1, lat1), LngLat::new(lng2, lat2)),
            wrapped_geographic_bounds: LngLatBox::new(
                LngLat::new(lng1 + 360.0, lat1),
                LngLat::new(lng2 + 360.0, lat2),
            ),
        }
    }
}

#[derive(Default, PartialEq, Eq, Debug, PartialOrd)]
struct Point {
    values: [CompactOptI32; 4],
    counts: [i32; 4],
    power: u8,
}

struct BandScale<'a> {
    name: &'a str,
    scaling: bool,
    reference: f32,
    binary_scale: f32,
    decimal_scale: f64,
}

#[inline]
fn aggregate_band(values: &[CompactOptI32], aggregation: Aggregation) -> (CompactOptI32, i32) {
    match aggregation {
        Aggregation::Max => (CompactOptI32::new(values.iter().flatten().max()), 0),
        Aggregation::Min => (CompactOptI32::new(values.iter().flatten().min()), 0),
        Aggregation::RoughAvg => {
            let mut sum = None;
            let mut count = 0usize;
            for value in values.iter().flatten() {
                sum = Some(match sum {
                    Some(sum) => sum + value,
                    None => value,
                });
                count += 1;
            }
            (CompactOptI32::new(sum), count as i32)
        }
        Aggregation::BitOr => (
            CompactOptI32::new(
                values
                    .iter()
                    .flatten()
                    .fold(None, |acc, value| Some(acc.unwrap_or(0) | value)),
            ),
            0,
        ),
    }
}

fn merge_point(
    points: &mut IndexMap<(u32, u32), Point>,
    key: (u32, u32),
    incoming: Point,
    aggregation: Aggregation,
    band_count: usize,
) {
    use indexmap::map::Entry;

    match points.entry(key) {
        Entry::Vacant(entry) => {
            entry.insert(incoming);
        }
        Entry::Occupied(mut entry) => {
            let point = entry.get_mut();
            match aggregation {
                Aggregation::Max => {
                    for index in 0..band_count {
                        if let Some(value) = incoming.values[index].get() {
                            point.values[index] = CompactOptI32::new(Some(
                                point.values[index].unwrap_or(value).max(value),
                            ));
                        }
                    }
                }
                Aggregation::Min => {
                    for index in 0..band_count {
                        if let Some(value) = incoming.values[index].get() {
                            point.values[index] = CompactOptI32::new(Some(
                                point.values[index].unwrap_or(value).min(value),
                            ));
                        }
                    }
                }
                Aggregation::RoughAvg => {
                    for index in 0..band_count {
                        if let Some(value) = incoming.values[index].get() {
                            point.values[index] =
                                CompactOptI32::new(Some(point.values[index].unwrap_or(0) + value));
                        }
                        point.counts[index] += incoming.counts[index];
                    }
                }
                Aggregation::BitOr => {
                    for index in 0..band_count {
                        if let Some(value) = incoming.values[index].get() {
                            point.values[index] =
                                CompactOptI32::new(Some(point.values[index].unwrap_or(0) | value));
                        }
                    }
                }
            }
            point.power = point.power.max(incoming.power);
        }
    }
}

pub(crate) fn zxy_to_chunk_id_range(base_z: u8, zxy: Zxy) -> (u64, u64) {
    let (z, x, y) = zxy;
    if z < base_z {
        let scale = 1 << (base_z - z);
        let begin = xy_to_hilbert(z, x, y) * scale * scale;
        (begin, begin + scale * scale)
    } else {
        let begin = xy_to_hilbert(base_z, x >> (z - base_z), y >> (z - base_z));
        (begin, begin + 1)
    }
}

pub(crate) fn make_layer(
    tile_context: &TileContext,
    product: &PreparedProduct,
    layer_name: &str,
) -> Option<Layer> {
    let extent = 4096;
    let (z, _, _) = tile_context.zxy;
    let (chunk_begin, chunk_end) = zxy_to_chunk_id_range(product.spec.base_z, tile_context.zxy);
    let chunks = product.chunks_in_range(chunk_begin, chunk_end);
    if chunks.is_empty() {
        return None;
    }

    let maximum_detail_zoom = (product.spec.grid_spec.lat_denom * 360.0 * 2.0 / 512.0)
        .log2()
        .round() as u8;

    let aggregation = product.spec.aggregation;
    let band_count = product.spec.band_specs.len();
    let mut points = chunks
        .par_iter()
        .fold(IndexMap::default, |mut points, (_, tile)| {
            let aggregation_scale = maximum_detail_zoom.saturating_sub(z);
            let aggregation_width = 1 << aggregation_scale;
            let mut previous_end = 0;

            for ((grid_x, grid_y, point_power), group) in tile
                .point_ids
                .iter()
                .copied()
                .zip_eq(tile.point_powers.iter().copied())
                .chunk_by(|&(point_id, point_power)| {
                    let (x, y) = hilbert_to_xy(16, point_id as u64);
                    (
                        x - x % aggregation_width,
                        y - y % aggregation_width,
                        point_power,
                    )
                })
                .into_iter()
            {
                let end = previous_end + group.count();
                let begin = previous_end;
                previous_end = end;
                let power = point_power.max(aggregation_scale);
                let width = 1 << power;

                let grid = &product.spec.grid_spec;
                let lng1 = grid.lng_0 + (grid_x as f64 - 0.5) / grid.lng_denom as f64;
                let lng2 = grid.lng_0 + ((grid_x + width) as f64 - 0.5) / grid.lng_denom as f64;
                let lat2 = grid.lat_0 + (grid_y as f64 - 0.5) / grid.lat_denom as f64;
                let lat1 = grid.lat_0 + ((grid_y + width) as f64 - 0.5) / grid.lat_denom as f64;
                let point_box = LngLatBox::new(LngLat::new(lng1, lat1), LngLat::new(lng2, lat2));
                if !tile_context.geographic_bounds.intersects_box(&point_box)
                    && (lng2 <= 180.0
                        || !tile_context
                            .wrapped_geographic_bounds
                            .intersects_box(&point_box))
                {
                    continue;
                }

                let mut values = [CompactOptI32::NONE; 4];
                let mut counts = [0; 4];
                for (band_index, band) in tile.bands.iter().enumerate() {
                    (values[band_index], counts[band_index]) =
                        aggregate_band(&band.values[begin..end], aggregation);
                }

                merge_point(
                    &mut points,
                    (grid_x, grid_y),
                    Point {
                        values,
                        counts,
                        power,
                    },
                    aggregation,
                    band_count,
                );
            }
            points
        })
        .reduce(IndexMap::default, |mut left, right| {
            for (key, point) in right {
                merge_point(&mut left, key, point, aggregation, band_count);
            }
            left
        });
    if points.is_empty() {
        return None;
    }

    if product.spec.aggregation == Aggregation::RoughAvg {
        for point in points.values_mut() {
            for index in 0..band_count {
                point.values[index] = point.values[index]
                    .map(|value| CompactOptI32::new(Some(value / point.counts[index])));
            }
        }
    }

    let band_scales = product
        .spec
        .band_specs
        .iter()
        .map(|band| BandScale {
            name: &band.name,
            scaling: band.binary_scale != 0
                || band.reference_value != 0.0
                || band.decimal_scale != 0,
            reference: band.reference_value,
            binary_scale: 2f32.powi(band.binary_scale as i32),
            decimal_scale: 10f64.powi(-band.decimal_scale as i32),
        })
        .collect_vec();

    mvt::render_mvt_layer(
        layer_name,
        &product.spec,
        extent,
        &tile_context.bounds,
        &points,
        &band_scales,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(value: i32) -> CompactOptI32 {
        CompactOptI32::new(Some(value))
    }

    #[test]
    fn rough_avg_aggregates_sum_and_count_in_one_pass() {
        let values = [value(7), CompactOptI32::NONE, value(-2), value(5)];

        let (sum, count) = aggregate_band(&values, Aggregation::RoughAvg);

        assert_eq!(sum, value(10));
        assert_eq!(count, 3);
    }

    #[test]
    fn non_average_aggregations_do_not_track_counts() {
        let values = [value(6), CompactOptI32::NONE, value(3)];

        for (aggregation, expected) in [
            (Aggregation::Max, 6),
            (Aggregation::Min, 3),
            (Aggregation::BitOr, 7),
        ] {
            let (aggregated, count) = aggregate_band(&values, aggregation);
            assert_eq!(aggregated, value(expected));
            assert_eq!(count, 0);
        }
    }

    #[test]
    fn merge_only_updates_active_bands() {
        let key = (1, 2);
        let mut points = IndexMap::default();
        points.insert(
            key,
            Point {
                values: [value(10), value(20), value(30), value(40)],
                counts: [1, 2, 3, 4],
                power: 1,
            },
        );

        merge_point(
            &mut points,
            key,
            Point {
                values: [value(5), value(7), value(11), value(13)],
                counts: [5, 6, 7, 8],
                power: 2,
            },
            Aggregation::RoughAvg,
            2,
        );

        let point = &points[&key];
        assert_eq!(point.values, [value(15), value(27), value(30), value(40)]);
        assert_eq!(point.counts, [6, 8, 3, 4]);
        assert_eq!(point.power, 2);
    }

    #[test]
    fn non_average_merge_does_not_update_counts_or_trailing_bands() {
        let key = (1, 2);

        for (aggregation, expected) in [
            (Aggregation::Max, [value(10), value(20)]),
            (Aggregation::Min, [value(5), value(7)]),
            (Aggregation::BitOr, [value(15), value(23)]),
        ] {
            let mut points = IndexMap::default();
            points.insert(
                key,
                Point {
                    values: [value(10), value(20), value(30), value(40)],
                    counts: [1, 2, 3, 4],
                    power: 1,
                },
            );

            merge_point(
                &mut points,
                key,
                Point {
                    values: [value(5), value(7), value(11), value(13)],
                    counts: [5, 6, 7, 8],
                    power: 2,
                },
                aggregation,
                2,
            );

            let point = &points[&key];
            assert_eq!(&point.values[..2], &expected);
            assert_eq!(&point.values[2..], &[value(30), value(40)]);
            assert_eq!(point.counts, [1, 2, 3, 4]);
            assert_eq!(point.power, 2);
        }
    }
}
