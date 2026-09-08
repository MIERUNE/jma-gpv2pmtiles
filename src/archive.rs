use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::mpsc::{SyncSender, sync_channel},
};

use anyhow::{Context, Result, bail, ensure};
use gpv_products::products::{GeneratingProcessType, GpvProductIdentifier};

use pmtiles::{Compression, PmTilesWriter, TileCoord, TileType};
use rayon::prelude::*;
use tracing::info;

use crate::{
    metadata,
    model::PreparedProduct,
    prepare, quantize,
    tile::{ColorEncoding, TileContext, TileOutput, Zxy, make_layer, zxy_to_chunk_id_range},
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
    /// Quantized class outputs left out of the tile, as `[<band>=]<value>[,...]`.
    pub omit_class: Vec<String>,
    /// Leave out cells whose physical value is zero before quantization.
    pub omit_zero: bool,
    /// Emit RGBA raster tiles with this colour encoding instead of MVT.
    pub raster: Option<String>,
    /// Write an index of the raster archives to this path.
    pub manifest: Option<PathBuf>,
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
            omit_class: Vec::new(),
            omit_zero: false,
            raster: None,
            manifest: None,
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
    let products = select_products(
        prepare::read_products(input, options.product.as_deref())?,
        options.product.as_deref(),
        options.layer_count,
        options.skip_analysis,
        options.min_lead_time,
    )?;
    let mut products = prepare::prepare_products(products);
    let layer_names = build_layer_names(options, products.len());

    ensure_compatible_products(&products)?;
    apply_renames(&mut products, &options.rename)?;
    apply_quantization(
        &mut products,
        &options.quantize,
        &options.omit_class,
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

    let output_format = match &options.raster {
        Some(spec) => {
            let encoding = spec.parse::<ColorEncoding>()?;
            encoding.check_bands(
                &products[0]
                    .spec
                    .band_specs
                    .iter()
                    .map(|band| band.name.clone())
                    .collect::<Vec<_>>(),
            )?;
            TileOutput::Raster(encoding)
        }
        None => TileOutput::Mvt,
    };

    match output_format {
        TileOutput::Mvt => {
            ensure_distinct_paths(input, &[output.to_path_buf()], None)?;
            write_archive(
                output,
                &products,
                &layer_names,
                options.min_zoom,
                max_zoom,
                output_format,
                None,
            )
        }
        TileOutput::Raster(_) => {
            let outputs = raster_output_paths(output, &products, options.layer_seq_start)?;
            ensure_distinct_paths(input, &outputs, options.manifest.as_deref())?;
            for (index, ((product, layer_name), output)) in products
                .iter()
                .zip(&layer_names)
                .zip(outputs.iter())
                .enumerate()
            {
                write_archive(
                    output,
                    std::slice::from_ref(product),
                    std::slice::from_ref(layer_name),
                    options.min_zoom,
                    max_zoom,
                    output_format,
                    Some(options.layer_seq_start + index),
                )?;
            }
            if let Some(manifest) = &options.manifest {
                write_manifest(
                    manifest,
                    &products,
                    &outputs,
                    output_format,
                    options.min_zoom,
                    max_zoom,
                )?;
            }
            Ok(())
        }
    }
}

/// Expands a raster output pattern into one archive path per forecast time.
/// Checks that nothing this run writes lands on the input or on another
/// output, before the first byte is written.
///
/// The manifest is written after the archives, so pointing it at one of them
/// replaced a perfectly good archive with JSON and still reported success.
/// Where an archive sits as seen from the manifest.
///
/// Relative, so that the set can be moved or served from anywhere as a unit.
/// Stripping the prefix alone only works when the archive is below the
/// manifest; an archive beside or above it needs the `..` steps spelled out, or
/// the client resolves the name against the wrong directory.
fn absolute_lexical(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("failed to resolve the current directory")?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    Ok(normalized)
}

fn relative_url(directory: &Path, output: &Path) -> Result<String> {
    let directory = absolute_lexical(if directory.as_os_str().is_empty() {
        Path::new(".")
    } else {
        directory
    })?;
    let output = absolute_lexical(output)?;
    if let Ok(below) = output.strip_prefix(&directory) {
        return Ok(below
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/"));
    }

    let shared = directory
        .components()
        .zip(output.components())
        .take_while(|(left, right)| left == right)
        .count();
    let up = directory.components().count() - shared;
    let mut url = "../".repeat(up);
    url.push_str(
        &output
            .components()
            .skip(shared)
            .map(|part| part.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
    );
    Ok(url)
}

fn ensure_distinct_paths(input: &Path, outputs: &[PathBuf], manifest: Option<&Path>) -> Result<()> {
    // Compared after resolving what exists, so that `./a.pmtiles` and
    // `a.pmtiles` are recognised as the same file.
    let resolve = |path: &Path| {
        if let Ok(existing) = path.canonicalize() {
            return existing;
        }
        // Nothing is there yet, so resolve the directory - which does exist -
        // and keep the name. A bare name means the current directory, and
        // leaving it bare would let `a.pmtiles` and `./a.pmtiles` pass as two
        // different files.
        let parent = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        match (parent.canonicalize(), path.file_name()) {
            (Ok(parent), Some(name)) => parent.join(name),
            _ => path.to_path_buf(),
        }
    };

    let mut seen = BTreeSet::new();
    let input = resolve(input);
    for (label, path) in outputs
        .iter()
        .map(|path| ("an output", path.as_path()))
        .chain(manifest.map(|path| ("the manifest", path)))
    {
        let resolved = resolve(path);
        ensure!(
            resolved != input,
            "{label} would overwrite the input {}",
            path.display()
        );
        ensure!(
            seen.insert(resolved),
            "{label} {} is written twice; give each forecast time its own path, and the \
             manifest a path of its own",
            path.display()
        );
    }
    Ok(())
}

/// Writes an index of the archives one run produced.
///
/// Animating a field means holding two forecast times at once and blending
/// between them, and a client cannot do that from a set of archives it has to
/// discover. The encoding is written once at the top: it is shared by every
/// archive here, and a reader that finds it differing per time has no way to
/// interpolate between them.
fn write_manifest(
    path: &Path,
    products: &[PreparedProduct],
    outputs: &[PathBuf],
    output_format: TileOutput,
    min_zoom: u8,
    max_zoom: u8,
) -> Result<()> {
    let directory = path.parent().unwrap_or(Path::new(""));
    let times = products
        .iter()
        .zip(outputs)
        .map(|(product, output)| -> Result<_> {
            let url = relative_url(directory, output)?;
            let mut entry = serde_json::json!({
                "url": url,
                "valid_time": product.product_id.datetime.to_rfc3339(),
                "reference_time": product.product_id.reference_datetime.to_rfc3339(),
            });
            // The extremes belong to one forecast time rather than to the set:
            // a later hour can hold a jet the first one did not.
            if let (Some(entry), TileOutput::Raster(encoding)) =
                (entry.as_object_mut(), output_format)
            {
                entry.insert(
                    "source_range".into(),
                    metadata::range_report(
                        &crate::tile::inspect_raster_range(product, encoding),
                        encoding,
                    ),
                );
            }
            Ok(entry)
        })
        .collect::<Result<Vec<_>>>()?;

    let encoding = match output_format {
        // The whole descriptor rather than a summary of it. A client blending
        // two times sets up its decode and its texture coordinates before it
        // opens any archive, and having to open one to find the rest is exactly
        // what the manifest is for avoiding.
        TileOutput::Raster(encoding) => metadata::raster_encoding(products.first(), encoding),
        TileOutput::Mvt => serde_json::Value::Null,
    };
    let manifest = serde_json::json!({
        "generator": "grib2pmtiles",
        "product": products.first().map(|product| product.spec.name.clone()),
        "minzoom": min_zoom,
        "maxzoom": max_zoom,
        "bounds": combined_bounds(products),
        "encoding": encoding,
        "times": times,
    });

    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(path, serde_json::to_vec_pretty(&manifest)?)
        .with_context(|| format!("failed to write the manifest {}", path.display()))?;
    info!(manifest = %path.display(), times = products.len(), "wrote raster manifest");
    Ok(())
}

fn raster_output_paths(
    output: &Path,
    products: &[PreparedProduct],
    sequence_start: usize,
) -> Result<Vec<PathBuf>> {
    let count = products.len();
    let Some(pattern) = output.to_str() else {
        ensure!(count == 1, "a raster output pattern must be valid UTF-8");
        return Ok(vec![output.to_path_buf()]);
    };
    let has_sequence = pattern.contains("{seq}");
    let has_valid_time = pattern.contains("{valid_time}");
    let has_reference_time = pattern.contains("{reference_time}");
    if count == 1 && !(has_sequence || has_valid_time || has_reference_time) {
        return Ok(vec![output.to_path_buf()]);
    }
    ensure!(
        count == 1 || has_sequence || has_valid_time,
        "--raster selected {count} forecast times, so the output path must contain the \
         {{seq}} or {{valid_time}} placeholder (for example, wind_{{valid_time}}.pmtiles)"
    );
    let end = sequence_start
        .checked_add(count)
        .context("the raster output sequence number overflowed")?;
    let outputs = (sequence_start..end)
        .zip(products)
        .map(|(sequence, product)| {
            PathBuf::from(
                pattern
                    .replace("{seq}", &sequence.to_string())
                    .replace(
                        "{valid_time}",
                        &product
                            .product_id
                            .datetime
                            .format("%Y%m%d%H%M%S")
                            .to_string(),
                    )
                    .replace(
                        "{reference_time}",
                        &product
                            .product_id
                            .reference_datetime
                            .format("%Y%m%d%H%M%S")
                            .to_string(),
                    ),
            )
        })
        .collect::<Vec<_>>();
    ensure!(
        outputs.iter().collect::<BTreeSet<_>>().len() == outputs.len(),
        "the raster output pattern expands to duplicate paths; add {{seq}} or {{valid_time}}"
    );
    Ok(outputs)
}

fn write_archive(
    output: &Path,
    products: &[PreparedProduct],
    layer_names: &[String],
    min_zoom: u8,
    max_zoom: u8,
    output_format: TileOutput,
    sequence: Option<usize>,
) -> Result<()> {
    // Measured over the source once, before any tile is written: saturation is
    // invisible in the output, so it has to be reported from the input.
    let range = match output_format {
        TileOutput::Raster(encoding) => products.first().map(|product| {
            let report = crate::tile::inspect_raster_range(product, encoding);
            if report.is_clipping() {
                let extremes = report
                    .extremes
                    .iter()
                    .take(encoding.bands())
                    .flatten()
                    .map(|(low, high)| format!("{low:.3}..{high:.3}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                tracing::warn!(
                    clipped = report.clipped,
                    of = report.total,
                    share = format!("{:.2}%", report.clipped_share()),
                    observed = extremes,
                    "values fall outside the encoding range and are saturated; \
                     widen --raster to keep them"
                );
            }
            report
        }),
        TileOutput::Mvt => None,
    };

    let bounds = combined_bounds(products);
    let center = ((bounds[0] + bounds[2]) / 2.0, (bounds[1] + bounds[3]) / 2.0);
    let archive_name = output
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("grib2pmtiles");
    let metadata = serde_json::to_string(&metadata::generate_metadata(
        archive_name,
        products,
        layer_names,
        bounds,
        metadata::MetadataOptions {
            min_zoom,
            max_zoom,
            output: output_format,
            sequence,
            range,
        },
    ))?;
    info!(
        layers = products.len(),
        min_zoom, max_zoom, "finished parsing GRIB2; generating PMTiles"
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
    // A PNG is already deflated; gzipping it again only costs time.
    let (tile_type, tile_compression) = match output_format {
        TileOutput::Mvt => (TileType::Mvt, Compression::Gzip),
        TileOutput::Raster(_) => (TileType::Png, Compression::None),
    };
    let mut writer = PmTilesWriter::new(tile_type)
        .tile_compression(tile_compression)
        .min_zoom(min_zoom)
        .max_zoom(max_zoom)
        .bounds(bounds[0], bounds[1], bounds[2], bounds[3])
        .center(center.0, center.1)
        .center_zoom(center_zoom(bounds).clamp(min_zoom, max_zoom))
        .metadata(&metadata)
        .create(&mut file)?;

    let (sender, receiver) = sync_channel::<(Zxy, Vec<u8>)>(32);
    std::thread::scope(|scope| -> Result<()> {
        let producer_sender = sender.clone();
        let product_refs = products;
        let layer_name_refs = layer_names;
        let producer = scope.spawn(move || {
            traverse_tile_pyramid(
                (0, 0, 0),
                product_refs,
                layer_name_refs,
                min_zoom,
                max_zoom,
                output_format,
                &producer_sender,
            )
        });
        drop(sender);

        // Tiles arrive in whatever order the worker threads finish, which put
        // them at different offsets on every run: the archives held identical
        // tiles and never matched byte for byte, so nothing downstream could
        // cache them, ship a delta of them or sign them. Collecting first and
        // writing in tile order makes the archive a function of its input.
        //
        // The channel still bounds how far ahead the producer may run; this
        // only holds the finished tiles, which the archive is about to hold
        // anyway.
        let mut tiles = Vec::new();
        while let Ok((zxy, encoded_tile)) = receiver.recv() {
            tiles.push((zxy, encoded_tile));
        }
        tiles.sort_unstable_by_key(|(zxy, _)| *zxy);
        for ((z, x, y), encoded_tile) in tiles {
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
    if let Some(spec) = &options.raster {
        spec.parse::<ColorEncoding>()?;
    }
    ensure!(
        options.manifest.is_none() || options.raster.is_some(),
        "--manifest indexes the archives --raster produces, so it needs --raster as well"
    );
    quantize::validate_syntax(&options.quantize)?;
    quantize::validate_omit_class_syntax(&options.omit_class)?;
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

/// Turns the physical `--quantize` boundaries and the omissions into the raw
/// values each forecast time stores.
///
/// Resolved against every time's own band spec. The boundaries are physical,
/// and the raw values they become depend on the packing reference and scale
/// factors, which GRIB is free to change from one message to the next.
/// Resolving once against the first time and copying the result quantized the
/// later times against the wrong numbers, so a forecast hour came out
/// differently depending on how many others were converted alongside it.
fn apply_quantization(
    products: &mut [PreparedProduct],
    args: &[String],
    omit_class_args: &[String],
    omit_zero: bool,
) -> Result<()> {
    if args.is_empty() && omit_class_args.is_empty() && !omit_zero {
        return Ok(());
    }

    for (index, product) in products.iter_mut().enumerate() {
        let band_specs = product.spec.band_specs.clone();
        let resolved = if args.is_empty() {
            vec![None; band_specs.len()]
        } else {
            quantize::resolve(args, &band_specs)?
        };
        let zero_omits = quantize::resolve_zero_omits(omit_zero, &band_specs);
        let class_omits = quantize::resolve_class_omits(omit_class_args, &resolved, &band_specs)?;

        // The specification is shared even though the raw values are not, so
        // reporting it once says everything.
        if index == 0 {
            for (band_index, band) in band_specs.iter().enumerate() {
                if let Some(quantize) = &resolved[band_index] {
                    info!(
                        band = %band.name,
                        classes = quantize.class_count(),
                        "quantizing values"
                    );
                }
                if zero_omits[band_index].is_some() {
                    info!(band = %band.name, "omitting physical zero before quantization");
                }
                if let Some(omit) = &class_omits[band_index] {
                    info!(
                        band = %band.name,
                        values = ?omit.physical(),
                        "omitting quantized classes"
                    );
                }
            }
        }

        product.spec.quantize = resolved;
        product.spec.omit_zero = zero_omits;
        product.spec.omit_class = class_omits;
    }
    Ok(())
}

trait IdentifiedProduct {
    fn product_id(&self) -> &GpvProductIdentifier;
}

impl IdentifiedProduct for PreparedProduct {
    fn product_id(&self) -> &GpvProductIdentifier {
        &self.product_id
    }
}

impl IdentifiedProduct for prepare::RawProduct {
    fn product_id(&self) -> &GpvProductIdentifier {
        &self.product_id
    }
}

fn select_products<T: IdentifiedProduct>(
    mut products: Vec<T>,
    requested_product: Option<&str>,
    layer_count: Option<usize>,
    skip_analysis: bool,
    min_lead_time: Option<i64>,
) -> Result<Vec<T>> {
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
            let product_id = product.product_id();
            (product_id.datetime - product_id.reference_datetime).num_minutes() >= minutes
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
            product.product_id().generating_process != GeneratingProcessType::Analysis
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
        left.product_id()
            .datetime
            .cmp(&right.product_id().datetime)
            .then_with(|| left.product_id().path().cmp(&right.product_id().path()))
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

fn product_selector(product: &impl IdentifiedProduct) -> String {
    let (data_kind, value_kind) = product.product_id().path_parts();
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
    let (west, east) = normalize_longitudes(bounds[0], bounds[2]);
    [west, bounds[1].max(-90.0), east, bounds[3].min(90.0)]
}

/// Brings a longitude span into the `-180..180` a client expects.
///
/// A global product carries its own frame - GSM runs `0..360` - and clamping
/// that span instead of wrapping it drops everything past the anti-meridian.
/// The archive still holds those tiles; the client simply stops asking for
/// them, because the bounds say the data ends at 180.
fn normalize_longitudes(west: f64, east: f64) -> (f64, f64) {
    // Whole-world spans are the common case here and cannot be expressed by
    // wrapping, since both edges land on the same meridian.
    if east - west >= 360.0 - f64::EPSILON {
        return (-180.0, 180.0);
    }

    let wrap = |longitude: f64| (longitude + 180.0).rem_euclid(360.0) - 180.0;
    let (west, east) = (wrap(west), wrap(east));
    if west <= east {
        (west, east)
    } else {
        // The span straddles the anti-meridian, which `[west, east]` cannot
        // describe with west below east. Widening to the world keeps every
        // tile reachable; the alternative loses one side of the seam.
        (-180.0, 180.0)
    }
}

/// A zoom at which the whole of `bounds` is on screen, for the client that
/// opens the archive without being told where to look.
///
/// Left at zero the map starts fully zoomed out, and a regional product is a
/// speck. This is the standard tile-count derivation: the span in tiles at a
/// zoom doubles with each level, so invert it and step back one.
fn center_zoom(bounds: [f64; 4]) -> u8 {
    let longitude_span = (bounds[2] - bounds[0]).abs().max(f64::MIN_POSITIVE);
    let latitude_span = (bounds[3] - bounds[1]).abs().max(f64::MIN_POSITIVE);
    let zoom = (360.0 / longitude_span)
        .min(180.0 / latitude_span)
        .log2()
        .floor();
    zoom.clamp(0.0, 22.0) as u8
}

fn traverse_tile_pyramid(
    zxy: Zxy,
    products: &[PreparedProduct],
    layer_names: &[String],
    min_zoom: u8,
    max_zoom: u8,
    output: TileOutput,
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
            .map(|(product, layer_name)| make_layer(&tile_context, product, layer_name, output))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        if !layers.is_empty() {
            let tile = match output {
                // MVT layers concatenate into one tile, and the archive stores
                // them gzipped.
                TileOutput::Mvt => {
                    let mut protobuf = Vec::with_capacity(layers.iter().map(Vec::len).sum());
                    for layer in layers {
                        protobuf.extend_from_slice(&layer);
                    }
                    let mut encoder =
                        flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                    encoder.write_all(&protobuf)?;
                    encoder.finish()?
                }
                // Raster archives hold a single product, so there is exactly
                // one image here and it is stored as it comes out.
                TileOutput::Raster(_) => layers
                    .into_iter()
                    .next()
                    .expect("the emptiness of the layers was just checked"),
            };
            sender
                .send((zxy, tile))
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
                    output,
                    sender,
                )?;
                Ok::<_, anyhow::Error>(())
            })?;
    }
    Ok(true)
}

#[cfg(test)]
mod path_tests {
    use super::*;

    /// The manifest is written after the archives, so aiming it at one of them
    /// replaced a finished archive with JSON and still reported success.
    #[test]
    fn a_manifest_may_not_land_on_an_archive() {
        let archive = PathBuf::from("wind.pmtiles");
        assert!(
            ensure_distinct_paths(Path::new("in.bin"), std::slice::from_ref(&archive), None)
                .is_ok()
        );
        assert!(
            ensure_distinct_paths(
                Path::new("in.bin"),
                std::slice::from_ref(&archive),
                Some(&archive)
            )
            .is_err()
        );
    }

    #[test]
    fn two_forecast_times_may_not_share_a_path() {
        let outputs = [
            PathBuf::from("wind.pmtiles"),
            PathBuf::from("./wind.pmtiles"),
        ];
        assert!(ensure_distinct_paths(Path::new("in.bin"), &outputs, None).is_err());
    }

    #[test]
    fn nothing_may_be_written_over_the_input() {
        let input = Path::new("in.bin");
        assert!(ensure_distinct_paths(input, &[PathBuf::from("in.bin")], None).is_err());
        assert!(
            ensure_distinct_paths(input, &[PathBuf::from("out.pmtiles")], Some(input)).is_err()
        );
    }

    /// A client resolves the url against the manifest's own directory, so an
    /// archive that is not below it needs the steps back spelled out.
    #[test]
    fn a_url_is_relative_to_the_manifest_wherever_the_archive_sits() {
        assert_eq!(
            relative_url(Path::new("out"), Path::new("out/wind_0.pmtiles")).unwrap(),
            "wind_0.pmtiles"
        );
        assert_eq!(
            relative_url(Path::new("out/manifests"), Path::new("out/wind_0.pmtiles")).unwrap(),
            "../wind_0.pmtiles"
        );
        assert_eq!(
            relative_url(Path::new("manifests"), Path::new("wind_0.pmtiles")).unwrap(),
            "../wind_0.pmtiles"
        );
        assert_eq!(
            relative_url(Path::new("a/b"), Path::new("c/wind.pmtiles")).unwrap(),
            "../../c/wind.pmtiles"
        );
    }

    #[test]
    fn relative_manifest_urls_do_not_depend_on_how_paths_were_spelled() {
        let cwd = std::env::current_dir().unwrap();
        let manifest_dir = cwd.join("out/manifests");
        let archive = cwd.join("out/wind.pmtiles");

        assert_eq!(
            relative_url(&manifest_dir, Path::new("out/wind.pmtiles")).unwrap(),
            "../wind.pmtiles"
        );
        assert_eq!(
            relative_url(Path::new("out/manifests"), &archive).unwrap(),
            "../wind.pmtiles"
        );
    }
}

#[cfg(test)]
mod bounds_tests {
    use super::*;

    /// A global product carries its own longitude frame. GSM runs `0..360`, and
    /// clamping that instead of wrapping it left bounds ending at 180: the
    /// archive held the western hemisphere and no client ever asked for it.
    #[test]
    fn a_global_span_covers_the_world_whatever_frame_it_arrived_in() {
        // GSM, half a cell either side of the full turn.
        assert_eq!(normalize_longitudes(-0.125, 359.875), (-180.0, 180.0));
        assert_eq!(normalize_longitudes(0.0, 360.0), (-180.0, 180.0));
        assert_eq!(normalize_longitudes(-180.0, 180.0), (-180.0, 180.0));
    }

    #[test]
    fn a_regional_span_keeps_its_own_edges() {
        // MSM, which is already inside the range and must not be widened.
        let (west, east) = normalize_longitudes(120.0, 150.0);
        assert!((west - 120.0).abs() < 1e-9 && (east - 150.0).abs() < 1e-9);

        // The same region expressed past the anti-meridian.
        let (west, east) = normalize_longitudes(480.0, 510.0);
        assert!((west - 120.0).abs() < 1e-9 && (east - 150.0).abs() < 1e-9);
    }

    #[test]
    fn a_span_across_the_anti_meridian_widens_rather_than_inverting() {
        // `[west, east]` cannot hold west above east, and inverting it would
        // lose one side of the seam.
        assert_eq!(normalize_longitudes(170.0, 190.0), (-180.0, 180.0));
    }

    /// Left at zero a client opens a regional archive zoomed fully out, with
    /// the data a speck in the middle.
    #[test]
    fn the_center_zoom_frames_the_data() {
        // The world.
        assert_eq!(center_zoom([-180.0, -85.0, 180.0, 85.0]), 0);
        // MSM, roughly 30 degrees of longitude over Japan.
        let msm = center_zoom([120.0, 22.0, 150.0, 48.0]);
        assert!((2..=4).contains(&msm), "MSM framed at zoom {msm}");
        // Something small enough that the zoom has to climb.
        assert!(center_zoom([139.6, 35.6, 139.8, 35.8]) > msm);
    }
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

    #[test]
    fn one_raster_time_accepts_a_plain_output_path() {
        let products = [product(0, GeneratingProcessType::Forecast)];
        assert_eq!(
            raster_output_paths(Path::new("wind.pmtiles"), &products, 0).unwrap(),
            [PathBuf::from("wind.pmtiles")]
        );
    }

    #[test]
    fn one_raster_time_expands_its_reference_time() {
        let products = [product(0, GeneratingProcessType::Forecast)];
        assert_eq!(
            raster_output_paths(Path::new("wind_{reference_time}.pmtiles"), &products, 0).unwrap(),
            [PathBuf::from("wind_20191012090500.pmtiles")]
        );
    }

    #[test]
    fn a_raster_output_pattern_expands_the_sequence() {
        let products = [
            product(0, GeneratingProcessType::Forecast),
            product(5, GeneratingProcessType::Forecast),
            product(10, GeneratingProcessType::Forecast),
        ];
        assert_eq!(
            raster_output_paths(Path::new("wind_{seq}.pmtiles"), &products, 4).unwrap(),
            [
                PathBuf::from("wind_4.pmtiles"),
                PathBuf::from("wind_5.pmtiles"),
                PathBuf::from("wind_6.pmtiles"),
            ]
        );
    }

    #[test]
    fn a_raster_output_pattern_expands_the_valid_time() {
        let products = [
            product(0, GeneratingProcessType::Forecast),
            product(5, GeneratingProcessType::Forecast),
        ];

        assert_eq!(
            raster_output_paths(
                Path::new("wind_{reference_time}_{valid_time}_{seq}.pmtiles"),
                &products,
                7,
            )
            .unwrap(),
            [
                PathBuf::from("wind_20191012090500_20191012090500_7.pmtiles"),
                PathBuf::from("wind_20191012090500_20191012091000_8.pmtiles"),
            ]
        );
    }

    #[test]
    fn several_raster_times_require_an_output_pattern() {
        let products = [
            product(0, GeneratingProcessType::Forecast),
            product(5, GeneratingProcessType::Forecast),
        ];
        let error = raster_output_paths(Path::new("wind.pmtiles"), &products, 0).unwrap_err();

        assert!(error.to_string().contains("{valid_time}"));
    }

    #[test]
    fn raster_output_paths_must_be_unique_after_expansion() {
        let products = [
            product(0, GeneratingProcessType::Analysis),
            product(0, GeneratingProcessType::Forecast),
        ];
        let error =
            raster_output_paths(Path::new("wind_{valid_time}.pmtiles"), &products, 0).unwrap_err();

        assert!(error.to_string().contains("duplicate"));
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
                omit_zero: Vec::new(),
                omit_class: Vec::new(),
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
    fn products_are_selected_before_expensive_preparation() {
        use gpv_products::products::{GpvProductElement, PointValue, ProductData};

        let selected = product(0, GeneratingProcessType::Forecast);
        let mut rejected = product(0, GeneratingProcessType::Forecast);
        rejected.product_id.kind = GpvProductElement::HiresNowcastIntensityError;

        // Preparing this point would panic because i32::MIN is the missing-value
        // sentinel. Its presence makes the test verify that rejected raw products
        // never reach prepare_product, rather than merely checking the result list.
        let rejected = prepare::RawProduct {
            product_id: rejected.product_id,
            data: ProductData {
                points: vec![PointValue {
                    x: 0,
                    y: 0,
                    point_id: 0,
                    point_power: 0,
                    band_idx: 0,
                    value: i32::MIN,
                }],
                band_specs: Vec::new(),
                grid: None,
            },
        };
        let selected = prepare::RawProduct {
            product_id: selected.product_id,
            data: ProductData::default(),
        };

        let selected = select_products(
            vec![rejected, selected],
            Some("hrnowc/intensity"),
            None,
            false,
            None,
        )
        .unwrap();
        let prepared = prepare::prepare_products(selected);

        assert_eq!(prepared.len(), 1);
        assert_eq!(
            product_selector(&prepared[0]),
            "hrnowc/intensity".to_string()
        );
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
