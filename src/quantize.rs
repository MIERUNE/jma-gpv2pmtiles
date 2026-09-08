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

    /// Class indices that emit `value`.
    fn classes_emitting(&self, value: f64) -> Vec<i32> {
        self.outputs
            .iter()
            .enumerate()
            .filter(|(_, output)| **output == value)
            .map(|(position, _)| position as i32)
            .collect()
    }

    pub fn class_count(&self) -> usize {
        self.outputs.len()
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
    split_band_prefix_for(arg, band_specs, "--quantize")
}

fn split_band_prefix_for<'a>(
    arg: &'a str,
    band_specs: &'a [BandSpec],
    option: &str,
) -> Result<(&'a str, &'a str)> {
    // A bare number never contains '=', so the prefix is unambiguous.
    if let Some((band_name, entries)) = arg.split_once('=') {
        return Ok((band_name.trim(), entries));
    }
    match band_specs {
        [band] => Ok((band.name.as_str(), arg)),
        _ => bail!(
            "{option} needs a band prefix such as {option} \"{}=1,2,4\" \
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
        .with_context(|| format!("expected a number but found {text:?}"))?;
    ensure!(value.is_finite(), "value {text:?} is not finite");
    Ok(value)
}

/// Values whose cells are left out of the tile entirely.
///
/// Values are stored in the integer space used at the pipeline stage where the
/// omission runs: raw values before quantization or class indices after it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BandOmit {
    stored: Vec<i32>,
    /// The requested physical values, kept for the metadata.
    physical: Vec<f64>,
}

impl BandOmit {
    #[inline]
    pub fn contains(&self, stored: i32) -> bool {
        self.stored.contains(&stored)
    }

    pub fn physical(&self) -> &[f64] {
        &self.physical
    }
}

/// Resolves physical zero into raw per-band values for `--omit-zero`.
///
/// The blanket flag is a no-op for a band whose encoding cannot represent
/// physical zero exactly.
pub(crate) fn resolve_zero_omits(
    omit_zero: bool,
    band_specs: &[BandSpec],
) -> Vec<Option<BandOmit>> {
    if !omit_zero {
        return vec![None; band_specs.len()];
    }

    band_specs
        .iter()
        .map(|band| {
            physical_to_raw_exact(0.0, band).map(|raw| BandOmit {
                stored: vec![raw],
                physical: vec![0.0],
            })
        })
        .collect()
}

/// Resolves `--omit-class` output values into quantized class indices.
pub(crate) fn resolve_class_omits(
    args: &[String],
    quantize: &[Option<BandQuantize>],
    band_specs: &[BandSpec],
) -> Result<Vec<Option<BandOmit>>> {
    let mut requested: Vec<Vec<f64>> = vec![Vec::new(); band_specs.len()];
    for arg in args {
        let (band_name, entries) = split_band_prefix_for(arg, band_specs, "--omit-class")?;
        let index = band_specs
            .iter()
            .position(|band| band.name == band_name)
            .with_context(|| {
                format!(
                    "unknown band {band_name:?} in --omit-class; this product has {}",
                    band_choices(band_specs)
                )
            })?;
        for entry in entries.split(',') {
            requested[index].push(parse_number(entry)?);
        }
    }

    let mut omits = vec![None; band_specs.len()];
    for (index, wanted) in requested.into_iter().enumerate() {
        if wanted.is_empty() {
            continue;
        }
        let band = &band_specs[index];
        let quantize = quantize[index].as_ref().with_context(|| {
            format!(
                "--omit-class for band {:?} requires --quantize for that band",
                band.name
            )
        })?;
        let mut stored = Vec::new();
        let mut physical = Vec::new();
        for value in wanted {
            let classes = quantize.classes_emitting(value);
            if classes.is_empty() {
                let available = quantize
                    .outputs()
                    .iter()
                    .map(|output| output.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!(
                    "--omit-class value {value} is not one of the classes of band \
                     {:?}; it emits {available}",
                    band.name
                );
            }
            physical.push(value);
            stored.extend(classes);
        }
        let distinct = stored
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        ensure!(
            distinct < quantize.class_count(),
            "--omit-class would drop every class of band {:?}",
            band.name
        );
        stored.sort_unstable();
        stored.dedup();
        omits[index] = Some(BandOmit { stored, physical });
    }
    Ok(omits)
}

/// Checks `--omit-class` syntax without needing the band specs.
pub(crate) fn validate_omit_class_syntax(args: &[String]) -> Result<()> {
    for arg in args {
        let entries = match arg.split_once('=') {
            Some((_, entries)) => entries,
            None => arg,
        };
        for entry in entries.split(',') {
            parse_number(entry)?;
        }
    }
    Ok(())
}

/// Converts a physical value to its raw value, or `None` if it is not on the grid.
fn physical_to_raw_exact(value: f64, band: &BandSpec) -> Option<i32> {
    let scaled = value * 10f64.powi(band.decimal_scale as i32);
    let raw = (scaled - band.reference_value as f64) / 2f64.powi(band.binary_scale as i32);
    let rounded = raw.round();
    ((raw - rounded).abs() < 1e-6 && rounded.abs() < i32::MAX as f64).then_some(rounded as i32)
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

    fn class_omits_for(
        spec: &str,
        omit: &[&str],
        bands: &[BandSpec],
    ) -> Result<Vec<Option<BandOmit>>> {
        let quantize = resolve(&[spec.to_string()], bands)?;
        let args = omit.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        resolve_class_omits(&args, &quantize, bands)
    }

    #[test]
    fn omitting_a_class_covers_only_that_class() {
        let omits = class_omits_for("0:0,1:1,2:2", &["0"], &[band("value")]).unwrap();
        let omit = omits[0].as_ref().unwrap();

        assert!(omit.contains(0));
        assert!(!omit.contains(1));
        assert!(!omit.contains(2));
        assert_eq!(omit.physical(), [0.0]);
    }

    #[test]
    fn nothing_is_omitted_without_the_options() {
        let omits = class_omits_for("0,1,2", &[], &[band("value")]).unwrap();

        assert!(omits[0].is_none());
    }

    #[test]
    fn several_values_can_be_omitted_at_once() {
        let omits = class_omits_for("0:0,1:1,2:2", &["0,2"], &[band("value")]).unwrap();
        let omit = omits[0].as_ref().unwrap();

        assert!(omit.contains(0) && omit.contains(2));
        assert!(!omit.contains(1));
    }

    #[test]
    fn omitting_a_value_that_is_not_a_class_is_rejected() {
        let error = class_omits_for("0:0,1:1", &["5"], &[band("value")]).unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("not one of the classes"), "got: {message}");
        assert!(
            message.contains("0, 1"),
            "should list the classes: {message}"
        );
    }

    #[test]
    fn omitting_every_class_is_rejected() {
        let error = class_omits_for("0:0,1:1", &["0,1"], &[band("value")]).unwrap_err();

        assert!(error.to_string().contains("every class"), "got: {error:#}");
    }

    #[test]
    fn omit_class_requires_quantization() {
        let error = resolve_class_omits(&["0".to_string()], &[None], &[band("value")]).unwrap_err();

        assert!(error.to_string().contains("requires --quantize"));
    }

    #[test]
    fn omit_zero_applies_to_every_band() {
        let bands = [band("u"), band("v")];

        let omits = resolve_zero_omits(true, &bands);

        assert!(omits[0].as_ref().unwrap().contains(0));
        assert!(omits[1].as_ref().unwrap().contains(0));
    }

    #[test]
    fn omit_zero_is_independent_of_quantized_outputs() {
        let omits = resolve_zero_omits(true, &[band("value")]);

        assert!(omits[0].as_ref().unwrap().contains(0));
    }

    #[test]
    fn omit_zero_is_a_no_op_when_physical_zero_is_not_representable() {
        let band = BandSpec {
            name: "value".to_string(),
            reference_value: 0.5,
            ..Default::default()
        };

        assert!(resolve_zero_omits(true, &[band])[0].is_none());
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
