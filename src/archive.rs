use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    sync::mpsc::{SyncSender, sync_channel},
};

use anyhow::{Context, Result, bail, ensure};
use gpv_products::products::GeneratingProcessType;

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
    /// First value substituted for `{seq}` in the layer name pattern.
    pub layer_seq_start: usize,
    /// Band renames as `from=to`.
    pub rename: Vec<String>,
    /// One `--quantize` specification per band; see [`crate::quantize`].
    pub quantize: Vec<String>,
    /// Values whose cells are left out of the tile, as `[<band>=]<value>[,...]`.
    pub omit: Vec<String>,
    /// Leave out cells whose value is zero, on every band.
    pub omit_zero: bool,
    /// Drop products whose generating process is an analysis rather than a forecast.
    pub skip_analysis: bool,
    /// Keep only products at least this many minutes ahead of the reference time.
    pub min_lead_time: Option<i64>,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        Self {
            product: None,
            layer_name_pattern: "layer_{seq}".to_string(),
            layer_count: None,
            min_zoom: 0,
            max_zoom: None,
            layer_seq_start: 0,
            rename: Vec::new(),
            quantize: Vec::new(),
            omit: Vec::new(),
            omit_zero: false,
            skip_analysis: false,
            min_lead_time: None,
        }
    }
}

/// Builds the source-layer names, numbering from `layer_seq_start`.
///
/// Products are ordered by valid time, so an offset lets the numbering match
/// the forecast hours of the input (for example `FH01-06` starting at 1).
fn build_layer_names(options: &ConvertOptions, count: usize) -> Vec<String> {
    let start = options.layer_seq_start;
    (start..start + count)
        .map(|sequence| {
            options
                .layer_name_pattern
                .replace("{seq}", &sequence.to_string())
        })
        .collect()
}

pub fn convert(input: &Path, output: &Path, options: &ConvertOptions) -> Result<()> {
    validate_options(options)?;
    info!(input = %input.display(), "parsing GRIB2");
    let mut products = select_products(
        prepare::read_products(input)?,
        options.product.as_deref(),
        options.layer_count,
        options.skip_analysis,
        options.min_lead_time,
    )?;
    let layer_names = build_layer_names(options, products.len());

    ensure_compatible_products(&products)?;
    apply_renames(&mut products, &options.rename)?;
    apply_quantization(
        &mut products,
        &options.quantize,
        &options.omit,
        options.omit_zero,
    )?;
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
    for arg in &options.rename {
        parse_rename(arg)?;
    }
    quantize::validate_syntax(&options.quantize)?;
    quantize::validate_omit_syntax(&options.omit)?;
    Ok(())
}

fn parse_rename(arg: &str) -> Result<(&str, &str)> {
    let invalid = || format!("--rename expects <from>=<to> but found {arg:?}");
    let (from, to) = arg.split_once('=').with_context(invalid)?;
    let (from, to) = (from.trim(), to.trim());
    ensure!(!from.is_empty() && !to.is_empty(), "{}", invalid());
    Ok((from, to))
}

/// Renames bands before anything downstream reads their names.
///
/// The band name becomes the MVT attribute key and the metadata field name, and
/// `--quantize` selects bands by name, so renaming first keeps all three
/// consistent. `--quantize` therefore refers to the new name.
fn apply_renames(products: &mut [PreparedProduct], args: &[String]) -> Result<()> {
    if args.is_empty() {
        return Ok(());
    }
    let Some(first) = products.first() else {
        return Ok(());
    };
    let mut names = first
        .spec
        .band_specs
        .iter()
        .map(|band| band.name.clone())
        .collect::<Vec<_>>();
    resolve_renames(&mut names, args)?;

    for product in products {
        for (band, name) in product.spec.band_specs.iter_mut().zip(&names) {
            band.name = name.clone();
        }
    }
    Ok(())
}

/// Applies the renames to a list of band names in place.
fn resolve_renames(names: &mut [String], args: &[String]) -> Result<()> {
    for arg in args {
        let (from, to) = parse_rename(arg)?;
        let available = names
            .iter()
            .map(|name| format!("{name:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        let index = names
            .iter()
            .position(|name| name == from)
            .with_context(|| {
                format!("unknown band {from:?} in --rename; this product has {available}")
            })?;
        ensure!(
            !names.iter().any(|name| name == to),
            "--rename target {to:?} collides with another band of this product"
        );
        info!(from = %from, to = %to, "renaming band");
        names[index] = to.to_string();
    }
    Ok(())
}

/// Resolves `--quantize` once and shares it with every layer.
///
/// `ensure_compatible_products` has already established that the products agree
/// on their bands, so a single resolution applies to all of them.
fn apply_quantization(
    products: &mut [PreparedProduct],
    args: &[String],
    omit_args: &[String],
    omit_zero: bool,
) -> Result<()> {
    if args.is_empty() && omit_args.is_empty() && !omit_zero {
        return Ok(());
    }
    let Some(first) = products.first() else {
        return Ok(());
    };
    let band_specs = first.spec.band_specs.clone();
    let resolved = if args.is_empty() {
        vec![None; band_specs.len()]
    } else {
        quantize::resolve(args, &band_specs)?
    };
    let omits = quantize::resolve_omits(omit_args, omit_zero, &resolved, &band_specs)?;

    for (index, band) in band_specs.iter().enumerate() {
        if let Some(quantize) = &resolved[index] {
            info!(
                band = %band.name,
                classes = quantize.class_count(),
                "quantizing values"
            );
        }
        if let Some(omit) = &omits[index] {
            info!(band = %band.name, values = ?omit.physical(), "omitting values");
        }
    }

    for product in products {
        product.spec.quantize = resolved.clone();
        product.spec.omit = omits.clone();
    }
    Ok(())
}

fn select_products(
    mut products: Vec<PreparedProduct>,
    requested_product: Option<&str>,
    layer_count: Option<usize>,
    skip_analysis: bool,
    min_lead_time: Option<i64>,
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

    if let Some(minutes) = min_lead_time {
        // The first nowcast step is valid at the reference time itself, so a
        // lead time floor is what separates "now" from the actual forecasts.
        let before = products.len();
        products.retain(|product| {
            (product.product_id.datetime - product.product_id.reference_datetime).num_minutes()
                >= minutes
        });
        ensure!(
            !products.is_empty(),
            "--min-lead-time {minutes} removed every product of {selected_product}"
        );
        info!(
            minutes,
            dropped = before - products.len(),
            "dropping products below the lead time"
        );
    }

    if skip_analysis {
        // Nowcast inputs lead with the observed field, whose valid time is
        // before the reference time. Dropping it here, ahead of the layer count,
        // keeps the numbering aligned with the forecast steps.
        let before = products.len();
        products.retain(|product| {
            product.product_id.generating_process != GeneratingProcessType::Analysis
        });
        ensure!(
            !products.is_empty(),
            "--skip-analysis removed every product; {selected_product} contains only analyses"
        );
        info!(
            dropped = before - products.len(),
            "skipping analysis products"
        );
    }

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
        let names = build_layer_names(&options, 12);

        assert_eq!(names.first().unwrap(), "rain250m_0");
        assert_eq!(names.last().unwrap(), "rain250m_11");
    }

    #[test]
    fn layer_seq_start_shifts_the_numbering() {
        // FH01-06 delivers six products whose forecast hours are 1 through 6.
        let options = ConvertOptions {
            layer_name_pattern: "rain1km6h_{seq}".into(),
            layer_seq_start: 1,
            ..Default::default()
        };
        validate_options(&options).unwrap();

        let names = build_layer_names(&options, 6);

        assert_eq!(names.first().unwrap(), "rain1km6h_1");
        assert_eq!(names.last().unwrap(), "rain1km6h_6");
        assert_eq!(names.len(), 6);
    }

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn args(values: &[&str]) -> Vec<String> {
        names(values)
    }

    #[test]
    fn rename_rewrites_the_band_name() {
        let mut bands = names(&["value"]);

        resolve_renames(&mut bands, &args(&["value=DN"])).unwrap();

        assert_eq!(bands, names(&["DN"]));
    }

    #[test]
    fn rename_touches_only_the_named_band() {
        let mut bands = names(&["u", "v"]);

        resolve_renames(&mut bands, &args(&["v=northward"])).unwrap();

        assert_eq!(bands, names(&["u", "northward"]));
    }

    #[test]
    fn renames_can_be_chained() {
        let mut bands = names(&["u", "v"]);

        resolve_renames(&mut bands, &args(&["u=eastward", "v=northward"])).unwrap();

        assert_eq!(bands, names(&["eastward", "northward"]));
    }

    #[test]
    fn rename_requires_both_sides() {
        for arg in ["value", "value=", "=DN", ""] {
            let error = parse_rename(arg).unwrap_err();
            assert!(
                error.to_string().contains("<from>=<to>"),
                "unexpected error for {arg:?}: {error}"
            );
        }
    }

    #[test]
    fn rename_reports_a_band_that_is_not_there() {
        let mut bands = names(&["value"]);

        let error = resolve_renames(&mut bands, &args(&["nope=DN"])).unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("unknown band"), "unexpected: {message}");
        assert!(message.contains("\"value\""), "unexpected: {message}");
    }

    #[test]
    fn rename_rejects_a_name_already_in_use() {
        // Renaming `u` onto `v` would leave two bands sharing one attribute key.
        let mut bands = names(&["u", "v"]);

        let error = resolve_renames(&mut bands, &args(&["u=v"])).unwrap_err();

        assert!(
            error.to_string().contains("collides"),
            "unexpected error: {error}"
        );
        assert_eq!(bands, names(&["u", "v"]), "the names must be left alone");
    }

    #[test]
    fn layer_pattern_requires_placeholder() {
        let options = ConvertOptions {
            layer_name_pattern: "rain250m".into(),
            ..Default::default()
        };
        assert!(validate_options(&options).is_err());
    }

    fn product(minutes: i64, process: GeneratingProcessType) -> PreparedProduct {
        use chrono::{TimeZone, Utc};
        use gpv_products::{
            model::{Aggregation, LngLatGrid},
            products::{GpvProductElement, GpvProductIdentifier},
        };

        let reference = Utc.timestamp_opt(1_570_871_100, 0).unwrap();
        let product_id = GpvProductIdentifier {
            kind: GpvProductElement::HiresNowcastIntensity,
            reference_datetime: reference,
            datetime: reference + chrono::Duration::minutes(minutes),
            generating_process: process,
            ..Default::default()
        };
        PreparedProduct {
            spec: crate::model::TilesetSpec {
                name: product_id.path(),
                base_z: 0,
                grid_spec: LngLatGrid {
                    lng_0: 0.0,
                    lat_0: 0.0,
                    lng_denom: 1.0,
                    lat_denom: 1.0,
                },
                aggregation: Aggregation::Max,
                band_specs: Vec::new(),
                quantize: Vec::new(),
                omit: Vec::new(),
                bounds: [0.0; 4],
            },
            product_id,
            chunks: Vec::new(),
        }
    }

    fn offsets(products: &[PreparedProduct]) -> Vec<i64> {
        products
            .iter()
            .map(|product| {
                (product.product_id.datetime - product.product_id.reference_datetime).num_minutes()
            })
            .collect()
    }

    #[test]
    fn the_analysis_leads_the_layers_by_default() {
        // A nowcast input starts with the observed field, five minutes back.
        let input = vec![
            product(-5, GeneratingProcessType::Analysis),
            product(0, GeneratingProcessType::Forecast),
            product(5, GeneratingProcessType::Forecast),
        ];

        let selected = select_products(input, None, None, false, None).unwrap();

        assert_eq!(offsets(&selected), [-5, 0, 5]);
    }

    #[test]
    fn skip_analysis_drops_the_observed_field() {
        let input = vec![
            product(-5, GeneratingProcessType::Analysis),
            product(0, GeneratingProcessType::Forecast),
            product(5, GeneratingProcessType::Forecast),
        ];

        let selected = select_products(input, None, None, true, None).unwrap();

        assert_eq!(offsets(&selected), [0, 5]);
    }

    #[test]
    fn skip_analysis_runs_before_the_layer_count() {
        // The count must apply to the forecasts, not include the analysis.
        let input = vec![
            product(-5, GeneratingProcessType::Analysis),
            product(0, GeneratingProcessType::Forecast),
            product(5, GeneratingProcessType::Forecast),
        ];

        let selected = select_products(input, None, Some(2), true, None).unwrap();

        assert_eq!(offsets(&selected), [0, 5]);
    }

    #[test]
    fn min_lead_time_also_drops_the_step_valid_at_the_reference_time() {
        // The first nowcast forecast is valid at the reference time itself.
        let input = vec![
            product(-5, GeneratingProcessType::Analysis),
            product(0, GeneratingProcessType::Forecast),
            product(5, GeneratingProcessType::Forecast),
            product(10, GeneratingProcessType::Forecast),
        ];

        let selected = select_products(input, None, None, false, Some(5)).unwrap();

        assert_eq!(offsets(&selected), [5, 10]);
    }

    #[test]
    fn min_lead_time_reports_an_input_with_nothing_far_enough_ahead() {
        let input = vec![product(0, GeneratingProcessType::Forecast)];

        let error = select_products(input, None, None, false, Some(5)).unwrap_err();

        assert!(
            error.to_string().contains("removed every product"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn skip_analysis_reports_an_input_with_no_forecast() {
        let input = vec![product(-5, GeneratingProcessType::Analysis)];

        let error = select_products(input, None, None, true, None).unwrap_err();

        assert!(
            error.to_string().contains("removed every product"),
            "unexpected error: {error}"
        );
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
