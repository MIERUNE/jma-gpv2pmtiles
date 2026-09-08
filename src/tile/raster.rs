//! Encodes band values into RGBA raster tiles.
//!
//! A raster tile carries data rather than a picture, so the client has to know
//! how to invert whatever was done to it. Every [`ColorEncoding`] therefore
//! ships a [`ColorEncoding::descriptor`] into the archive metadata. Its
//! structured channel, offset and scale fields are sufficient to invert the
//! pixels; no layer has to guess from a tileset name.
//!
//! Two hazards apply to every encoding here, and both are quiet when they bite:
//!
//! * **Alpha is never a mask.** Several browser image-decoding paths -
//!   `createImageBitmap` among them - premultiply alpha, which zeroes the
//!   colour channels of a transparent pixel. That would destroy exactly the
//!   values the mask is marking. Tiles are always opaque, and coverage is
//!   spelled out in a colour channel instead.
//! * **Parameters belong to the archive, never to a tile.** Deriving a range
//!   per tile would make neighbouring tiles decode the same byte differently,
//!   and the field would jump across every tile seam.

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use crate::{
    geo::web_mercator_to_lnglat,
    model::TilesetSpec,
    tile::{BandScale, Point, PointMap, Zxy},
};

/// Side length in pixels of a raster tile.
pub(crate) const TILE_EXTENT: u32 = 512;

/// How the band values of one grid cell become a pixel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ColorEncoding {
    /// A scalar spread over all 24 colour bits. Exact to `interval`, at the
    /// cost of having no channel left for a mask, so it suits a grid that
    /// covers every tile it appears in - a global model, or terrain.
    ///
    /// The layout is the one Mapbox terrain-RGB uses, and `base = -10000` with
    /// `interval = 0.1` reproduces it byte for byte, so an existing terrain-RGB
    /// decoder reads these tiles unchanged.
    ///
    /// ```glsl
    /// float value = dot(texel.rgb, uWeights) + uOffset;   // no mask
    /// ```
    ///
    /// Spending all 24 bits also spends the precision of the `highp float` the
    /// client decodes into. Measured with `interval = 0.1`, reconstruction in
    /// `f32` is worst by 0.175 - nearly two intervals - once the codes run to
    /// `2^24`, and by 0.003 over the span terrain-RGB was drawn for. So this is
    /// exact to `interval` in the encoding, but only about that in a shader,
    /// and only while the values stay in a sensible range. A field that needs
    /// both a wide range and a fine step wants [`Self::Scalar16`] over a
    /// tighter one.
    Scalar24 { base: f64, interval: f64 },

    /// A scalar in 16 bits across red and green, with the on-grid flag in blue.
    ///
    /// Coarser than [`Self::Scalar24`] but it keeps a channel free for the
    /// mask, which is what a regional model such as MSM or LFM needs: the grid
    /// stops partway across the tile, and a sentinel value cannot say so
    /// reliably once the client filters the texture bilinearly.
    ///
    /// ```glsl
    /// float value  = dot(texel.rg, uWeights) + uOffset;
    /// float onGrid = step(uThreshold, texel.b);
    /// ```
    Scalar16 { min: f64, max: f64 },

    /// The two components of a vector, signed-normalised into red and green
    /// with the on-grid flag in blue. This is the one a particle layer wants:
    /// it recovers a velocity in three instructions from one scalar uniform.
    ///
    /// ```glsl
    /// vec4  texel    = texture2D(uField, uv);
    /// vec2  velocity = texel.rg * uScale + uOffset;
    /// float onGrid   = step(uThreshold, texel.b);
    /// ```
    VectorField {
        /// The largest value **either component** can hold, not the largest
        /// speed: a field at the limit in both directions has a magnitude of
        /// `component_limit * sqrt(2)`.
        component_limit: f64,
    },
}

impl std::str::FromStr for ColorEncoding {
    type Err = anyhow::Error;

    /// Parses `<encoding>:<parameters>`, the form `--raster` takes.
    ///
    /// The parameters are part of the name rather than separate flags because
    /// they differ per encoding: a speed limit means nothing to a scalar, and a
    /// pair of separate flags would let the two be combined into a request that
    /// cannot be served.
    fn from_str(spec: &str) -> Result<Self> {
        let (name, parameters) = spec.split_once(':').unwrap_or((spec, ""));
        let numbers = |expected: usize| -> Result<Vec<f64>> {
            let values = parameters
                .split(',')
                .filter(|part| !part.trim().is_empty())
                .map(|part| {
                    part.trim()
                        .parse::<f64>()
                        .with_context(|| format!("{part:?} is not a number"))
                })
                .collect::<Result<Vec<_>>>()?;
            ensure!(
                values.len() == expected,
                "{name:?} takes {expected} parameter(s), but {} were given in {spec:?}",
                values.len()
            );
            ensure!(
                values.iter().all(|value| value.is_finite()),
                "the parameters of {name:?} have to be finite"
            );
            Ok(values)
        };

        match name {
            "vector-field" => {
                let [component_limit] = numbers(1)?[..] else {
                    unreachable!("the count was checked")
                };
                ensure!(
                    component_limit > 0.0,
                    "the component limit of {name:?} has to be positive, but was \
                     {component_limit}"
                );
                Ok(Self::VectorField { component_limit })
            }
            "scalar16" => {
                let [min, max] = numbers(2)?[..] else {
                    unreachable!("the count was checked")
                };
                ensure!(
                    min < max,
                    "the range of {name:?} has to be increasing, but was {min},{max}"
                );
                Ok(Self::Scalar16 { min, max })
            }
            "scalar24" => {
                let [base, interval] = numbers(2)?[..] else {
                    unreachable!("the count was checked")
                };
                ensure!(
                    interval > 0.0,
                    "the interval of {name:?} has to be positive, but was {interval}"
                );
                Ok(Self::Scalar24 { base, interval })
            }
            _ => bail!(
                "unknown raster encoding {name:?}. Expected one of \
                 vector-field:<component-limit>, scalar16:<min>,<max>, scalar24:<base>,<interval>"
            ),
        }
    }
}

/// The byte a [`ColorEncoding::VectorField`] component decodes to zero at.
const ZERO_CODE: f64 = 128.0;

/// Codes either side of [`ZERO_CODE`]. One code at the bottom is given up so
/// that zero is representable at all; the `SNORM` formats make the same trade.
const SIGNED_CODES: f64 = 127.0;

/// How a client tells an on-grid pixel from an off-grid one.
///
/// Expressed against the sampled texel rather than the byte, like every other
/// coefficient here, and as a threshold rather than an equality: a sampler
/// filtering across the edge of the grid returns values between the two, and
/// only a threshold gives a clean boundary. Comparing against 255 would be
/// false for every texel a shader ever sees.
/// The space the coefficients are written against: what a `sampler2D` returns
/// for a UNORM texture, so a client never has to know the channel is 8 bits.
///
/// It sits beside the encoding rather than inside the decode rule because it
/// describes the numbers the client feeds in, not the arithmetic applied to
/// them. Keeping the logical description separate from how the tile happens to
/// be stored is what lets the container change later without the client's
/// decode changing with it.
const TEXEL_SAMPLE_SPACE: &str = "normalized-texel";

fn mask_descriptor() -> Value {
    json!({
        "kind": "channel-gte",
        "channel": "b",
        "threshold": 0.5,
    })
}

/// Turns a texel, which a sampler hands the shader in `0..1`, back into the
/// byte that was written. Folding it into the published coefficients is what
/// keeps this constant out of the client: nothing there has to know that a
/// channel is eight bits wide.
const BYTE_CODES: f64 = 255.0;

/// The number of codes [`ColorEncoding::Scalar16`] spreads its range over.
const SCALAR16_CODES: f64 = ((1u32 << 16) - 1) as f64;

/// The largest value [`ColorEncoding::Scalar24`] can hold.
const SCALAR24_CODES: f64 = ((1u32 << 24) - 1) as f64;

impl ColorEncoding {
    /// How many bands the encoding consumes, in band order.
    pub(crate) fn bands(&self) -> usize {
        match self {
            Self::Scalar24 { .. } | Self::Scalar16 { .. } => 1,
            Self::VectorField { .. } => 2,
        }
    }

    /// The band names this encoding means, in the order it reads them.
    ///
    /// `None` where any single band will do; a scalar is a scalar whatever it
    /// is called.
    fn required_bands(&self) -> Option<&'static [&'static str]> {
        match self {
            Self::Scalar24 { .. } | Self::Scalar16 { .. } => None,
            // East-west first, north-south second, which is the order the
            // components are read in and the one the GRIB2 products use.
            Self::VectorField { .. } => Some(&["u", "v"]),
        }
    }

    /// Checks that a product's bands are the ones this encoding means, rather
    /// than merely enough of them.
    ///
    /// Taking whatever the first bands happen to be would encode a wave
    /// product's height and direction as a velocity. Nothing downstream could
    /// notice: the tiles would be well-formed, and the particles would simply
    /// flow the wrong way.
    pub(crate) fn check_bands(&self, names: &[String]) -> Result<()> {
        let bands = self.bands();
        if let Some(required) = self.required_bands() {
            ensure!(
                names.len() == required.len()
                    && names.iter().zip(required).all(|(name, want)| name == want),
                "this encoding reads {required:?} in that order, but the product has {names:?}. \
                 Use --rename to line the names up if the product carries the same quantities \
                 under other names."
            );
        } else {
            ensure!(
                names.len() == bands,
                "this encoding reads a single band, but the product has {}: {names:?}. Split the \
                 product, or pick an encoding that reads them all.",
                names.len()
            );
        }
        Ok(())
    }

    /// Whether encoding these values would saturate rather than represent them.
    fn clips(&self, values: [f64; 2]) -> bool {
        match *self {
            Self::Scalar24 { base, interval } => {
                // Zero is a value here, not a reserved code: this encoding has
                // no mask, so `base` itself has to be representable. Counting
                // it as clipped reported every cell sitting exactly on the
                // bottom of the range - which for, say, a precipitation field
                // is most of them.
                let code = (values[0] - base) / interval;
                !code.is_finite() || code < 0.0 || code > SCALAR24_CODES
            }
            Self::Scalar16 { min, max } => {
                !values[0].is_finite() || values[0] < min || values[0] > max
            }
            Self::VectorField { component_limit } => values[..self.bands()]
                .iter()
                .any(|value| !value.is_finite() || value.abs() > component_limit),
        }
    }

    /// The pixel written where the grid has no value.
    ///
    /// Always opaque. For a vector field the components also sit at zero, so a
    /// client that ignores the mask sees calm air rather than a hard edge of
    /// maximum wind.
    fn off_grid(&self) -> [u8; 4] {
        match self {
            Self::Scalar24 { .. } => [0, 0, 0, 255],
            Self::Scalar16 { .. } => [0, 0, 0, 255],
            Self::VectorField { .. } => [ZERO_CODE as u8, ZERO_CODE as u8, 0, 255],
        }
    }

    /// Encodes the physical values of one grid cell.
    fn encode(&self, values: [f64; 2]) -> [u8; 4] {
        if values[..self.bands()].iter().any(|value| value.is_nan()) {
            return self.off_grid();
        }
        match *self {
            Self::Scalar24 { base, interval } => {
                let code = normalize((values[0] - base) / interval, 0.0, SCALAR24_CODES) as u32;
                [(code >> 16) as u8, (code >> 8) as u8, code as u8, 255]
            }
            Self::Scalar16 { min, max } => {
                let code = normalize(
                    (values[0] - min) / (max - min) * SCALAR16_CODES,
                    0.0,
                    SCALAR16_CODES,
                ) as u32;
                [(code >> 8) as u8, code as u8, 255, 255]
            }
            Self::VectorField { component_limit } => [
                signed_code(values[0], component_limit),
                signed_code(values[1], component_limit),
                255,
                255,
            ],
        }
    }

    /// What the archive metadata carries, so that a client can invert this
    /// without being told anything else.
    pub(crate) fn descriptor(&self) -> Value {
        match *self {
            Self::Scalar24 { base, interval } => json!({
                "encoding": "scalar-24",
                "base": base,
                "interval": interval,
                "compatible_decoder": "mapbox-terrain-rgb",
                "sample_space": TEXEL_SAMPLE_SPACE,
                "decode": {
                    "kind": "linear-packed",
                    "channels": ["r", "g", "b"],
                    "weights": [
                        65536.0 * BYTE_CODES * interval,
                        256.0 * BYTE_CODES * interval,
                        BYTE_CODES * interval,
                    ],
                    "offset": base,
                },
                "mask": null,
            }),
            Self::Scalar16 { min, max } => json!({
                "encoding": "scalar-16",
                "min": min,
                "max": max,
                "sample_space": TEXEL_SAMPLE_SPACE,
                "decode": {
                    "kind": "linear-packed",
                    "channels": ["r", "g"],
                    "weights": [
                        256.0 * BYTE_CODES * (max - min) / SCALAR16_CODES,
                        BYTE_CODES * (max - min) / SCALAR16_CODES,
                    ],
                    "offset": min,
                },
                "mask": mask_descriptor(),
            }),
            Self::VectorField { component_limit } => json!({
                "encoding": "vector-field",
                "component_limit": component_limit,
                "sample_space": TEXEL_SAMPLE_SPACE,
                "decode": {
                    "kind": "linear-channels",
                    "channels": ["r", "g"],
                    "scale": BYTE_CODES * component_limit / SIGNED_CODES,
                    "offset": -ZERO_CODE * component_limit / SIGNED_CODES,
                },
                "mask": mask_descriptor(),
            }),
        }
    }
}

/// Rounds into `low..=high`, mapping NaN to `low` rather than letting the cast
/// pick a value. NaN survives `clamp`, and `as` would turn it into zero - the
/// one code both encodings keep back to mean something else.
#[inline]
fn normalize(value: f64, low: f64, high: f64) -> f64 {
    if value.is_nan() {
        return low;
    }
    value.round().clamp(low, high)
}

/// Signed-normalises one component of a vector into `1..=255`, with zero landing
/// exactly on [`ZERO_CODE`].
///
/// An even split across all 256 codes would put zero at 127.5, which no byte can
/// hold, and a still field would creep in one direction forever.
#[inline]
fn signed_code(value: f64, component_limit: f64) -> u8 {
    let normalized = (value / component_limit).clamp(-1.0, 1.0);
    if normalized.is_nan() {
        return ZERO_CODE as u8;
    }
    (normalized * SIGNED_CODES + ZERO_CODE).round() as u8
}

/// Pixels of neighbouring tiles carried around each edge.
///
/// A sampler filtering near the edge of a tile reaches past it, and without a
/// margin it clamps to the edge instead: the field flattens into a visible
/// seam along every tile boundary. One pixel is all a bilinear filter can
/// reach.
pub(crate) const TILE_BUFFER: u32 = 1;

/// The side of the stored image: the tile plus its margin on both sides.
pub(crate) const IMAGE_EXTENT: u32 = TILE_EXTENT + 2 * TILE_BUFFER;

/// The source grid of one tile, resampled onto a regular lattice.
///
/// The points arrive at whatever resolution the quadtree merged them to, so
/// their spacing is not uniform. Laying them on the finest lattice present
/// first means the interpolation below never has to know that.
struct SampleGrid {
    /// Lattice index of column zero, and of row zero.
    ///
    /// Held as an index rather than as a grid coordinate so that the lattice a
    /// point falls in is a property of the grid alone. Anchoring on whichever
    /// points a tile happened to read would leave two tiles computing the same
    /// position through different subtractions, and the last bit of the
    /// interpolation weight - and so the last code of the pixel - would differ
    /// across every seam.
    first: (i64, i64),
    /// Grid coordinates per lattice step. A power of two, so dividing a
    /// coordinate by it is exact.
    step: f64,
    columns: usize,
    rows: usize,
    /// Physical values per node, `None` where the grid has no data.
    nodes: Vec<Option<[f64; 2]>>,
}

impl SampleGrid {
    fn build(
        deduped_points: &PointMap<(u32, u32), Point>,
        band_scales: &[BandScale<'_>],
        bands: usize,
    ) -> Option<Self> {
        let mut step = u32::MAX;
        let (mut x1, mut y1, mut x2, mut y2) = (u32::MAX, u32::MAX, 0, 0);
        for ((x, y), point) in deduped_points {
            let width = 1 << point.power;
            step = step.min(width);
            x1 = x1.min(*x);
            y1 = y1.min(*y);
            x2 = x2.max(x + width);
            y2 = y2.max(y + width);
        }
        if step == u32::MAX {
            return None;
        }

        let columns = ((x2 - x1) / step) as usize + 1;
        let rows = ((y2 - y1) / step) as usize + 1;
        let mut grid = Self {
            first: (i64::from(x1 / step), i64::from(y1 / step)),
            step: f64::from(step),
            columns,
            rows,
            nodes: vec![None; columns * rows],
        };

        for ((x, y), point) in deduped_points {
            let mut values = [0.0; 2];
            let mut complete = true;
            for (index, value) in values.iter_mut().enumerate().take(bands) {
                match point.values[index].get() {
                    Some(raw) => *value = band_scales[index].physical(raw),
                    // Half a vector is not a velocity, and interpolating over
                    // the gap would invent one.
                    None => complete = false,
                }
            }
            if !complete {
                continue;
            }

            // A point merged to a coarser level owns every node it covers.
            let width = 1 << point.power;
            for row in (y - y1) / step..(y + width - y1) / step {
                for column in (x - x1) / step..(x + width - x1) / step {
                    let index = row as usize * columns + column as usize;
                    if index < grid.nodes.len() {
                        grid.nodes[index] = Some(values);
                    }
                }
            }
        }
        Some(grid)
    }

    /// Interpolates at a grid coordinate, skipping nodes the grid does not
    /// reach.
    ///
    /// Returns the value and whether the point itself is inside the data. The
    /// two differ along the edge of a regional grid, and both are needed: the
    /// value carries past the edge so that a client filtering across it blends
    /// against real data rather than against a sentinel, while the flag still
    /// marks where the data actually stops.
    fn sample(&self, x: f64, y: f64, interpolate: bool) -> Option<([f64; 2], bool)> {
        // Both divisions are exact, and neither depends on which points this
        // tile read, so the same position gives the same weights everywhere.
        let column = x / self.step - self.first.0 as f64;
        let row = y / self.step - self.first.1 as f64;
        let (column1, row1) = (column.floor(), row.floor());
        let (fx, fy) = (column - column1, row - row1);

        // A quantized field carries class representatives, and the values
        // between two classes are not values at all. Blending them would write
        // a class that does not exist into the tile, which no filter setting on
        // the client can undo.
        let (fx, fy) = if interpolate {
            (fx, fy)
        } else {
            (fx.round(), fy.round())
        };

        let mut total = [0.0; 2];
        let mut weight_sum = 0.0;
        for (dy, wy) in [(0.0, 1.0 - fy), (1.0, fy)] {
            for (dx, wx) in [(0.0, 1.0 - fx), (1.0, fx)] {
                let (column, row) = (column1 + dx, row1 + dy);
                if column < 0.0 || row < 0.0 {
                    continue;
                }
                let (column, row) = (column as usize, row as usize);
                if column >= self.columns || row >= self.rows {
                    continue;
                }
                let Some(values) = self.nodes[row * self.columns + column] else {
                    continue;
                };
                let weight = wx * wy;
                total[0] += values[0] * weight;
                total[1] += values[1] * weight;
                weight_sum += weight;
            }
        }
        if weight_sum <= 0.0 {
            return None;
        }

        // Inside the data means the node this point sits nearest to has a
        // value, which puts the boundary within half a cell of the truth.
        let nearest = {
            let (column, row) = (column.round(), row.round());
            (column >= 0.0 && row >= 0.0)
                .then_some((column as usize, row as usize))
                .filter(|(column, row)| *column < self.columns && *row < self.rows)
                .and_then(|(column, row)| self.nodes[row * self.columns + column])
                .is_some()
        };
        Some(([total[0] / weight_sum, total[1] / weight_sum], nearest))
    }
}

/// What the source values look like beside the range the encoding was given.
///
/// Saturation is silent - a 60 m/s jet in a `vector-field:50` archive simply
/// becomes 50 - so this is measured once over the source and reported, rather
/// than left for a reader to notice as a field that stops getting faster.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub(crate) struct RangeReport {
    /// Smallest and largest physical value seen, per band the encoding reads.
    pub extremes: [Option<(f64, f64)>; 2],
    /// Grid points whose value the range cannot hold.
    pub clipped: u64,
    /// Grid points carrying a value at all.
    pub total: u64,
}

impl RangeReport {
    pub(crate) fn is_clipping(&self) -> bool {
        self.clipped > 0
    }

    /// The share of values the range cannot hold, as a percentage.
    pub(crate) fn clipped_share(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        self.clipped as f64 * 100.0 / self.total as f64
    }

    fn observe(&mut self, encoding: ColorEncoding, values: [f64; 2], bands: usize) {
        self.total += 1;
        if encoding.clips(values) {
            self.clipped += 1;
        }
        for (extreme, value) in self.extremes.iter_mut().zip(values).take(bands) {
            *extreme = Some(match *extreme {
                Some((low, high)) => (low.min(value), high.max(value)),
                None => (value, value),
            });
        }
    }
}

/// Measures the source values of one product against an encoding, in one pass
/// over the grid rather than once per tile: a point appears in a tile at every
/// zoom, and counting it there would report it many times over.
pub(crate) fn inspect_range(
    chunks: &[(u64, crate::model::BaseTile)],
    band_scales: &[BandScale<'_>],
    encoding: ColorEncoding,
) -> RangeReport {
    let bands = encoding.bands();
    let mut report = RangeReport::default();
    if band_scales.len() < bands {
        return report;
    }

    for (_, tile) in chunks {
        let Some(first) = tile.bands.first() else {
            continue;
        };
        for index in 0..first.values.len() {
            let mut values = [0.0; 2];
            let mut complete = true;
            for (band, value) in values.iter_mut().enumerate().take(bands) {
                match tile.bands[band].values[index].get() {
                    // These are raw readings, straight out of the GRIB.
                    Some(raw) => *value = band_scales[band].source_physical(raw),
                    None => complete = false,
                }
            }
            if complete {
                report.observe(encoding, values, bands);
            }
        }
    }
    report
}

/// Renders the points of one tile into an RGBA raster tile.
///
/// The value at each pixel is interpolated from the source grid rather than
/// taken from whichever cell happens to cover it, so that a particle crossing
/// the field sees it change smoothly instead of in steps the size of a grid
/// cell. Every encoding here is affine in the texel, so interpolating before
/// encoding and interpolating after are the same thing.
///
/// Returns `None` when no pixel is covered, so that the caller can leave the
/// tile out of the archive entirely.
pub(super) fn render_raster_tile(
    tileset_spec: &TilesetSpec,
    zxy: Zxy,
    deduped_points: &PointMap<(u32, u32), Point>,
    band_scales: &[BandScale<'_>],
    encoding: ColorEncoding,
) -> Result<Option<Vec<u8>>> {
    let bands = encoding.bands();
    if band_scales.len() < bands {
        return Ok(None);
    }
    let Some(grid) = SampleGrid::build(deduped_points, band_scales, bands) else {
        return Ok(None);
    };
    // Matches the filter the metadata asks the client for: interpolating here
    // and then telling the client not to would put the invented values in the
    // tile anyway.
    let interpolate = !band_scales
        .iter()
        .take(bands)
        .any(|band| band.is_quantized());

    let spec = &tileset_spec.grid_spec;
    let size = IMAGE_EXTENT as usize;
    let mut pixels = encoding.off_grid().repeat(size * size);
    let mut covered = false;

    // Pixel positions are derived from the whole zoom level rather than from
    // this tile's own corner. Both give the same place, but only this one gives
    // the same bits: the numerator is a whole number of pixels and the divisor
    // a power of two, so the division is exact and a pixel in the margin lands
    // byte-for-byte on the neighbour's copy of it. Working from the corner
    // rounds twice, and the seam disagreed by a code.
    let (z, tile_x, tile_y) = zxy;
    let pixels_across = f64::from(TILE_EXTENT) * f64::from(1u32 << z);
    let position = |tile: u32, index: usize| {
        (f64::from(tile) * f64::from(TILE_EXTENT) + index as f64 + 0.5 - f64::from(TILE_BUFFER))
            / pixels_across
    };
    // A whole turn of longitude, in grid coordinates, for the products whose
    // frame runs 0..360 while the tile asks in -180..180.
    let turn = 360.0 * f64::from(spec.lng_denom);

    for row in 0..size {
        for column in 0..size {
            let (longitude, latitude) =
                web_mercator_to_lnglat(position(tile_x, column), position(tile_y, row));

            let y = (latitude - spec.lat_0) * f64::from(spec.lat_denom);
            let x = (longitude - spec.lng_0) * f64::from(spec.lng_denom);
            // The same meridian can be named twice; take whichever naming the
            // grid holds data for.
            let Some((values, on_grid)) = [x, x + turn, x - turn]
                .into_iter()
                .find_map(|x| grid.sample(x, y, interpolate))
            else {
                continue;
            };

            let mut rgba = encoding.encode(values);
            if !on_grid {
                // The value stays, so that a client filtering across the edge
                // blends against real data; only the flag says it is outside.
                rgba[2] = encoding.off_grid()[2];
            } else {
                covered = true;
            }
            let index = (row * size + column) * 4;
            pixels[index..index + 4].copy_from_slice(&rgba);
        }
    }

    if !covered {
        return Ok(None);
    }
    Ok(Some(encode_png(&pixels)?))
}

fn encode_png(pixels: &[u8]) -> Result<Vec<u8>> {
    let mut buffer = Vec::new();
    let mut encoder = png::Encoder::new(&mut buffer, IMAGE_EXTENT, IMAGE_EXTENT);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    // Leaves the adaptive row filter this preset selects in place. A field is
    // smooth and its off-grid margin is constant, so the filters predict it
    // almost perfectly: measured over one archive, turning them off grew it
    // from 2.0 MB to 26.5 MB. Note that `set_compression` also picks a filter,
    // so anything set before it is discarded.
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder
        .write_header()
        .context("failed to write the raster tile header")?;
    writer
        .write_image_data(pixels)
        .context("failed to write the raster tile data")?;
    writer
        .finish()
        .context("failed to finish the raster tile")?;
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny client driven only by the descriptor JSON. Round-trip tests use
    /// this instead of matching on [`ColorEncoding`], so changing metadata
    /// independently of the encoder is observable.
    /// Decodes a pixel the way a shader does: sample the texture, then apply
    /// the published coefficients and nothing else.
    ///
    /// The channels arrive in `0..1`, exactly as a sampler delivers them, so
    /// this fails if the descriptor is written in byte space. Every round-trip
    /// test goes through here rather than through a private helper, which is
    /// what stops the published numbers from drifting away from the encoder.
    fn decode(descriptor: &Value, pixel: [u8; 4]) -> Vec<f64> {
        decode_texel(descriptor, pixel.map(|code| f64::from(code) / 255.0))
    }

    /// Decodes texels that need not sit on a code, which is what the shader
    /// receives once the sampler has filtered between two of them.
    fn decode_texel(descriptor: &Value, sampled: [f64; 4]) -> Vec<f64> {
        let texel = |name: &str| {
            let index = match name {
                "r" => 0,
                "g" => 1,
                "b" => 2,
                "a" => 3,
                _ => panic!("unknown channel {name:?}"),
            };
            sampled[index]
        };
        let decode = &descriptor["decode"];
        let channels = decode["channels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|name| texel(name.as_str().unwrap()))
            .collect::<Vec<_>>();
        let offset = decode["offset"].as_f64().unwrap();

        match decode["kind"].as_str().unwrap() {
            // dot(texel.channels, weights) + offset
            "linear-packed" => {
                let weights = decode["weights"].as_array().unwrap();
                vec![
                    channels
                        .iter()
                        .zip(weights)
                        .map(|(value, weight)| value * weight.as_f64().unwrap())
                        .sum::<f64>()
                        + offset,
                ]
            }
            // texel.channels * scale + offset
            "linear-channels" => {
                let scale = decode["scale"].as_f64().unwrap();
                channels
                    .iter()
                    .map(|value| value * scale + offset)
                    .collect()
            }
            kind => panic!("unknown decode kind {kind:?}"),
        }
    }

    fn wind(component_limit: f64) -> ColorEncoding {
        ColorEncoding::VectorField { component_limit }
    }

    /// Calm air has to land on a code that means zero, which is why one code is
    /// given up at the bottom of the range: an even split across all 256 would
    /// put zero at 127.5, and a still field would creep in one direction.
    ///
    /// The code itself is exact. Folding the texel scale into the published
    /// coefficients leaves a rounding residual in the decode, so what is
    /// checked there is that it stays far below anything the encoding can
    /// resolve - the drift this guards against was most of a quantisation step,
    /// and the residual is a fraction of a millionth of one.
    #[test]
    fn calm_air_decodes_to_zero_within_rounding() {
        for component_limit in [1.0, 12.5, 50.0, 100.0] {
            let encoding = wind(component_limit);
            let pixel = encoding.encode([0.0, 0.0]);
            assert_eq!([pixel[0], pixel[1]], [ZERO_CODE as u8; 2]);

            let step = component_limit / SIGNED_CODES;
            for component in decode(&encoding.descriptor(), pixel) {
                assert!(
                    component.abs() < step * 1e-6,
                    "calm air decoded as {component} m/s, against a step of {step}"
                );
            }
        }
    }

    #[test]
    fn a_vector_survives_the_round_trip_within_half_a_step() {
        let encoding = wind(100.0);
        let step = 100.0 / SIGNED_CODES;

        for u in [-100.0, -37.5, -0.25, 0.0, 0.25, 12.5, 99.9, 100.0] {
            for v in [-100.0, -1.0, 0.0, 4.0, 100.0] {
                let decoded = decode(&encoding.descriptor(), encoding.encode([u, v]));
                assert!(
                    (decoded[0] - u).abs() <= step / 2.0 + f64::EPSILON
                        && (decoded[1] - v).abs() <= step / 2.0 + f64::EPSILON,
                    "({u}, {v}) decoded as {decoded:?}"
                );
            }
        }
    }

    #[test]
    fn a_vector_field_reaches_both_ends_of_its_range() {
        let encoding = wind(50.0);
        let step = 50.0 / SIGNED_CODES;

        assert_eq!(encoding.encode([-50.0, 50.0])[..2], [1, 255]);
        for (decoded, expected) in decode(&encoding.descriptor(), encoding.encode([-50.0, 50.0]))
            .into_iter()
            .zip([-50.0, 50.0])
        {
            assert!(
                (decoded - expected).abs() < step * 1e-6,
                "{expected} decoded as {decoded}"
            );
        }
    }

    #[test]
    fn values_outside_the_range_clamp_instead_of_wrapping() {
        let encoding = wind(50.0);

        // Wrapping here would turn a violent easterly into a westerly.
        assert_eq!(encoding.encode([-1000.0, 1000.0])[..2], [1, 255]);
        assert_eq!(
            encoding.encode([f64::NEG_INFINITY, f64::INFINITY])[..2],
            [1, 255]
        );
        // NaN must not fall through the cast into a neighbouring code.
        assert_eq!(encoding.encode([f64::NAN, f64::NAN]), encoding.off_grid());

        let terrain = ColorEncoding::Scalar24 {
            base: 0.0,
            interval: 1.0,
        };
        assert_eq!(terrain.encode([-1.0, 0.0])[..3], [0, 0, 0]);
        assert_eq!(terrain.encode([f64::NAN, 0.0]), terrain.off_grid());
    }

    #[test]
    fn a_scalar_survives_the_round_trip_within_half_an_interval() {
        let encoding = ColorEncoding::Scalar24 {
            base: -10000.0,
            interval: 0.1,
        };

        for value in [-10000.0, -9999.9, -273.15, 0.0, 15.5, 8848.0] {
            let decoded = decode(&encoding.descriptor(), encoding.encode([value, 0.0]))[0];
            assert!(
                (decoded - value).abs() <= 0.05 + 1e-9,
                "{value} -> {decoded}"
            );
        }
    }

    #[test]
    fn a_masked_scalar_survives_the_round_trip_and_keeps_its_mask() {
        // The point of spending only 16 bits: blue is still free to say where
        // the grid stops, which a regional model needs.
        let encoding = ColorEncoding::Scalar16 {
            min: -50.0,
            max: 50.0,
        };
        let step = 100.0 / SCALAR16_CODES;

        for value in [-50.0, -12.34, 0.0, 0.001, 27.5, 50.0] {
            let pixel = encoding.encode([value, 0.0]);
            assert_eq!(pixel[2], 255, "blue marks the pixel as on-grid");
            let decoded = decode(&encoding.descriptor(), pixel)[0];
            assert!(
                (decoded - value).abs() <= step / 2.0 + 1e-9,
                "{value} -> {decoded}"
            );
        }

        assert_eq!(encoding.off_grid()[2], 0);
        assert_eq!(encoding.encode([-1000.0, 0.0])[..2], [0, 0]);
        assert_eq!(encoding.encode([1000.0, 0.0])[..2], [255, 255]);
    }

    #[test]
    fn every_tile_is_opaque_so_that_alpha_cannot_be_premultiplied_away() {
        // A browser that premultiplies alpha would zero the components of any
        // transparent pixel, which is exactly the data these carry.
        for encoding in [
            wind(50.0),
            ColorEncoding::Scalar24 {
                base: 0.0,
                interval: 1.0,
            },
            ColorEncoding::Scalar16 { min: 0.0, max: 1.0 },
        ] {
            assert_eq!(encoding.off_grid()[3], 255);
            assert_eq!(encoding.encode([1.0, 1.0])[3], 255);
        }
    }

    #[test]
    fn masked_encodings_distinguish_off_grid_pixels_from_values() {
        // Blue carries the on-grid flag for a vector field...
        let encoding = wind(100.0);
        assert_eq!(encoding.off_grid()[2], 0);
        assert_eq!(encoding.encode([0.0, 0.0])[2], 255);

        // ...and for a masked scalar.
        let scalar = ColorEncoding::Scalar16 { min: 0.0, max: 1.0 };
        assert_eq!(scalar.off_grid()[2], 0);
        assert_eq!(scalar.encode([0.0, 0.0])[2], 255);
    }

    /// Applies the published mask rule to a pixel, in the space a shader works
    /// in. A rule written against the byte would be false for every texel.
    fn on_grid(descriptor: &Value, pixel: [u8; 4]) -> bool {
        let mask = &descriptor["mask"];
        if mask.is_null() {
            return true;
        }
        let index = match mask["channel"].as_str().unwrap() {
            "r" => 0,
            "g" => 1,
            "b" => 2,
            "a" => 3,
            channel => panic!("unknown channel {channel:?}"),
        };
        let texel = f64::from(pixel[index]) / 255.0;
        match mask["kind"].as_str().unwrap() {
            "channel-gte" => texel >= mask["threshold"].as_f64().unwrap(),
            kind => panic!("unknown mask kind {kind:?}"),
        }
    }

    /// The masks have to work in texel space too. `"value": 255` reads
    /// perfectly sensibly and is false for every texel a sampler can produce,
    /// so the whole tile would come back off-grid.
    #[test]
    fn the_published_mask_is_in_texel_space() {
        for encoding in [
            wind(50.0),
            ColorEncoding::Scalar16 {
                min: 0.0,
                max: 10.0,
            },
        ] {
            let descriptor = encoding.descriptor();
            assert!(
                on_grid(&descriptor, encoding.encode([1.0, 1.0])),
                "{encoding:?} marked a value as off-grid"
            );
            assert!(
                !on_grid(&descriptor, encoding.off_grid()),
                "{encoding:?} marked its own gap as on-grid"
            );

            // Halfway across the edge of the grid a sampler returns the middle
            // of the two, and the threshold has to land somewhere definite.
            let mut blended = encoding.encode([1.0, 1.0]);
            blended[2] = 128;
            assert!(on_grid(&descriptor, blended));
            blended[2] = 127;
            assert!(!on_grid(&descriptor, blended));
        }

        // A scalar-24 has no channel to spare and says so.
        assert!(
            ColorEncoding::Scalar24 {
                base: 0.0,
                interval: 1.0,
            }
            .descriptor()["mask"]
                .is_null()
        );
    }

    /// Decodes the way a fragment shader does, in `f32`. `fused` picks whether
    /// the compiler contracted the multiply and add, which it is free to do and
    /// which changes the rounding.
    fn decode_f32(descriptor: &Value, pixel: [u8; 4], fused: bool) -> Vec<f32> {
        let texel = |name: &str| {
            let index = match name {
                "r" => 0,
                "g" => 1,
                "b" => 2,
                "a" => 3,
                channel => panic!("unknown channel {channel:?}"),
            };
            f32::from(pixel[index]) / 255.0
        };
        let decode = &descriptor["decode"];
        assert_eq!(descriptor["sample_space"], "normalized-texel");
        let channels = decode["channels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|name| texel(name.as_str().unwrap()))
            .collect::<Vec<_>>();
        let offset = decode["offset"].as_f64().unwrap() as f32;
        let step = |value: f32, coefficient: f32, accumulator: f32| {
            if fused {
                value.mul_add(coefficient, accumulator)
            } else {
                value * coefficient + accumulator
            }
        };

        match decode["kind"].as_str().unwrap() {
            "linear-packed" => {
                let weights = decode["weights"].as_array().unwrap();
                let mut total = offset;
                for (value, weight) in channels.iter().zip(weights) {
                    total = step(*value, weight.as_f64().unwrap() as f32, total);
                }
                vec![total]
            }
            "linear-channels" => {
                let scale = decode["scale"].as_f64().unwrap() as f32;
                channels
                    .iter()
                    .map(|value| step(*value, scale, offset))
                    .collect()
            }
            kind => panic!("unknown decode kind {kind:?}"),
        }
    }

    /// The residual left by folding the texel scale into the coefficients has
    /// to stay negligible in the precision the shader actually has, not just in
    /// the `f64` the encoder used. A drifting calm field is what this buys.
    #[test]
    fn calm_air_stays_still_in_shader_precision() {
        for component_limit in [10.0, 50.0, 100.0] {
            let encoding = wind(component_limit);
            let descriptor = encoding.descriptor();
            let pixel = encoding.encode([0.0, 0.0]);
            let step = component_limit / SIGNED_CODES;

            for fused in [true, false] {
                for component in decode_f32(&descriptor, pixel, fused) {
                    // A metre per hour of drift is already invisible; this
                    // leaves three orders of magnitude of headroom under it.
                    assert!(
                        f64::from(component.abs()) < 1e-4,
                        "calm air drifts {component} m/s (fused: {fused}), \
                         against a step of {step}"
                    );
                }
            }
        }
    }

    /// The sampler filters before the shader ever runs, so decoding has to
    /// commute with it:
    ///
    /// ```text
    /// decode(mix(a, b, t)) == mix(decode(a), decode(b), t)
    /// ```
    ///
    /// Every encoding here is affine in the texel, which is what makes that
    /// hold. It is not obvious across a carry in a packed scalar, where two
    /// neighbouring values have codes that look nothing alike, so that case is
    /// checked explicitly.
    #[test]
    fn decoding_commutes_with_the_filtering_that_precedes_it() {
        let cases: [(ColorEncoding, [f64; 2], [f64; 2]); 4] = [
            (
                ColorEncoding::Scalar16 {
                    min: 0.0,
                    max: SCALAR16_CODES,
                },
                // Either side of a carry: [0, 255] and [1, 0].
                [255.0, 0.0],
                [256.0, 0.0],
            ),
            (
                ColorEncoding::Scalar16 {
                    min: -50.0,
                    max: 50.0,
                },
                [-12.5, 0.0],
                [31.25, 0.0],
            ),
            (
                ColorEncoding::Scalar24 {
                    base: -10000.0,
                    interval: 0.1,
                },
                [100.0, 0.0],
                [8848.0, 0.0],
            ),
            (wind(50.0), [-30.0, 12.0], [7.0, -21.0]),
        ];

        for (encoding, low, high) in cases {
            let descriptor = encoding.descriptor();
            let (a, b) = (encoding.encode(low), encoding.encode(high));
            let texel = |pixel: [u8; 4]| pixel.map(|code| f64::from(code) / 255.0);

            for step in 0..=8 {
                let t = f64::from(step) / 8.0;
                let mixed: [f64; 4] =
                    std::array::from_fn(|index| texel(a)[index] * (1.0 - t) + texel(b)[index] * t);

                let filtered_then_decoded = decode_texel(&descriptor, mixed);
                let decoded_then_mixed = decode(&descriptor, a)
                    .into_iter()
                    .zip(decode(&descriptor, b))
                    .map(|(low, high)| low * (1.0 - t) + high * t)
                    .collect::<Vec<_>>();

                for (filtered, mixed) in filtered_then_decoded.iter().zip(&decoded_then_mixed) {
                    assert!(
                        (filtered - mixed).abs() < 1e-9 * mixed.abs().max(1.0),
                        "{encoding:?} at t={t}: filtering first gives {filtered}, \
                         decoding first gives {mixed}"
                    );
                }
            }
        }
    }

    /// Packing a scalar into all 24 bits asks for more precision than the
    /// `f32` a shader decodes into can return. The encoding is exact to
    /// `interval`; the reconstruction is not, and how far off depends on how
    /// much of the range is used, so this pins both ends rather than letting
    /// the claim stand unqualified.
    #[test]
    fn a_packed_scalar_loses_precision_in_a_shader_at_the_top_of_its_range() {
        let interval = 0.1;
        let encoding = ColorEncoding::Scalar24 {
            base: -10000.0,
            interval,
        };
        let descriptor = encoding.descriptor();

        let error = |value: f64| {
            let pixel = encoding.encode([value, 0.0]);
            [true, false]
                .into_iter()
                .map(|fused| {
                    let decoded = f64::from(decode_f32(&descriptor, pixel, fused)[0]);
                    (decoded - value).abs()
                })
                .fold(0.0, f64::max)
        };

        // Over the span terrain-RGB was drawn for, well inside one interval.
        for value in [-10000.0, -500.0, 0.0, 3776.0, 8848.0] {
            assert!(
                error(value) < interval,
                "{value} m reconstructed with an error of {}",
                error(value)
            );
        }

        // Once the codes run high the mantissa runs out, and the worst case is
        // not at the very top - that one happens to be representable - but
        // somewhere among the codes that are not. This is the part a caller has
        // to know before choosing this over a narrower `Scalar16`.
        let worst = (1..(1u32 << 24))
            .step_by(1021)
            .map(|code| error(-10000.0 + f64::from(code) * interval))
            .fold(0.0, f64::max);
        assert!(
            worst > interval,
            "the loss over the full range has gone away ({worst} at worst); the \
             warning on ColorEncoding::Scalar24 can be relaxed"
        );
    }

    /// A shader gets its channels in `0..1`; a descriptor written in byte space
    /// would be out by 255 and the tiles would look like noise.
    #[test]
    fn the_published_coefficients_are_in_texel_space() {
        for encoding in [
            wind(100.0),
            ColorEncoding::Scalar16 {
                min: -50.0,
                max: 50.0,
            },
            ColorEncoding::Scalar24 {
                base: -10000.0,
                interval: 0.1,
            },
        ] {
            let descriptor = encoding.descriptor();
            assert!(
                descriptor["decode"]["offset"].is_f64(),
                "every kind decodes with one offset"
            );

            // The largest channel value a shader can see is 1.0, so the
            // coefficients alone have to span the whole range.
            let full = decode(&descriptor, [255, 255, 255, 255])[0];
            let empty = decode(&descriptor, [0, 0, 0, 255])[0];
            let span = (full - empty).abs();
            let expected = match encoding {
                ColorEncoding::Scalar24 { interval, .. } => SCALAR24_CODES * interval,
                ColorEncoding::Scalar16 { min, max } => max - min,
                ColorEncoding::VectorField { component_limit } => {
                    component_limit * BYTE_CODES / SIGNED_CODES
                }
            };
            assert!(
                (span - expected).abs() < expected * 1e-9,
                "{encoding:?} spans {span} but should span {expected}"
            );
        }
    }

    #[test]
    fn the_descriptor_names_the_encoding_and_its_parameters() {
        let descriptor = wind(80.0).descriptor();
        assert_eq!(descriptor["encoding"], "vector-field");
        assert_eq!(descriptor["component_limit"], 80.0);
        assert_eq!(descriptor["decode"]["kind"], "linear-channels");
        assert_eq!(descriptor["mask"]["channel"], "b");

        let descriptor = ColorEncoding::Scalar24 {
            base: -10000.0,
            interval: 0.1,
        }
        .descriptor();
        assert_eq!(descriptor["encoding"], "scalar-24");
        assert_eq!(descriptor["base"], -10000.0);
        assert_eq!(descriptor["interval"], 0.1);
    }

    /// Enough bands is not the same as the right ones. A wave product has three
    /// and would otherwise have its height and direction encoded as a velocity,
    /// producing tiles that are well-formed and wrong.
    #[test]
    fn an_encoding_rejects_bands_that_are_not_the_ones_it_means() {
        let names = |names: &[&str]| {
            names
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>()
        };

        let vector = wind(50.0);
        assert!(vector.check_bands(&names(&["u", "v"])).is_ok());
        // Order carries meaning: east-west first.
        assert!(vector.check_bands(&names(&["v", "u"])).is_err());
        // A wave product, which has enough bands and none of the right ones.
        assert!(
            vector
                .check_bands(&names(&["height", "direction", "period"]))
                .is_err()
        );
        assert!(vector.check_bands(&names(&["value", "error"])).is_err());
        assert!(vector.check_bands(&names(&["u"])).is_err());

        let scalar = ColorEncoding::Scalar16 { min: 0.0, max: 1.0 };
        // Any name will do for a scalar, but only one of them.
        assert!(scalar.check_bands(&names(&["value"])).is_ok());
        assert!(scalar.check_bands(&names(&["temperature"])).is_ok());
        assert!(scalar.check_bands(&names(&["value", "error"])).is_err());
        assert!(scalar.check_bands(&names(&[])).is_err());
    }

    #[test]
    fn each_encoding_declares_the_bands_it_reads() {
        assert_eq!(wind(1.0).bands(), 2);
        assert_eq!(
            ColorEncoding::Scalar24 {
                base: 0.0,
                interval: 1.0
            }
            .bands(),
            1
        );
    }

    /// A tile is mostly one value, and the row filters flatten that to almost
    /// nothing. Turning them off - which `set_compression` will silently do if
    /// a filter is chosen before it - costs an order of magnitude in size, and
    /// nothing else in the pipeline would notice.
    #[test]
    fn a_nearly_uniform_tile_stays_small() {
        let mut pixels = wind(50.0)
            .off_grid()
            .repeat((IMAGE_EXTENT * IMAGE_EXTENT) as usize);
        for row in 100..200 {
            for column in 100..200 {
                let index = (row * IMAGE_EXTENT as usize + column) * 4;
                pixels[index..index + 4].copy_from_slice(&[12, 200, 255, 255]);
            }
        }

        let png = encode_png(&pixels).unwrap();
        assert!(
            png.len() < pixels.len() / 100,
            "expected the filters to flatten the tile, but {} bytes came out of {}",
            png.len(),
            pixels.len()
        );
    }

    #[test]
    fn a_specification_parses_into_the_encoding_it_names() {
        assert_eq!(
            "vector-field:100".parse::<ColorEncoding>().unwrap(),
            ColorEncoding::VectorField {
                component_limit: 100.0
            }
        );
        assert_eq!(
            "scalar16:-50,50".parse::<ColorEncoding>().unwrap(),
            ColorEncoding::Scalar16 {
                min: -50.0,
                max: 50.0
            }
        );
        assert_eq!(
            "scalar24:-10000,0.1".parse::<ColorEncoding>().unwrap(),
            ColorEncoding::Scalar24 {
                base: -10000.0,
                interval: 0.1
            }
        );
    }

    #[test]
    fn a_specification_that_cannot_be_served_is_rejected() {
        // Each of these would otherwise produce tiles that decode to nothing
        // useful, and the failure would only show up on screen.
        for spec in [
            "vector-field",         // no speed limit
            "vector-field:0",       // every value saturates
            "vector-field:-10",     // flips the field
            "vector-field:100,200", // too many
            "scalar16:50,-50",      // decreasing
            "scalar16:1",           // too few
            "scalar24:0,0",         // zero interval
            "scalar16:0,nan",       // not finite
            "hue:1",                // not an encoding
            "",
        ] {
            assert!(
                spec.parse::<ColorEncoding>().is_err(),
                "{spec:?} should have been rejected"
            );
        }
    }

    /// A lattice with `step` 1 whose origin is the grid origin, so that grid
    /// coordinates and node indices coincide.
    fn lattice(rows: [[Option<f64>; 3]; 3]) -> SampleGrid {
        SampleGrid {
            first: (0, 0),
            step: 1.0,
            columns: 3,
            rows: 3,
            nodes: rows
                .iter()
                .flatten()
                .map(|value| value.map(|value| [value, 0.0]))
                .collect(),
        }
    }

    #[test]
    fn sampling_between_nodes_interpolates() {
        let grid = lattice([
            [Some(0.0), Some(10.0), Some(20.0)],
            [Some(0.0), Some(10.0), Some(20.0)],
            [Some(0.0), Some(10.0), Some(20.0)],
        ]);

        assert_eq!(grid.sample(0.0, 0.0, true).unwrap().0[0], 0.0);
        assert_eq!(grid.sample(0.5, 0.0, true).unwrap().0[0], 5.0);
        assert_eq!(grid.sample(1.25, 1.0, true).unwrap().0[0], 12.5);
    }

    /// Weighting a missing neighbour as zero would drag the value toward the
    /// bottom of the range all along the edge of a regional grid, which is
    /// exactly where a client is most likely to be looking.
    #[test]
    fn a_gap_is_skipped_rather_than_counted_as_zero() {
        let grid = lattice([
            [Some(10.0), None, None],
            [Some(10.0), None, None],
            [None, None, None],
        ]);

        // Halfway toward the gap the only value in reach is the one node.
        assert_eq!(grid.sample(0.5, 0.5, true).unwrap().0[0], 10.0);
        assert_eq!(grid.sample(0.9, 0.0, true).unwrap().0[0], 10.0);
    }

    /// A quantized field holds class representatives, and a value between two
    /// classes is not a class. Blending them would write one into the tile, and
    /// no filter setting on the client could undo it.
    #[test]
    fn a_quantized_field_is_resampled_without_inventing_classes() {
        let grid = lattice([
            [Some(0.0), Some(10.0), Some(20.0)],
            [Some(0.0), Some(10.0), Some(20.0)],
            [Some(0.0), Some(10.0), Some(20.0)],
        ]);

        for (x, expected) in [(0.4, 0.0), (0.5, 10.0), (0.6, 10.0), (1.4, 10.0)] {
            assert_eq!(
                grid.sample(x, 0.0, false).unwrap().0[0],
                expected,
                "nearest at {x}"
            );
        }
        // The same positions do interpolate when the values are measurements.
        assert_eq!(grid.sample(0.4, 0.0, true).unwrap().0[0], 4.0);
    }

    /// The value carries past the edge of the data while the flag does not, so
    /// that a client filtering across the boundary blends against real values
    /// rather than against the off-grid sentinel.
    #[test]
    fn a_value_reaches_past_the_edge_that_the_flag_marks() {
        let grid = lattice([
            [Some(7.0), None, None],
            [None, None, None],
            [None, None, None],
        ]);

        let (values, on_grid) = grid.sample(0.2, 0.0, true).unwrap();
        assert_eq!(values[0], 7.0);
        assert!(on_grid, "the nearest node has a value");

        // Past halfway the nearest node is the empty one: still a value to
        // blend against, no longer inside the data.
        let (values, on_grid) = grid.sample(0.8, 0.0, true).unwrap();
        assert_eq!(values[0], 7.0);
        assert!(!on_grid);

        // Out of reach of every node at all.
        assert!(grid.sample(5.0, 5.0, true).is_none());
        assert!(grid.sample(-3.0, 0.0, true).is_none());
    }

    #[test]
    fn the_encoded_png_is_readable_and_keeps_every_byte() {
        let mut pixels = wind(1.0)
            .off_grid()
            .repeat((IMAGE_EXTENT * IMAGE_EXTENT) as usize);
        pixels[..4].copy_from_slice(&[10, 200, 255, 255]);

        let png = encode_png(&pixels).unwrap();
        let decoder = png::Decoder::new(std::io::Cursor::new(&png));
        let mut reader = decoder.read_info().unwrap();
        let mut decoded = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut decoded).unwrap();

        assert_eq!((info.width, info.height), (IMAGE_EXTENT, IMAGE_EXTENT));
        assert_eq!(info.color_type, png::ColorType::Rgba);
        assert_eq!(&decoded[..info.buffer_size()], &pixels[..]);
    }
}
