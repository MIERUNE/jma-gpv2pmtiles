use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::{
    model::PreparedProduct,
    tile::{ColorEncoding, RangeReport, TileOutput},
};

#[derive(Serialize)]
struct VectorLayer {
    id: String,
    minzoom: u8,
    maxzoom: u8,
    fields: BTreeMap<String, &'static str>,
}

#[derive(Clone, Copy)]
pub(crate) struct MetadataOptions {
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub output: TileOutput,
    pub sequence: Option<usize>,
    /// How the source values sat inside the encoding's range.
    pub range: Option<RangeReport>,
}

/// Version of the `raster_encoding` object.
///
/// Bumped whenever a client that understood the previous shape would read this
/// one wrongly rather than merely miss something. It moved from code-based to
/// texel-based coefficients before this existed, which is exactly the kind of
/// change a reader cannot detect on its own.
const RASTER_ENCODING_SCHEMA_VERSION: u32 = 1;

/// The level the field sits on, as GRIB2 recorded it.
///
/// A client showing wind has to know whether it is looking at the surface or at
/// 850 hPa, and nothing else in the archive says so.
fn level_descriptor(product: &PreparedProduct) -> Value {
    use gpv_products::products::FixedSurface;

    // The scale factor is a negative power of ten, the way GRIB2 stores it.
    let scaled = |value: u32, factor: i8| f64::from(value) * 10f64.powi(-i32::from(factor));

    match product.product_id.surface {
        FixedSurface::None => Value::Null,
        FixedSurface::Surface => json!({ "type": "surface" }),
        FixedSurface::Msl => json!({ "type": "mean-sea-level" }),
        FixedSurface::Altitude(value, factor) => json!({
            "type": "height", "value": scaled(value, factor), "unit": "m",
        }),
        FixedSurface::IsobaricSurface(value, factor) => json!({
            "type": "isobaric", "value": scaled(value, factor), "unit": "Pa",
        }),
        FixedSurface::DepthBelowSeaLevel(value, factor) => json!({
            "type": "depth-below-sea-level", "value": scaled(value, factor), "unit": "m",
        }),
        FixedSurface::CloudTops => json!({ "type": "cloud-tops" }),
        FixedSurface::TankTotal => json!({ "type": "tank-total" }),
        FixedSurface::Tank(value) => json!({ "type": "tank", "value": value }),
    }
}

/// Everything a client needs to decode a raster tile, save what differs
/// between forecast times.
///
/// Shared by the archive metadata and by the manifest an animation reads: a
/// client blending two times has to configure its decode and its texture
/// coordinates before opening any archive, so the manifest cannot carry a
/// summary of this and expect the client to find the rest.
pub(crate) fn raster_encoding(product: Option<&PreparedProduct>, encoding: ColorEncoding) -> Value {
    let mut raster = encoding.descriptor();
    let Some(object) = raster.as_object_mut() else {
        return raster;
    };

    object.insert(
        "schema_version".into(),
        json!(RASTER_ENCODING_SCHEMA_VERSION),
    );
    // How the pixels are stored, kept apart from what they mean: a different
    // container would leave the decode rule standing.
    object.insert("container".into(), json!("png"));
    object.insert("tile_size".into(), json!(crate::tile::RASTER_TILE_EXTENT));
    // The stored image is larger than the tile: a client samples the inner
    // square and lets the filter reach into the margin.
    object.insert("buffer".into(), json!(crate::tile::RASTER_TILE_BUFFER));
    object.insert("image_size".into(), json!(crate::tile::RASTER_IMAGE_EXTENT));

    if let Some(product) = product {
        object.insert(
            "bands".into(),
            json!(
                product.spec.band_specs[..encoding.bands()]
                    .iter()
                    .map(|band| band.name.as_str())
                    .collect::<Vec<_>>()
            ),
        );
        object.insert("sampling".into(), sampling_contract(product));
        object.insert("level".into(), level_descriptor(product));
    }
    if let ColorEncoding::VectorField { .. } = encoding {
        // Which way each component points, so that a client need not infer it
        // from the band names. The order is fixed by the encoding, which reads
        // `u` before `v`.
        object.insert("components".into(), json!(["eastward", "northward"]));
        // Every vector field these products carry - wind, current, drift - is a
        // velocity in metres per second.
        object.insert("units".into(), json!("m/s"));
    }
    raster
}

/// What the source values were, beside the range the archive can hold.
///
/// Saturation cannot be seen in the tiles: a value above the limit is written
/// as the limit, and reads back as a real measurement. Recording the extremes
/// lets a reader tell one from the other, and lets the next run pick a range
/// that holds them.
pub(crate) fn range_report(range: &RangeReport, encoding: ColorEncoding) -> Value {
    let extremes = range
        .extremes
        .iter()
        .take(encoding.bands())
        .map(|extreme| match extreme {
            Some((low, high)) => json!({ "min": low, "max": high }),
            None => Value::Null,
        })
        .collect::<Vec<_>>();

    json!({
        "bands": extremes,
        "values": range.total,
        "clipped_count": range.clipped,
        "clipped_share_percent": range.clipped_share(),
    })
}

/// How the tile has to be uploaded and sampled for the decode rule to hold.
///
/// These are not suggestions. A colour-space conversion rewrites every channel,
/// premultiplication zeroes the channels of a transparent pixel, and mipmaps
/// average codes across zoom levels; each destroys the values outright, and
/// none of them announces itself.
fn sampling_contract(product: &PreparedProduct) -> Value {
    // Quantized values are class indices, and the classes between two of them
    // do not exist. Filtering would invent them, so such a tile has to be read
    // back exactly as it was written.
    let quantized = product.spec.quantize.iter().any(Option::is_some);

    json!({
        "sample_space": "normalized-texel",
        "texture_format": "rgba8unorm",
        "filter": if quantized { "nearest" } else { "linear" },
        "color_space_conversion": "none",
        "premultiply_alpha": false,
        "mipmaps": false,
    })
}

pub(crate) fn generate_metadata(
    archive_name: &str,
    products: &[PreparedProduct],
    layer_names: &[String],
    bounds: [f64; 4],
    options: MetadataOptions,
) -> Map<String, Value> {
    let MetadataOptions {
        min_zoom,
        max_zoom,
        output,
        sequence,
        range,
    } = options;
    let vector_layers = products
        .iter()
        .zip(layer_names)
        .map(|(product, layer_name)| VectorLayer {
            id: layer_name.clone(),
            minzoom: min_zoom,
            maxzoom: max_zoom,
            fields: product
                .spec
                .band_specs
                .iter()
                .map(|band| (band.name.clone(), "Number"))
                .collect(),
        })
        .collect::<Vec<_>>();

    // Quantization is lossy, so record it: the values are class representatives,
    // which a consumer cannot tell apart from measurements otherwise.
    let quantization = products
        .first()
        .map(|product| {
            product
                .spec
                .band_specs
                .iter()
                .zip(&product.spec.quantize)
                .zip(&product.spec.omit_class)
                .filter_map(|((band, quantize), omit_class)| {
                    let quantize = quantize.as_ref()?;
                    Some((
                        band.name.clone(),
                        json!({
                            "bounds": quantize.bounds(),
                            "outputs": quantize.outputs(),
                            "omitted_classes": omit_class.as_ref().map(|omit| omit.physical()).unwrap_or(&[]),
                        }),
                    ))
                })
                .collect::<Map<String, Value>>()
        })
        .unwrap_or_default();
    let omitted_input_values = products
        .first()
        .map(|product| {
            product
                .spec
                .band_specs
                .iter()
                .zip(&product.spec.omit_zero)
                .filter_map(|(band, omit)| {
                    Some((band.name.clone(), json!(omit.as_ref()?.physical())))
                })
                .collect::<Map<String, Value>>()
        })
        .unwrap_or_default();

    let mut metadata = Map::new();
    metadata.insert("name".into(), json!(archive_name));
    metadata.insert("description".into(), json!(""));
    metadata.insert(
        "format".into(),
        json!(match output {
            TileOutput::Mvt => "pbf",
            TileOutput::Raster(_) => "png",
        }),
    );
    metadata.insert("type".into(), json!("overlay"));
    metadata.insert("generator".into(), json!("grib2pmtiles"));
    metadata.insert("version".into(), json!("1.0.0"));
    metadata.insert("minzoom".into(), json!(min_zoom));
    metadata.insert("maxzoom".into(), json!(max_zoom));
    metadata.insert("bounds".into(), json!(bounds));
    match output {
        // A raster archive has no attributes to describe, but it does need to
        // say how a pixel turns back into a value: the parameters are chosen
        // per archive and a client cannot recover them from the tiles.
        TileOutput::Raster(encoding) => {
            let mut raster = raster_encoding(products.first(), encoding);
            if let (Some(object), Some(range)) = (raster.as_object_mut(), range) {
                // The only part that belongs to one archive rather than to the
                // set: what the source values of this forecast time were.
                object.insert("source_range".into(), range_report(&range, encoding));
            }
            metadata.insert("raster_encoding".into(), raster);
            if let Some(product) = products.first() {
                metadata.insert("product".into(), json!(product.spec.name));
                metadata.insert(
                    "reference_time".into(),
                    json!(product.product_id.reference_datetime.to_rfc3339()),
                );
                metadata.insert(
                    "valid_time".into(),
                    json!(product.product_id.datetime.to_rfc3339()),
                );
            }
            if let Some(sequence) = sequence {
                metadata.insert("sequence".into(), json!(sequence));
            }
        }
        TileOutput::Mvt => {
            metadata.insert("vector_layers".into(), json!(vector_layers));
        }
    }
    if !quantization.is_empty() {
        metadata.insert("quantization".into(), Value::Object(quantization));
    }
    if !omitted_input_values.is_empty() {
        metadata.insert(
            "omitted_input_values".into(),
            Value::Object(omitted_input_values),
        );
    }
    metadata
}
