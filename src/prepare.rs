use std::{
    fs::File,
    io::{BufReader, Read},
    path::Path,
};

use anyhow::{Context, Result};
use foldhash::HashMap;
use gpv_products::{
    grib::GridSquareMessageReader,
    products::{GpvProductIdentifier, ProductData},
};
use rayon::prelude::*;
use tinygrib2::MessageReader;
use tinymvt::webmercator::lnglat_to_web_mercator;

use crate::{
    hilbert::xy_to_hilbert,
    model::{Band, BaseTile, CompactOptI32, PreparedProduct, TilesetSpec},
};

#[derive(Clone, Debug, Default)]
struct TilePoint {
    point_id: u32,
    point_power: u8,
    values: [CompactOptI32; 4],
}

pub(crate) fn read_products(input: &Path) -> Result<Vec<PreparedProduct>> {
    let file =
        File::open(input).with_context(|| format!("failed to open input {}", input.display()))?;
    let message_reader = if input.extension().is_some_and(|ext| ext == "gz") {
        read_messages(BufReader::new(flate2::read::GzDecoder::new(
            BufReader::new(file),
        )))?
    } else {
        read_messages(BufReader::new(file))?
    };

    Ok(message_reader
        .products
        .into_par_iter()
        .map(|(product_id, product_data)| prepare_product(product_id, product_data))
        .collect())
}

fn read_messages<R: Read>(mut reader: R) -> Result<GridSquareMessageReader> {
    let mut message_reader = GridSquareMessageReader::default();
    while let Some(()) = message_reader.read_next_message(&mut reader)? {}
    Ok(message_reader)
}

fn prepare_product(
    product_id: GpvProductIdentifier,
    mut product_data: ProductData,
) -> PreparedProduct {
    product_data.points.par_iter_mut().for_each(|point| {
        point.point_id = xy_to_hilbert(16, point.x as u32, point.y as u32) as u32;
    });
    product_data
        .points
        .par_sort_unstable_by_key(|point| point.point_id);

    let mut min_lng = f64::MAX;
    let mut max_lng = f64::MIN;
    let mut min_lat = f64::MAX;
    let mut max_lat = f64::MIN;
    let base_z = product_id.base_z();
    let grid = product_id.grid();
    let buffer = (2.0 / 256.0) / (1 << base_z) as f64;
    let mut tiles: HashMap<(u32, u32), Vec<TilePoint>> = HashMap::default();

    for point_bands in product_data
        .points
        .chunk_by(|left, right| left.point_id == right.point_id)
    {
        let point = point_bands.first().expect("point group is not empty");
        let width = 1 << point.point_power;
        let lng1 = grid.lng_0 + (point.x as f64 - 0.5) / grid.lng_denom as f64;
        let lng2 = grid.lng_0 + ((point.x + width) as f64 - 0.5) / grid.lng_denom as f64;
        let lat1 = grid.lat_0 + ((point.y + width) as f64 - 0.5) / grid.lat_denom as f64;
        let lat2 = grid.lat_0 + (point.y as f64 - 0.5) / grid.lat_denom as f64;

        min_lng = min_lng.min(lng1);
        max_lng = max_lng.max(lng2);
        min_lat = min_lat.min(lat2);
        max_lat = max_lat.max(lat1);

        let (mx1, my1) = lnglat_to_web_mercator(lng1, lat1);
        let (mx2, my2) = lnglat_to_web_mercator(lng2, lat2);
        if my1.is_nan() || my2.is_nan() {
            continue;
        }

        let x1 = ((mx1 - buffer) * (1 << base_z) as f64).floor() as i32;
        let x2 = ((mx2 + buffer) * (1 << base_z) as f64).ceil() as i32 - 1;
        let y1 = ((my1 - buffer) * (1 << base_z) as f64).floor() as i32;
        let y2 = ((my2 + buffer) * (1 << base_z) as f64).ceil() as i32 - 1;

        let mut tile_point = TilePoint {
            point_id: point.point_id,
            point_power: point.point_power,
            ..Default::default()
        };
        for point_band in point_bands {
            tile_point.values[point_band.band_idx as usize] =
                CompactOptI32::new(Some(point_band.value));
        }

        for x in x1..=x2 {
            let x = x.rem_euclid(1 << base_z);
            for y in y1..=y2 {
                if !(0..1 << base_z).contains(&y) {
                    continue;
                }
                tiles
                    .entry((x as u32, y as u32))
                    .or_default()
                    .push(tile_point.clone());
            }
        }
    }

    let band_count = product_id.bands().len();
    assert!((1..=4).contains(&band_count));
    let mut chunks = tiles
        .into_par_iter()
        .map(|((tile_x, tile_y), points)| {
            let bands = (0..band_count)
                .map(|band_index| Band {
                    values: points
                        .iter()
                        .map(|point| point.values[band_index])
                        .collect(),
                })
                .collect();
            let tile = BaseTile {
                point_ids: points.iter().map(|point| point.point_id).collect(),
                point_powers: points.iter().map(|point| point.point_power).collect(),
                bands,
            };
            (xy_to_hilbert(base_z, tile_x, tile_y), tile)
        })
        .collect::<Vec<_>>();
    chunks.par_sort_unstable_by_key(|(tile_id, _)| *tile_id);

    let spec = TilesetSpec {
        name: product_id.path(),
        base_z,
        grid_spec: product_id.grid().clone(),
        aggregation: product_id.aggregation(),
        quantize: vec![None; product_data.band_specs.len()],
        omit: vec![None; product_data.band_specs.len()],
        band_specs: product_data.band_specs,
        bounds: [min_lng, min_lat, max_lng, max_lat],
    };

    PreparedProduct {
        product_id,
        spec,
        chunks,
    }
}
