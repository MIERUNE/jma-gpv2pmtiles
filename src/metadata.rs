use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::model::PreparedProduct;

#[derive(Serialize)]
struct VectorLayer {
    id: String,
    minzoom: u8,
    maxzoom: u8,
    fields: BTreeMap<String, &'static str>,
}

pub(crate) fn generate_metadata(
    archive_name: &str,
    products: &[PreparedProduct],
    layer_names: &[String],
    bounds: [f64; 4],
    min_zoom: u8,
    max_zoom: u8,
) -> Map<String, Value> {
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
                .filter_map(|(band, quantize)| {
                    let quantize = quantize.as_ref()?;
                    Some((
                        band.name.clone(),
                        json!({
                            "bounds": quantize.bounds(),
                            "outputs": quantize.outputs(),
                        }),
                    ))
                })
                .collect::<Map<String, Value>>()
        })
        .unwrap_or_default();

    let mut metadata = Map::new();
    metadata.insert("name".into(), json!(archive_name));
    metadata.insert("description".into(), json!(""));
    metadata.insert("format".into(), json!("pbf"));
    metadata.insert("type".into(), json!("overlay"));
    metadata.insert("generator".into(), json!("grib2pmtiles"));
    metadata.insert("version".into(), json!("1.0.0"));
    metadata.insert("minzoom".into(), json!(min_zoom));
    metadata.insert("maxzoom".into(), json!(max_zoom));
    metadata.insert("bounds".into(), json!(bounds));
    metadata.insert("vector_layers".into(), json!(vector_layers));
    if !quantization.is_empty() {
        metadata.insert("quantization".into(), Value::Object(quantization));
    }
    metadata
}
