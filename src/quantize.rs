//! Optional value quantization.
//!
//! Quantization collapses band values into a small number of classes *before*
//! the polygons are built. `tile::make_polygons_grouped_by_value` groups cells
//! by value and unions each group, so fewer distinct values means adjacent
//! cells merge into larger polygons. That is where a tile's bytes actually
//! live: geometry, not attributes.
//!
//! A specification is a comma separated list of `<boundary>[:<value>]` entries,
//! optionally prefixed with `<band>=`. Boundaries are inclusive lower bounds in
//! physical units, and each class emits its own boundary unless a replacement
//! value is given. The last class is open ended, and values below the first
//! boundary fall into the first class.

use anyhow::{Context, Result, bail, ensure};
use gpv_products::model::BandSpec;

/// A `--quantize` specification resolved against a band spec.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BandQuantize {
    /// Inclusive lower bound of each class, in raw (unscaled) value space.
    raw_bounds: Vec<i32>,
    /// Inclusive lower bound of each class in physical units, kept for metadata.
    bounds: Vec<f64>,
    /// Value emitted for each class, in physical units.
    outputs: Vec<f64>,
    /// Every output is an integer, so the attribute can be emitted as `sint`.
    integral_outputs: bool,
}

impl BandQuantize {
    /// Class index of a raw value.
    ///
    /// Values below the first boundary fall into the first class, and the last
    /// class is open ended, so every input maps to a class.
    #[inline]
    pub fn class_of(&self, raw: i32) -> i32 {
        let above = self.raw_bounds.partition_point(|bound| *bound <= raw);
        above.saturating_sub(1) as i32
    }

    /// Physical value emitted for a class index produced by [`Self::class_of`].
    #[inline]
    pub fn output(&self, class: i32) -> f64 {
        self.outputs[class as usize]
    }

    /// Whether every class emits an integer.
    ///
    /// This is a property of the specification, not of any single value, so the
    /// attribute keeps one MVT value type across the whole tileset.
    #[inline]
    pub fn integral_outputs(&self) -> bool {
        self.integral_outputs
    }

    pub fn bounds(&self) -> &[f64] {
        &self.bounds
    }

    pub fn outputs(&self) -> &[f64] {
        &self.outputs
    }
}

/// Resolves the raw `--quantize` arguments against the bands of the product.
///
/// Returns one entry per band, in band order.
pub(crate) fn resolve(
    args: &[String],
    band_specs: &[BandSpec],
) -> Result<Vec<Option<BandQuantize>>> {
    let mut resolved: Vec<Option<BandQuantize>> = vec![None; band_specs.len()];
    for arg in args {
        let (band_name, entries) = split_band_prefix(arg, band_specs)?;
        let index = band_specs
            .iter()
            .position(|band| band.name == band_name)
            .with_context(|| {
                format!(
                    "unknown band {band_name:?} in --quantize; this product has {}",
                    band_choices(band_specs)
                )
            })?;
        ensure!(
            resolved[index].is_none(),
            "band {band_name:?} is quantized more than once"
        );
        resolved[index] = Some(parse_entries(entries, &band_specs[index])?);
    }
    Ok(resolved)
}

/// Splits an optional `<band>=` prefix, defaulting to the only band.
fn split_band_prefix<'a>(arg: &'a str, band_specs: &'a [BandSpec]) -> Result<(&'a str, &'a str)> {
    // A bare number never contains '=', so the prefix is unambiguous.
    if let Some((band_name, entries)) = arg.split_once('=') {
        return Ok((band_name.trim(), entries));
    }
    match band_specs {
        [band] => Ok((band.name.as_str(), arg)),
        _ => bail!(
            "--quantize needs a band prefix such as --quantize \"{}=1,2,4\" \
             because this product has {}",
            band_specs
                .first()
                .map(|band| band.name.as_str())
                .unwrap_or("band"),
            band_choices(band_specs)
        ),
    }
}

fn band_choices(band_specs: &[BandSpec]) -> String {
    if band_specs.is_empty() {
        return "no bands".to_string();
    }
    let names = band_specs
        .iter()
        .map(|band| format!("{:?}", band.name))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{} band(s): {names}", band_specs.len())
}

/// Checks the syntax of every specification without needing the band specs.
///
/// Resolving needs the product, which is only known after the whole GRIB2 file
/// has been parsed, so a typo would otherwise surface minutes into a run.
pub(crate) fn validate_syntax(args: &[String]) -> Result<()> {
    for arg in args {
        let entries = match arg.split_once('=') {
            Some((_, entries)) => entries,
            None => arg,
        };
        parse_entries_unscaled(entries)?;
    }
    Ok(())
}

fn parse_entries(entries: &str, band: &BandSpec) -> Result<BandQuantize> {
    let (bounds, outputs) = parse_entries_unscaled(entries)?;

    let raw_bounds = bounds
        .iter()
        .map(|bound| physical_to_raw(*bound, band))
        .collect::<Vec<_>>();
    // Two boundaries collapsing onto the same raw value would leave a class
    // that no value can reach, which the metadata would still advertise.
    for window in raw_bounds.windows(2) {
        ensure!(
            window[0] < window[1],
            "--quantize boundaries are finer than the resolution of band {:?}: \
             two boundaries both map to raw value {}",
            band.name,
            window[0]
        );
    }

    let integral_outputs = outputs
        .iter()
        .all(|output| output.fract() == 0.0 && output.abs() <= i64::MAX as f64);
    Ok(BandQuantize {
        raw_bounds,
        bounds,
        outputs,
        integral_outputs,
    })
}

/// Parses the boundary/value list, leaving the band scaling out of it.
fn parse_entries_unscaled(entries: &str) -> Result<(Vec<f64>, Vec<f64>)> {
    let mut bounds = Vec::new();
    let mut outputs = Vec::new();
    for entry in entries.split(',') {
        let entry = entry.trim();
        ensure!(
            !entry.is_empty(),
            "--quantize contains an empty entry; expected <boundary>[:<value>]"
        );
        let (bound, output) = match entry.split_once(':') {
            Some((bound, output)) => (parse_number(bound)?, parse_number(output)?),
            // Without a replacement value the class emits its own boundary.
            None => {
                let bound = parse_number(entry)?;
                (bound, bound)
            }
        };
        if let Some(previous) = bounds.last() {
            ensure!(
                bound > *previous,
                "--quantize boundaries must be strictly increasing, but {bound} follows {previous}"
            );
        }
        bounds.push(bound);
        outputs.push(output);
    }
    ensure!(!bounds.is_empty(), "--quantize needs at least one boundary");
    Ok((bounds, outputs))
}

fn parse_number(text: &str) -> Result<f64> {
    let text = text.trim();
    let value = text
        .parse::<f64>()
        .with_context(|| format!("--quantize expected a number but found {text:?}"))?;
    ensure!(value.is_finite(), "--quantize value {text:?} is not finite");
    Ok(value)
}

/// Converts a physical boundary into the smallest raw value that belongs to it.
///
/// The forward direction is `(reference + raw * 2^binary_scale) * 10^-decimal_scale`,
/// which is monotonically increasing because both scales are positive, so the
/// inverse below is well defined.
fn physical_to_raw(bound: f64, band: &BandSpec) -> i32 {
    let scaled = bound * 10f64.powi(band.decimal_scale as i32);
    let raw = (scaled - band.reference_value as f64) / 2f64.powi(band.binary_scale as i32);
    // A boundary that sits on the raw grid must stay on it. Inverting the
    // scales can miss by a few ULP, and a bare `ceil` would then push the
    // boundary a whole step up.
    let snapped = if (raw - raw.round()).abs() < 1e-6 {
        raw.round()
    } else {
        raw.ceil()
    };
    // `i32::MIN` is reserved as the missing-value marker by `CompactOptI32`.
    snapped.clamp(i32::MIN as f64 + 1.0, i32::MAX as f64) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn band(name: &str) -> BandSpec {
        BandSpec {
            name: name.to_string(),
            ..Default::default()
        }
    }

    fn scaled_band(name: &str, decimal_scale: i8) -> BandSpec {
        BandSpec {
            name: name.to_string(),
            decimal_scale,
            ..Default::default()
        }
    }

    fn resolve_one(spec: &str, band: BandSpec) -> Result<BandQuantize> {
        Ok(resolve(&[spec.to_string()], &[band])?
            .pop()
            .flatten()
            .expect("band should be quantized"))
    }

    #[test]
    fn a_boundary_without_a_replacement_emits_itself() {
        let quantize = resolve_one("1,2,4", band("value")).unwrap();

        assert_eq!(quantize.outputs(), [1.0, 2.0, 4.0]);
        assert!(quantize.integral_outputs());
    }

    #[test]
    fn a_replacement_value_overrides_the_boundary() {
        let quantize = resolve_one("1:0.5,2:1.5,4:3", band("value")).unwrap();

        assert_eq!(quantize.bounds(), [1.0, 2.0, 4.0]);
        assert_eq!(quantize.outputs(), [0.5, 1.5, 3.0]);
        // A fractional replacement must not be advertised as an integer column.
        assert!(!quantize.integral_outputs());
    }

    #[test]
    fn classes_cover_every_value_including_the_open_ends() {
        let quantize = resolve_one("1,2,4", band("value")).unwrap();

        // Below the first boundary falls into the first class.
        assert_eq!(quantize.class_of(i32::MIN + 1), 0);
        assert_eq!(quantize.class_of(0), 0);
        // Boundaries are inclusive lower bounds.
        assert_eq!(quantize.class_of(1), 0);
        assert_eq!(quantize.class_of(2), 1);
        assert_eq!(quantize.class_of(3), 1);
        assert_eq!(quantize.class_of(4), 2);
        // The last class is open ended.
        assert_eq!(quantize.class_of(i32::MAX), 2);
    }

    #[test]
    fn boundaries_are_converted_into_raw_value_space() {
        // decimal_scale 1 means the raw values are tenths.
        let quantize = resolve_one("1,2,4.5", scaled_band("value", 1)).unwrap();

        // 1.0 mm/h is raw 10, not raw 11: inverting 10^1 must not drift upwards.
        assert_eq!(quantize.class_of(9), 0);
        assert_eq!(quantize.class_of(10), 0);
        assert_eq!(quantize.class_of(19), 0);
        assert_eq!(quantize.class_of(20), 1);
        assert_eq!(quantize.class_of(44), 1);
        assert_eq!(quantize.class_of(45), 2);
    }

    #[test]
    fn boundaries_must_be_strictly_increasing() {
        let error = resolve_one("1,2,2", band("value")).unwrap_err();

        assert!(
            error.to_string().contains("strictly increasing"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn boundaries_finer_than_the_band_resolution_are_rejected() {
        // With decimal_scale 0 the raw grid is whole units, so 1.2 and 1.4 both
        // land on raw 2.
        let error = resolve_one("1.2,1.4", band("value")).unwrap_err();

        assert!(
            error.to_string().contains("finer than the resolution"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_multi_band_product_requires_a_band_prefix() {
        let bands = [band("u"), band("v")];

        let error = resolve(&["1,2".to_string()], &bands).unwrap_err();

        assert!(
            error.to_string().contains("needs a band prefix"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_band_prefix_selects_the_band_to_quantize() {
        let bands = [band("u"), band("v")];

        let resolved = resolve(&["v=1,2".to_string()], &bands).unwrap();

        assert!(resolved[0].is_none());
        assert_eq!(resolved[1].as_ref().unwrap().outputs(), [1.0, 2.0]);
    }

    #[test]
    fn an_unknown_band_lists_the_available_ones() {
        let bands = [band("u"), band("v")];

        let error = resolve(&["w=1,2".to_string()], &bands).unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("unknown band"), "unexpected: {message}");
        assert!(message.contains("\"u\""), "unexpected: {message}");
    }

    #[test]
    fn a_band_cannot_be_quantized_twice() {
        let bands = [band("u"), band("v")];

        let error = resolve(&["u=1,2".to_string(), "u=3,4".to_string()], &bands).unwrap_err();

        assert!(
            error.to_string().contains("more than once"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_non_numeric_entry_is_rejected() {
        let error = resolve_one("1,abc", band("value")).unwrap_err();

        assert!(
            format!("{error:#}").contains("expected a number"),
            "unexpected error: {error:#}"
        );
    }
}
