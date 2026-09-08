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

use fast_hilbert::xy2h;

use crate::{
    geo::lnglat_to_web_mercator,
    model::{Band, BaseTile, CompactOptI32, PreparedProduct, TilesetSpec},
};

#[derive(Clone, Debug, Default)]
struct TilePoint {
    x: u16,
    y: u16,
    point_power: u8,
    values: [CompactOptI32; 4],
}

#[derive(Debug)]
pub(crate) struct RawProduct {
    pub product_id: GpvProductIdentifier,
    pub data: ProductData,
}

pub(crate) fn read_products(
    input: &Path,
    retained_product: Option<&str>,
) -> Result<Vec<RawProduct>> {
    let file =
        File::open(input).with_context(|| format!("failed to open input {}", input.display()))?;
    let message_reader = if input.extension().is_some_and(|ext| ext == "gz") {
        read_messages(
            BufReader::new(flate2::read::GzDecoder::new(BufReader::new(file))),
            retained_product,
        )?
    } else {
        read_messages(BufReader::new(file), retained_product)?
    };

    Ok(message_reader
        .products
        .into_iter()
        .map(|(product_id, data)| RawProduct { product_id, data })
        .collect())
}

pub(crate) fn prepare_products(products: Vec<RawProduct>) -> Vec<PreparedProduct> {
    products
        .into_par_iter()
        .map(|product| prepare_product(product.product_id, product.data))
        .collect()
}

fn read_messages<R: Read>(
    mut reader: R,
    retained_product: Option<&str>,
) -> Result<GridSquareMessageReader> {
    let mut message_reader = GridSquareMessageReader::retaining_product(retained_product);
    while let Some(()) = message_reader.read_next_message(&mut reader)? {}
    Ok(message_reader)
}

fn prepare_product(
    product_id: GpvProductIdentifier,
    mut product_data: ProductData,
) -> PreparedProduct {
    let grid = product_data
        .grid
        .clone()
        .unwrap_or_else(|| product_id.grid().clone());
    product_data.points.par_iter_mut().for_each(|point| {
        point.point_id = xy2h(point.x as u32, point.y as u32, 16) as u32;
    });
    product_data
        .points
        .par_sort_unstable_by_key(|point| point.point_id);

    let mut min_lng = f64::MAX;
    let mut max_lng = f64::MIN;
    let mut min_lat = f64::MAX;
    let mut max_lat = f64::MIN;
    let base_z = (grid.lat_denom * 360. * 2. / 512.).log2().ceil() as u8;
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
            x: point.x,
            y: point.y,
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
                point_positions: points.iter().map(|point| (point.x, point.y)).collect(),
                point_powers: points.iter().map(|point| point.point_power).collect(),
                bands,
            };
            (xy2h(tile_x, tile_y, base_z), tile)
        })
        .collect::<Vec<_>>();
    chunks.par_sort_unstable_by_key(|(tile_id, _)| *tile_id);

    let spec = TilesetSpec {
        name: product_id.path(),
        base_z,
        grid_spec: grid,
        aggregation: product_id.aggregation(),
        quantize: vec![None; product_data.band_specs.len()],
        omit_zero: vec![None; product_data.band_specs.len()],
        omit_class: vec![None; product_data.band_specs.len()],
        band_specs: product_data.band_specs,
        bounds: [min_lng, min_lat, max_lng, max_lat],
    };

    PreparedProduct {
        product_id,
        spec,
        chunks,
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use gpv_products::{
        model::{BandSpec, LngLatGrid},
        products::{
            FixedSurface, GeneratingProcessType, GpvProductElement, GpvProductIdentifier,
            PointValue, ProductData,
        },
    };

    use super::*;

    #[test]
    fn prepared_tiles_keep_original_grid_coordinates() {
        let datetime = Utc.timestamp_opt(1_570_871_100, 0).unwrap();
        let product_id = GpvProductIdentifier {
            kind: GpvProductElement::HiresNowcastIntensity,
            datetime,
            reference_datetime: datetime,
            generating_process: GeneratingProcessType::Forecast,
            ..Default::default()
        };
        let product_data = ProductData {
            points: vec![PointValue {
                x: 100,
                y: 200,
                point_id: 0,
                point_power: 0,
                band_idx: 0,
                value: 42,
            }],
            band_specs: vec![BandSpec {
                name: "value".to_string(),
                ..Default::default()
            }],
            grid: None,
        };

        let prepared = prepare_product(product_id, product_data);
        let mut position_count = 0;
        for (_, tile) in &prepared.chunks {
            assert_eq!(tile.point_positions.len(), tile.point_powers.len());
            for position in &tile.point_positions {
                assert_eq!(*position, (100, 200));
                position_count += 1;
            }
        }

        assert!(position_count > 0);
    }

    #[test]
    fn preparation_uses_the_grid_derived_from_the_message() {
        let grid = LngLatGrid {
            lng_0: 120.0,
            lat_0: 20.0,
            lng_denom: 40.0,
            lat_denom: 50.0,
        };
        let product_id = GpvProductIdentifier {
            kind: GpvProductElement::LfmWind,
            surface: FixedSurface::IsobaricSurface(1000, 0),
            generating_process: GeneratingProcessType::Forecast,
            ..Default::default()
        };
        let product_data = ProductData {
            points: vec![PointValue {
                x: 0,
                y: 0,
                point_id: 0,
                point_power: 0,
                band_idx: 0,
                value: 42,
            }],
            band_specs: vec![BandSpec::default(), BandSpec::default()],
            grid: Some(grid.clone()),
        };

        let prepared = prepare_product(product_id, product_data);

        assert_eq!(prepared.spec.grid_spec, grid);
        assert_eq!(prepared.spec.base_z, 7);
    }
}
