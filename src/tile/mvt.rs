use foldhash::HashMap;
use i_overlay::{
    core::{
        fill_rule::FillRule,
        overlay::{ContourDirection, IntOverlayOptions, Overlay},
        overlay_rule::OverlayRule,
    },
    i_float::int::point::IntPoint,
    i_shape::int::shape::IntContour,
};
use rayon::prelude::*;
use tinymvt::{vector_tile::tile::Layer, webmercator::lnglat_to_web_mercator};

use crate::{
    model::{CompactOptI32, TilesetSpec},
    tile::{BandScale, IndexMap, Point, TileBounds},
};

pub(super) fn render_mvt_layer(
    layer_name: &str,
    tileset_spec: &TilesetSpec,
    extent: i32,
    tile_bounds: &TileBounds,
    deduped_points: &indexmap::IndexMap<(u32, u32), Point, foldhash::fast::RandomState>,
    band_scales: &Vec<BandScale<'_>>,
) -> Option<Layer> {
    let grouped_rings = make_polygons_grouped_by_value(
        deduped_points,
        &tileset_spec.grid_spec,
        tile_bounds,
        extent,
    );
    if grouped_rings.is_empty() {
        return None;
    }

    // Encode geometries
    let value_geom = grouped_rings
        .into_par_iter()
        .map(|(values, contours)| {
            let mut geom_enc = tinymvt::geometry::GeometryEncoder::new();

            // Unary-union of polygons having same value
            let mpoly = Overlay::with_contours_custom(
                &contours,
                &[],
                IntOverlayOptions {
                    output_direction: ContourDirection::CounterClockwise, // MVT spec
                    ..Default::default()
                },
                Default::default(),
            )
            .overlay(OverlayRule::Subject, FillRule::Positive);

            for poly in mpoly {
                for ring in poly {
                    geom_enc.add_ring(ring.iter().map(|p| [p.x, p.y]));
                }
            }
            (values, geom_enc.into_vec())
        })
        .collect::<Vec<_>>();

    // Encode features
    let mut tags_enc = tinymvt::tag::TagsEncoder::new();
    let mut features = Vec::with_capacity(value_geom.len());
    for (values, encoded_geom) in value_geom.into_iter() {
        for (band, raw_value) in band_scales.iter().zip(values) {
            let Some(value) = raw_value.get() else {
                continue;
            };
            tags_enc.add(band.name, band.tag_value(value));
        }

        let feat_id = match tileset_spec.band_specs.len() {
            1 => values[0].unwrap() as u64,
            2 => values[0].unwrap_or(0) as u64 | ((values[1].unwrap_or(0) as u64) << 32),
            3 => {
                // TODO: need improvements?
                values[0].unwrap_or(0) as u64
                    | ((values[1].unwrap_or(0) as u64) << 32)
                    | ((values[2].unwrap_or(0) as u64) << 16)
            }
            4 => {
                // TODO: need improvements?
                values[0].unwrap_or(0) as u64
                    | ((values[1].unwrap_or(0) as u64) << 32)
                    | ((values[2].unwrap_or(0) as u64) << 16)
                    | ((values[3].unwrap_or(0) as u64) << 24)
            }
            _ => unimplemented!("num_bands > 4 is not supported"),
        };
        features.push(tinymvt::vector_tile::tile::Feature {
            id: Some(feat_id),
            tags: tags_enc.take_tags(),
            r#type: Some(tinymvt::vector_tile::tile::GeomType::Polygon as i32),
            geometry: encoded_geom,
        });
    }

    // Layer
    let (keys, values) = tags_enc.into_keys_and_values();
    Some(Layer {
        version: 2,
        name: layer_name.to_string(),
        features,
        keys,
        values,
        extent: Some(extent as u32),
    })
}

/// Creates polygons from data points
fn make_polygons_grouped_by_value(
    deduped_points: &IndexMap<(u32, u32), Point>,
    grid_spec: &gpv_products::model::LngLatGrid,
    tile_bounds: &TileBounds,
    extent: i32,
) -> HashMap<[CompactOptI32; 4], Vec<IntContour<i32>>> {
    let mut grouped_rings: HashMap<[CompactOptI32; 4], Vec<IntContour<i32>>> = HashMap::default();
    let buffer_pixels = 2;
    let buffer = buffer_pixels * extent / 256;
    let tile_width = tile_bounds.mx2 - tile_bounds.mx1;
    let w = (extent as f64) / tile_width;

    for ((x, y), point) in deduped_points {
        let value = point.values;
        let width = 1 << point.power;
        let lng1 = grid_spec.lng_0 + (*x as f64 - 0.5) / grid_spec.lng_denom as f64;
        let lng2 = grid_spec.lng_0 + ((*x + width) as f64 - 0.5) / grid_spec.lng_denom as f64;
        let lat2 = grid_spec.lat_0 + (*y as f64 - 0.5) / grid_spec.lat_denom as f64;
        let lat1 = grid_spec.lat_0 + ((*y + width) as f64 - 0.5) / grid_spec.lat_denom as f64;
        let (mx1, my1) = lnglat_to_web_mercator(lng1, lat1);
        let (mx2, my2) = lnglat_to_web_mercator(lng2, lat2);
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
                grouped_rings.entry(value).or_default().push(vec![
                    IntPoint { x: tx1, y: ty1 },
                    IntPoint { x: tx2, y: ty1 },
                    IntPoint { x: tx2, y: ty2 },
                    IntPoint { x: tx1, y: ty2 },
                ]);
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
                    grouped_rings.entry(value).or_default().push(vec![
                        IntPoint { x: tx1, y: ty1 },
                        IntPoint { x: tx2, y: ty1 },
                        IntPoint { x: tx2, y: ty2 },
                        IntPoint { x: tx1, y: ty2 },
                    ]);
                }
            }
        }
    }

    grouped_rings
}
