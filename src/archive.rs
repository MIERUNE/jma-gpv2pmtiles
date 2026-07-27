use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    sync::mpsc::{SyncSender, sync_channel},
};

use anyhow::{Context, Result, bail, ensure};

use pmtiles::{Compression, PmTilesWriter, TileCoord, TileType};
use prost::Message;
use rayon::prelude::*;
use tracing::info;

use crate::{
    metadata,
    model::PreparedProduct,
    prepare, quantize,
    tile::{TileContext, Zxy, make_layer, zxy_to_chunk_id_range},
};

#[derive(Clone, Debug)]
pub struct ConvertOptions {
    pub product: Option<String>,
    pub layer_name_pattern: String,
    pub layer_count: Option<usize>,
    pub min_zoom: u8,
    pub max_zoom: Option<u8>,
    /// One `--quantize` specification per band; see [`crate::quantize`].
    pub quantize: Vec<String>,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        Self {
            product: None,
            layer_name_pattern: "layer_{seq}".to_string(),
            layer_count: None,
            min_zoom: 0,
            max_zoom: None,
            quantize: Vec::new(),
        }
    }
}

pub fn convert(input: &Path, output: &Path, options: &ConvertOptions) -> Result<()> {
    validate_options(options)?;
    info!(input = %input.display(), "parsing GRIB2");
    let mut products = select_products(
        prepare::read_products(input)?,
        options.product.as_deref(),
        options.layer_count,
    )?;
    let layer_names = (0..products.len())
        .map(|sequence| {
            options
                .layer_name_pattern
                .replace("{seq}", &sequence.to_string())
        })
        .collect::<Vec<_>>();

    ensure_compatible_products(&products)?;
    apply_quantization(&mut products, &options.quantize)?;
    let max_zoom = options.max_zoom.unwrap_or_else(|| {
        products
            .iter()
            .map(|product| {
                (product.spec.grid_spec.lat_denom * 360.0 * 2.0 / 512.0)
                    .log2()
                    .round() as u8
            })
            .max()
            .unwrap_or(options.min_zoom)
    });
    ensure!(
        options.min_zoom <= max_zoom,
        "minimum zoom {} exceeds maximum zoom {max_zoom}",
        options.min_zoom
    );

    let bounds = combined_bounds(&products);
    let center = ((bounds[0] + bounds[2]) / 2.0, (bounds[1] + bounds[3]) / 2.0);
    let archive_name = output
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("grib2pmtiles");
    let metadata = serde_json::to_string(&metadata::generate_metadata(
        archive_name,
        &products,
        &layer_names,
        bounds,
        options.min_zoom,
        max_zoom,
    ))?;
    info!(
        layers = products.len(),
        min_zoom = options.min_zoom,
        max_zoom,
        "finished parsing GRIB2; generating PMTiles"
    );

    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(output)
        .with_context(|| format!("failed to create output {}", output.display()))?;
    let mut writer = PmTilesWriter::new(TileType::Mvt)
        .tile_compression(Compression::Gzip)
        .min_zoom(options.min_zoom)
        .max_zoom(max_zoom)
        .bounds(bounds[0], bounds[1], bounds[2], bounds[3])
        .center(center.0, center.1)
        .metadata(&metadata)
        .create(&mut file)?;

    let (sender, receiver) = sync_channel::<(Zxy, Vec<u8>)>(32);
    std::thread::scope(|scope| -> Result<()> {
        let producer_sender = sender.clone();
        let product_refs = &products;
        let layer_name_refs = &layer_names;
        let producer = scope.spawn(move || {
            traverse_tile_pyramid(
                (0, 0, 0),
                product_refs,
                layer_name_refs,
                options.min_zoom,
                max_zoom,
                &producer_sender,
            )
        });
        drop(sender);

        while let Ok(((z, x, y), encoded_tile)) = receiver.recv() {
            writer.add_raw_tile(TileCoord::new(z, x, y)?, &encoded_tile)?;
        }

        producer
            .join()
            .map_err(|_| anyhow::anyhow!("tile producer panicked"))??;
        Ok(())
    })?;

    writer.finalize()?;
    normalize_empty_leaf_offset(&mut file)?;
    file.flush()?;
    info!(
        output = %output.display(),
        layers = products.len(),
        "wrote PMTiles"
    );
    Ok(())
}

// pmtiles-rs 0.23 leaves this offset at zero when the root directory fits
// without leaf directories. The PMTiles v3 verifier expects it to point to
// the end of tile data even when the corresponding length is zero.
fn normalize_empty_leaf_offset(file: &mut (impl Read + Write + Seek)) -> Result<()> {
    const LEAF_OFFSET_POSITION: u64 = 40;
    const LEAF_LENGTH_POSITION: u64 = 48;
    const DATA_OFFSET_POSITION: u64 = 56;

    let mut bytes = [0; 8];
    file.seek(SeekFrom::Start(LEAF_OFFSET_POSITION))?;
    file.read_exact(&mut bytes)?;
    let leaf_offset = u64::from_le_bytes(bytes);

    file.seek(SeekFrom::Start(LEAF_LENGTH_POSITION))?;
    file.read_exact(&mut bytes)?;
    let leaf_length = u64::from_le_bytes(bytes);
    if leaf_offset != 0 || leaf_length != 0 {
        return Ok(());
    }

    file.seek(SeekFrom::Start(DATA_OFFSET_POSITION))?;
    file.read_exact(&mut bytes)?;
    let data_offset = u64::from_le_bytes(bytes);
    file.read_exact(&mut bytes)?;
    let data_length = u64::from_le_bytes(bytes);
    let empty_leaf_offset = data_offset
        .checked_add(data_length)
        .context("PMTiles data range overflows u64")?;

    file.seek(SeekFrom::Start(LEAF_OFFSET_POSITION))?;
    file.write_all(&empty_leaf_offset.to_le_bytes())?;
    Ok(())
}

fn validate_options(options: &ConvertOptions) -> Result<()> {
    ensure!(
        options.layer_name_pattern.contains("{seq}"),
        "layer name pattern must contain the {{seq}} placeholder"
    );
    if let Some(layer_count) = options.layer_count {
        ensure!(layer_count > 0, "layer count must be greater than zero");
    }
    quantize::validate_syntax(&options.quantize)?;
    Ok(())
}

/// Resolves `--quantize` once and shares it with every layer.
///
/// `ensure_compatible_products` has already established that the products agree
/// on their bands, so a single resolution applies to all of them.
fn apply_quantization(products: &mut [PreparedProduct], args: &[String]) -> Result<()> {
    if args.is_empty() {
        return Ok(());
    }
    let Some(first) = products.first() else {
        return Ok(());
    };
    let resolved = quantize::resolve(args, &first.spec.band_specs)?;
    for (band, quantize) in first.spec.band_specs.iter().zip(&resolved) {
        if let Some(quantize) = quantize {
            info!(
                band = %band.name,
                classes = quantize.outputs().len(),
                "quantizing values"
            );
        }
    }
    for product in products {
        product.spec.quantize = resolved.clone();
    }
    Ok(())
}

fn select_products(
    mut products: Vec<PreparedProduct>,
    requested_product: Option<&str>,
    layer_count: Option<usize>,
) -> Result<Vec<PreparedProduct>> {
    let available_products = products
        .iter()
        .map(product_selector)
        .collect::<BTreeSet<_>>();
    ensure!(
        !available_products.is_empty(),
        "input contains no convertible value products"
    );
    let selected_product = resolve_product_selector(&available_products, requested_product)?;
    products.retain(|product| product_selector(product) == selected_product);

    products.sort_by(|left, right| {
        left.product_id
            .datetime
            .cmp(&right.product_id.datetime)
            .then_with(|| left.product_id.path().cmp(&right.product_id.path()))
    });
    if let Some(layer_count) = layer_count {
        ensure!(
            products.len() >= layer_count,
            "requested {layer_count} layers, but the input contains only {} matching value products",
            products.len()
        );
        products.truncate(layer_count);
    }
    Ok(products)
}

fn product_selector(product: &PreparedProduct) -> String {
    let (data_kind, value_kind) = product.product_id.path_parts();
    if value_kind.is_empty() {
        data_kind.to_string()
    } else {
        format!("{data_kind}/{value_kind}")
    }
}

fn resolve_product_selector(
    available_products: &BTreeSet<String>,
    requested_product: Option<&str>,
) -> Result<String> {
    if let Some(requested_product) = requested_product {
        ensure!(
            available_products.contains(requested_product),
            "product `{requested_product}` is not present in the input; available choices:\n{}",
            product_choices(available_products)
        );
        return Ok(requested_product.to_string());
    }

    ensure!(
        available_products.len() == 1,
        "input contains multiple products; select one explicitly:\n{}",
        product_choices(available_products)
    );
    Ok(available_products
        .first()
        .expect("one available product was verified")
        .clone())
}

fn product_choices(available_products: &BTreeSet<String>) -> String {
    available_products
        .iter()
        .map(|product| format!("  --product {product}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn ensure_compatible_products(products: &[PreparedProduct]) -> Result<()> {
    let first = &products[0].spec;
    for product in &products[1..] {
        if product.spec.base_z != first.base_z || product.spec.grid_spec != first.grid_spec {
            bail!(
                "selected products use incompatible grids: {} and {}",
                first.name,
                product.spec.name
            );
        }
    }
    Ok(())
}

fn combined_bounds(products: &[PreparedProduct]) -> [f64; 4] {
    let mut bounds = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
    for product in products {
        bounds[0] = bounds[0].min(product.spec.bounds[0]);
        bounds[1] = bounds[1].min(product.spec.bounds[1]);
        bounds[2] = bounds[2].max(product.spec.bounds[2]);
        bounds[3] = bounds[3].max(product.spec.bounds[3]);
    }
    bounds[0] = bounds[0].max(-180.0);
    bounds[1] = bounds[1].max(-90.0);
    bounds[2] = bounds[2].min(180.0);
    bounds[3] = bounds[3].min(90.0);
    bounds
}

fn traverse_tile_pyramid(
    zxy: Zxy,
    products: &[PreparedProduct],
    layer_names: &[String],
    min_zoom: u8,
    max_zoom: u8,
    sender: &SyncSender<(Zxy, Vec<u8>)>,
) -> Result<bool> {
    let has_source_data = products.iter().any(|product| {
        let (begin, end) = zxy_to_chunk_id_range(product.spec.base_z, zxy);
        product.has_chunks_in_range(begin, end)
    });
    if !has_source_data {
        return Ok(false);
    }

    let (z, x, y) = zxy;
    if z >= min_zoom {
        let tile_context = TileContext::new(zxy);
        let layers = products
            .par_iter()
            .zip(layer_names.par_iter())
            .filter_map(|(product, layer_name)| make_layer(&tile_context, product, layer_name))
            .collect::<Vec<_>>();
        if !layers.is_empty() {
            let protobuf = tinymvt::vector_tile::Tile { layers }.encode_to_vec();
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(&protobuf)?;
            sender
                .send((zxy, encoder.finish()?))
                .map_err(|_| anyhow::anyhow!("PMTiles writer stopped before tile generation"))?;
        }
    }

    if z < max_zoom {
        [(0, 0), (1, 0), (0, 1), (1, 1)]
            .par_iter()
            .try_for_each(|(dx, dy)| {
                traverse_tile_pyramid(
                    (z + 1, x * 2 + dx, y * 2 + dy),
                    products,
                    layer_names,
                    min_zoom,
                    max_zoom,
                    sender,
                )?;
                Ok::<_, anyhow::Error>(())
            })?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn layer_pattern_is_zero_based() {
        let options = ConvertOptions {
            layer_name_pattern: "rain250m_{seq}".into(),
            layer_count: Some(12),
            ..Default::default()
        };
        validate_options(&options).unwrap();
        let names = (0..12)
            .map(|sequence| {
                options
                    .layer_name_pattern
                    .replace("{seq}", &sequence.to_string())
            })
            .collect::<Vec<_>>();
        assert_eq!(names.first().unwrap(), "rain250m_0");
        assert_eq!(names.last().unwrap(), "rain250m_11");
    }

    #[test]
    fn layer_pattern_requires_placeholder() {
        let options = ConvertOptions {
            layer_name_pattern: "rain250m".into(),
            ..Default::default()
        };
        assert!(validate_options(&options).is_err());
    }

    #[test]
    fn single_product_is_selected_implicitly() {
        let available = BTreeSet::from(["hrnowc/intensity".to_string()]);

        assert_eq!(
            resolve_product_selector(&available, None).unwrap(),
            "hrnowc/intensity"
        );
    }

    #[test]
    fn multiple_products_require_an_explicit_selection() {
        let available =
            BTreeSet::from(["hrnowc/precip".to_string(), "hrnowc/intensity".to_string()]);

        let error = resolve_product_selector(&available, None).unwrap_err();
        assert_eq!(
            error.to_string(),
            "input contains multiple products; select one explicitly:\n  --product hrnowc/intensity\n  --product hrnowc/precip"
        );
    }

    #[test]
    fn requested_product_must_be_available() {
        let available =
            BTreeSet::from(["hrnowc/intensity".to_string(), "hrnowc/precip".to_string()]);

        assert_eq!(
            resolve_product_selector(&available, Some("hrnowc/precip")).unwrap(),
            "hrnowc/precip"
        );
        let error = resolve_product_selector(&available, Some("hrnowc/echotops")).unwrap_err();
        assert!(error.to_string().contains("available choices:"));
        assert!(error.to_string().contains("--product hrnowc/intensity"));
        assert!(error.to_string().contains("--product hrnowc/precip"));
    }

    #[test]
    fn empty_leaf_offset_points_after_tile_data() {
        let mut bytes = vec![0; 80];
        bytes[56..64].copy_from_slice(&100u64.to_le_bytes());
        bytes[64..72].copy_from_slice(&23u64.to_le_bytes());
        let mut cursor = Cursor::new(bytes);

        normalize_empty_leaf_offset(&mut cursor).unwrap();

        let bytes = cursor.into_inner();
        assert_eq!(u64::from_le_bytes(bytes[40..48].try_into().unwrap()), 123);
    }
}
