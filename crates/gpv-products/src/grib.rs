use std::io::Read;

use bit_vec::BitVec;
use chrono::{DateTime, Duration, NaiveDate, NaiveTime, TimeZone, Utc};
use foldhash::HashMap;
use itertools::Itertools;
use tinygrib2::{
    MessageReader,
    message::{DataRepresentationSectionHeader, IdentificationSectionHeader},
    templates::{
        DataRepresentationTemplate5_0, DataRepresentationTemplate5_3,
        DataRepresentationTemplate5_200, GridDefinitionTemplate3_0, ProductDefinitionTemplate4_0,
        ProductDefinitionTemplate4_1, ProductDefinitionTemplate4_8, ProductDefinitionTemplate4_11,
        ProductDefinitionTemplate4_50000, ProductDefinitionTemplate4_50011,
        ProductDefinitionTemplate4_50031, TimeInterval, read_data_7_0, read_data_7_3,
        read_data_7_200,
    },
};

use crate::model::{BandSpec, LngLatGrid};
use crate::products::{
    Ensemble, FixedSurface, GeneratingProcessType, GpvProductElement, GpvProductIdentifier,
    get_product_id_and_band,
};
use crate::products::{PointValue, ProductData};

const MICRODEGREES_PER_DEGREE: f64 = 1_000_000.0;
const GRID_TOLERANCE: f64 = 0.01;

enum DataRepresentationTemplate {
    Template5_0(DataRepresentationTemplate5_0),
    Template5_3(DataRepresentationTemplate5_3),
    Template5_200(DataRepresentationTemplate5_200),
}

fn read_bitmap<R: Read>(reader: &mut R, num_points: usize) -> std::io::Result<BitVec> {
    let mut bytes = vec![0; num_points.div_ceil(8)];
    reader.read_exact(&mut bytes)?;

    let mut bitmap = BitVec::from_bytes(&bytes);
    bitmap.truncate(num_points);
    Ok(bitmap)
}

fn unsupported_grid(
    product_id: &GpvProductIdentifier,
    template: &GridDefinitionTemplate3_0,
    reason: impl std::fmt::Display,
) -> tinygrib2::Error {
    tinygrib2::Error::UnsupportedData(format!(
        "unsupported grid for {} (d_i={}, d_j={} microdegrees): {reason}",
        product_id.path(),
        template.d_i,
        template.d_j
    ))
}

/// Selects the finest base grid needed by one product while retaining the
/// configured grid for products whose messages intentionally mix resolutions.
fn resolve_product_grid(
    product_id: &GpvProductIdentifier,
    product: &ProductData,
    template: &GridDefinitionTemplate3_0,
) -> tinygrib2::Result<LngLatGrid> {
    if template.d_i == 0 || template.d_j == 0 {
        return Err(unsupported_grid(
            product_id,
            template,
            "grid increments must be positive",
        ));
    }

    let mut grid = product
        .grid
        .clone()
        .unwrap_or_else(|| product_id.grid().clone());
    let finer_longitude =
        template.d_i as f64 + 1.0 < MICRODEGREES_PER_DEGREE / f64::from(grid.lng_denom);
    let finer_latitude =
        template.d_j as f64 + 1.0 < MICRODEGREES_PER_DEGREE / f64::from(grid.lat_denom);

    if (finer_longitude || finer_latitude) && !product.points.is_empty() {
        return Err(unsupported_grid(
            product_id,
            template,
            "a finer grid appeared after values for this product had already been read",
        ));
    }
    if finer_longitude {
        grid.lng_denom = (MICRODEGREES_PER_DEGREE / template.d_i as f64) as f32;
    }
    if finer_latitude {
        grid.lat_denom = (MICRODEGREES_PER_DEGREE / template.d_j as f64) as f32;
    }
    Ok(grid)
}

/// The base-grid coordinates of a grid's lower-left corner.
///
/// The corner has to sit on a cell boundary, so dividing it by the cell width
/// has to come out whole. Multiplying by the width instead - as this once did -
/// is a test every whole coordinate passes whatever width it is checked
/// against, so it never rejected anything.
///
/// Replacing it with the real test refuses products that convert today. The
/// hourly nowcast lands on `6401.50032` base cells, which is neither a whole
/// cell nor a clean half, and the truncation below has been moving it one base
/// cell west and south ever since. That is a fault in the base grid this
/// product is measured against rather than in the file, so the shift is kept -
/// silently dropping a supported product would be worse - and reported, so it
/// stops being invisible.
#[derive(Debug, PartialEq, Eq)]
struct GridCorner {
    x: u32,
    y: u32,
    snap: Option<(u32, u32)>,
}

/// The statistical operation the first time range describes.
///
/// A statistical template promises at least one range; indexing straight into
/// the list turns a truncated message into a panic instead of a report.
fn first_statistical_process(interval: &TimeInterval) -> tinygrib2::Result<u8> {
    interval
        .time_ranges
        .first()
        .map(|range| range.statistical_process)
        .ok_or_else(|| {
            tinygrib2::Error::InvalidData("a statistical product carries no time range".to_string())
        })
}

/// When a field is valid.
///
/// A statistical product covers a period rather than an instant, and GRIB2
/// spells out where that period ends: `forecast_time` marks only where it
/// begins. Reading the start instead placed every accumulated product a whole
/// period early - an hour for the short-range forecasts, five minutes for the
/// nowcasts - so the choice between the two is made here, once, where it can be
/// tested.
///
/// See templates 4.8 and 4.11: the forecast time is the start of the overall
/// interval, and the explicit date is its end.
fn valid_datetime(
    reference_datetime: DateTime<Utc>,
    template: &ProductDefinitionTemplate4_0,
    interval: Option<&TimeInterval>,
) -> tinygrib2::Result<DateTime<Utc>> {
    if let Some(interval) = interval {
        // Present but unreadable is not the same as absent. Falling back to the
        // start of the period would give a plausible time that is quietly an
        // accumulation period wrong, which nothing downstream could detect.
        return Utc
            .with_ymd_and_hms(
                i32::from(interval.year),
                u32::from(interval.month),
                u32::from(interval.day),
                u32::from(interval.hour),
                u32::from(interval.minute),
                u32::from(interval.second),
            )
            .single()
            .ok_or_else(|| {
                tinygrib2::Error::InvalidData(format!(
                    "the statistical period ends at {}-{:02}-{:02} {:02}:{:02}:{:02}, \
                     which is not a valid time",
                    interval.year,
                    interval.month,
                    interval.day,
                    interval.hour,
                    interval.minute,
                    interval.second
                ))
            });
    }

    let offset = i64::from(template.forecast_time);
    Ok(reference_datetime
        + match template.indicator_of_unit_of_time_range {
            0 => Duration::minutes(offset),
            1 => Duration::hours(offset),
            2 => Duration::days(offset),
            unit => {
                return Err(tinygrib2::Error::UnsupportedData(format!(
                    "time range unit {unit} is not supported"
                )));
            }
        })
}

fn grid_corner(
    product_id: &GpvProductIdentifier,
    template: &GridDefinitionTemplate3_0,
    grid: &LngLatGrid,
    width: u32,
) -> tinygrib2::Result<GridCorner> {
    let longitude = (template.lo1 as f64 + 1. - MICRODEGREES_PER_DEGREE * grid.lng_0)
        * f64::from(grid.lng_denom)
        / MICRODEGREES_PER_DEGREE;
    let latitude = (template.la1 as f64 + 1. - MICRODEGREES_PER_DEGREE * grid.lat_0)
        * f64::from(grid.lat_denom)
        / MICRODEGREES_PER_DEGREE;
    if longitude < 0. || latitude < 0. {
        return Err(unsupported_grid(
            product_id,
            template,
            format!("its corner ({longitude}, {latitude}) precedes the base grid origin"),
        ));
    }

    let (x_first, y_last) = (longitude as i64, latitude as i64);
    let (x_aligned, y_aligned) = (
        x_first - x_first % i64::from(width),
        y_last - y_last % i64::from(width),
    );
    let snap = (x_aligned != x_first || y_aligned != y_last)
        .then_some(((x_first - x_aligned) as u32, (y_last - y_aligned) as u32));
    Ok(GridCorner {
        x: x_aligned as u32,
        y: y_aligned as u32,
        snap,
    })
}

/// Rejects grids whose coordinates cannot be represented by [`PointValue`].
///
/// The point-building loops can then use ordinary arithmetic and lossless
/// `u16` casts without a malformed grid causing an overflow panic or wrap.
fn validate_grid_extent(
    product_id: &GpvProductIdentifier,
    template: &GridDefinitionTemplate3_0,
    x_first: u32,
    y_first: u32,
    width: u32,
) -> tinygrib2::Result<()> {
    let x_steps = template
        .n_i
        .checked_sub(1)
        .ok_or_else(|| unsupported_grid(product_id, template, "the longitude point count is zero"))?
        .checked_mul(width)
        .ok_or_else(|| unsupported_grid(product_id, template, "the longitude extent overflows"))?;
    let y_steps = template
        .n_j
        .checked_sub(1)
        .ok_or_else(|| unsupported_grid(product_id, template, "the latitude point count is zero"))?
        .checked_mul(width)
        .ok_or_else(|| unsupported_grid(product_id, template, "the latitude extent overflows"))?;
    let x_last = x_first
        .checked_add(x_steps)
        .ok_or_else(|| unsupported_grid(product_id, template, "the longitude extent overflows"))?;
    let y_last = match template.scanning_mode {
        0 => y_first.checked_sub(y_steps).ok_or_else(|| {
            unsupported_grid(
                product_id,
                template,
                "the latitude extent precedes the configured origin",
            )
        })?,
        64 => y_first.checked_add(y_steps).ok_or_else(|| {
            unsupported_grid(product_id, template, "the latitude extent overflows")
        })?,
        mode => {
            return Err(unsupported_grid(
                product_id,
                template,
                format!("scanning mode {mode} is not supported"),
            ));
        }
    };

    if [x_first, x_last, y_first, y_last]
        .into_iter()
        .any(|coordinate| coordinate > u32::from(u16::MAX))
    {
        return Err(unsupported_grid(
            product_id,
            template,
            format!(
                "its base-grid extent ({x_first}, {y_first})..({x_last}, {y_last}) exceeds the u16 coordinate range"
            ),
        ));
    }
    Ok(())
}

fn grid_cell_width(
    product_id: &GpvProductIdentifier,
    template: &GridDefinitionTemplate3_0,
    grid: &LngLatGrid,
) -> tinygrib2::Result<u32> {
    let longitude_width = template.d_i as f64 * f64::from(grid.lng_denom) / MICRODEGREES_PER_DEGREE;
    let latitude_width = template.d_j as f64 * f64::from(grid.lat_denom) / MICRODEGREES_PER_DEGREE;
    let rounded_longitude = longitude_width.round();
    let rounded_latitude = latitude_width.round();
    let valid = rounded_longitude >= 1.0
        && rounded_longitude <= i32::MAX as f64
        && (longitude_width - rounded_longitude).abs() < GRID_TOLERANCE
        && (latitude_width - rounded_latitude).abs() < GRID_TOLERANCE
        && rounded_longitude == rounded_latitude;
    if !valid {
        return Err(unsupported_grid(
            product_id,
            template,
            format!(
                "increments map to unequal or fractional base-grid widths ({longitude_width}, {latitude_width})"
            ),
        ));
    }

    let width = rounded_longitude as u32;
    if !width.is_power_of_two() {
        return Err(unsupported_grid(
            product_id,
            template,
            format!("cell width {width} is not a power of two"),
        ));
    }
    Ok(width)
}

#[derive(Default)]
pub struct GridSquareMessageReader {
    ids: Option<IdentificationSectionHeader>,
    current_gds_tmpl: Option<GridDefinitionTemplate3_0>,
    current_product_id: GpvProductIdentifier,
    current_band: u8,
    current_drs: Option<DataRepresentationSectionHeader>,
    current_drs_tmpl: Option<DataRepresentationTemplate>,
    current_bitmap: Option<BitVec>,
    warned_grid_snap: bool,
    retained_product: Option<String>,
    pub products: HashMap<GpvProductIdentifier, ProductData>,
    /// Track latest reference_datetime for each kind path
    pub latest_reference_times: HashMap<String, DateTime<Utc>>,
}

impl GridSquareMessageReader {
    pub fn retaining_product(retained_product: Option<&str>) -> Self {
        Self {
            retained_product: retained_product.map(str::to_owned),
            ..Default::default()
        }
    }

    fn retains_values_for(&self, product_id: &GpvProductIdentifier) -> bool {
        let Some(requested) = self.retained_product.as_deref() else {
            return true;
        };
        let (data_kind, value_kind) = product_id.path_parts();
        if value_kind.is_empty() {
            requested == data_kind
        } else {
            requested
                .strip_prefix(data_kind)
                .and_then(|suffix| suffix.strip_prefix('/'))
                == Some(value_kind)
        }
    }
}

impl<R: Read> MessageReader<R> for GridSquareMessageReader {
    fn handle_identification(
        &mut self,
        ids: tinygrib2::message::IdentificationSectionHeader,
        _reader: &mut std::io::Take<&mut R>,
    ) -> tinygrib2::Result<()> {
        assert_eq!(ids.production_status_of_processed_data, 0);
        self.ids = Some(ids);
        Ok(())
    }

    fn handle_grid_definition(
        &mut self,
        gds: tinygrib2::message::GridDefinitionSectionHeader,
        reader: &mut std::io::Take<&mut R>,
    ) -> tinygrib2::Result<()> {
        if gds.template_number != 0 {
            return Err(tinygrib2::Error::UnsupportedData(format!(
                "grid definition template 3.{} is not supported",
                gds.template_number
            )));
        }
        let tmpl = GridDefinitionTemplate3_0::read(reader)?;
        if tmpl.resolution_and_component_flags != 0x30 {
            return Err(tinygrib2::Error::UnsupportedData(format!(
                "grid resolution/component flags {:#04x} are not supported",
                tmpl.resolution_and_component_flags
            )));
        }
        if !matches!(tmpl.scanning_mode, 0 | 64) {
            return Err(tinygrib2::Error::UnsupportedData(format!(
                "grid scanning mode {} is not supported",
                tmpl.scanning_mode
            )));
        }
        self.current_gds_tmpl = Some(tmpl);
        Ok(())
    }

    fn handle_product_definition(
        &mut self,
        pds: tinygrib2::message::ProductDefinitionSectionHeader,
        reader: &mut std::io::Take<&mut R>,
    ) -> tinygrib2::Result<()> {
        let ids = self.ids.as_ref().unwrap();
        let reference_datetime = Utc.from_utc_datetime(
            &NaiveDate::from_ymd_opt(ids.year as i32, ids.month as u32, ids.day as u32)
                .unwrap()
                .and_time(
                    NaiveTime::from_hms_opt(ids.hour as u32, ids.minute as u32, ids.second as u32)
                        .unwrap(),
                ),
        );
        (self.current_product_id, self.current_band) = if pds.template_number == 50031 {
            // Special case (typhoon storm)
            let tmpl = ProductDefinitionTemplate4_50031::read(reader)?;
            assert_eq!(tmpl.background_process, 170);
            let datetime = match tmpl.indicator_of_unit_of_time_range_forecast {
                0 => reference_datetime + Duration::minutes(tmpl.forecast_time as i64),
                1 => reference_datetime + Duration::hours(tmpl.forecast_time as i64),
                2 => reference_datetime + Duration::days(tmpl.forecast_time as i64),
                v => unimplemented!("{}", v),
            };
            (
                GpvProductIdentifier {
                    kind: GpvProductElement::TyphoonStorm,
                    datetime,
                    reference_datetime,
                    generating_process: GeneratingProcessType::Forecast,
                    surface: FixedSurface::None,
                    ensemble: None,
                    variant: Some(format!("TC{}", tmpl.tc_number)),
                },
                0,
            )
        } else {
            // Set by the statistical templates, which carry the period their
            // field covers.
            let mut interval_end: Option<TimeInterval> = None;
            let (tmpl0, statistical_process, ensemble) = match pds.template_number {
                0 | 50000 => {
                    let tmpl0 = match pds.template_number {
                        0 => ProductDefinitionTemplate4_0::read(reader)?,
                        50000 => ProductDefinitionTemplate4_50000::read(reader)?.template_0,
                        _ => unreachable!(),
                    };
                    (tmpl0, None, None)
                }
                1 | 11 => {
                    let (tmpl1, stat_process) = match pds.template_number {
                        1 => (ProductDefinitionTemplate4_1::read(reader)?, None),
                        11 => {
                            let tmpl11 = ProductDefinitionTemplate4_11::read(reader)?;
                            let stat_process = first_statistical_process(&tmpl11.interval)?;
                            interval_end = Some(tmpl11.interval);
                            (tmpl11.template_1, Some(stat_process))
                        }
                        _ => unreachable!(),
                    };
                    let ensemble = Ensemble {
                        perturbation_number: tmpl1.perturbation_number,
                        ensemble_type: tmpl1.type_of_ensemble_forecast,
                    };
                    (tmpl1.template_0, stat_process, Some(ensemble))
                }
                8 | 50008 | 50009 | 50011 | 50012 => {
                    let tmpl8 = match pds.template_number {
                        8 | 50008 | 50009 | 50012 => ProductDefinitionTemplate4_8::read(reader)?,
                        50011 => ProductDefinitionTemplate4_50011::read(reader)?.template_8,
                        _ => unreachable!(),
                    };
                    let tmpl0 = tmpl8.template_0;
                    let stat_process = first_statistical_process(&tmpl8.interval)?;
                    interval_end = Some(tmpl8.interval);
                    (tmpl0, Some(stat_process), None)
                }
                _ => unreachable!("template 4.{:#?} is not supported yet", pds.template_number),
            };
            let datetime = valid_datetime(reference_datetime, &tmpl0, interval_end.as_ref())?;
            let gds_tmpl: &GridDefinitionTemplate3_0 = self.current_gds_tmpl.as_ref().unwrap();
            get_product_id_and_band(
                &tmpl0,
                gds_tmpl,
                statistical_process,
                reference_datetime,
                datetime,
                ensemble,
            )
        };

        self.products
            .entry(self.current_product_id.clone())
            .or_insert_with(|| ProductData {
                points: vec![],
                band_specs: self
                    .current_product_id
                    .bands()
                    .iter()
                    .map(|band_name| BandSpec {
                        name: band_name.to_string(),
                        ..Default::default()
                    })
                    .collect(),
                grid: None,
            });

        // Update latest reference_datetime for this kind
        let kind_path = self.current_product_id.kind_path();
        let ref_dt = self.current_product_id.reference_datetime;
        self.latest_reference_times
            .entry(kind_path)
            .and_modify(|existing| {
                if ref_dt > *existing {
                    *existing = ref_dt;
                }
            })
            .or_insert(ref_dt);

        Ok(())
    }

    fn handle_data_representation(
        &mut self,
        drs: tinygrib2::message::DataRepresentationSectionHeader,
        reader: &mut std::io::Take<&mut R>,
    ) -> tinygrib2::Result<()> {
        let product = self.products.get_mut(&self.current_product_id).unwrap();
        let band = &mut product.band_specs[self.current_band as usize];
        match drs.template_number {
            0 => {
                let tmpl = DataRepresentationTemplate5_0::read(reader)?;
                // ensure that the same product does not have different scale factors
                assert!(
                    (band.reference_value == 0.0 || band.reference_value == tmpl.reference_value)
                        && (band.binary_scale == 0
                            || band.binary_scale == tmpl.binary_scale_factor as i8)
                        && (band.decimal_scale == 0
                            || band.decimal_scale == tmpl.decimal_scale_factor as i8)
                );
                band.reference_value = tmpl.reference_value;
                band.binary_scale = tmpl.binary_scale_factor as i8;
                band.decimal_scale = tmpl.decimal_scale_factor as i8;
                self.current_drs_tmpl = Some(DataRepresentationTemplate::Template5_0(tmpl));
            }
            3 => {
                let tmpl = DataRepresentationTemplate5_3::read(reader)?;
                let tmpl0 = &tmpl.template_2.template_0;
                // ensure that the same product does not have different scale factors
                assert!(
                    band.reference_value == 0.0 || band.reference_value == tmpl0.reference_value
                );
                assert!(
                    band.binary_scale == 0 || band.binary_scale == tmpl0.binary_scale_factor as i8
                );
                assert!(
                    band.decimal_scale == 0
                        || band.decimal_scale == tmpl0.decimal_scale_factor as i8
                );
                band.reference_value = tmpl0.reference_value;
                band.binary_scale = tmpl0.binary_scale_factor as i8;
                band.decimal_scale = tmpl0.decimal_scale_factor as i8;
                self.current_drs_tmpl = Some(DataRepresentationTemplate::Template5_3(tmpl));
            }
            200 => {
                let tmpl = DataRepresentationTemplate5_200::read(reader)?;
                assert!(band.decimal_scale == 0 || band.decimal_scale == tmpl.decimal_scale_factor);
                band.reference_value = 0.0;
                band.binary_scale = 0;
                band.decimal_scale = tmpl.decimal_scale_factor;
                self.current_drs_tmpl = Some(DataRepresentationTemplate::Template5_200(tmpl));
            }
            _ => unimplemented!("template 5.{:?} is not supported yet", drs.template_number),
        }
        self.current_drs = Some(drs);
        Ok(())
    }

    fn handle_bitmap(
        &mut self,
        bitmap_header: tinygrib2::message::BitmapSectionHeader,
        reader: &mut std::io::Take<&mut R>,
    ) -> tinygrib2::Result<()> {
        match bitmap_header.bit_map_indicator {
            0 => {}
            254 => {
                return Ok(());
            }
            255 => {
                self.current_bitmap = None;
                return Ok(());
            }
            _ => unimplemented!(),
        }
        let gds = self.current_gds_tmpl.as_ref().unwrap();
        let num_points = gds.n_i as usize * gds.n_j as usize;
        self.current_bitmap = Some(read_bitmap(reader, num_points)?);
        Ok(())
    }

    fn handle_data(
        &mut self,
        data: tinygrib2::message::DataSectionHeader,
        reader: &mut std::io::Take<&mut R>,
    ) -> tinygrib2::Result<()> {
        let mut values = match self.current_drs_tmpl.as_ref().unwrap() {
            DataRepresentationTemplate::Template5_0(tmpl) => read_data_7_0(
                reader,
                self.current_drs.as_ref().unwrap().number_of_values,
                tmpl,
            )?,
            DataRepresentationTemplate::Template5_3(tmpl) => read_data_7_3(reader, tmpl)?,
            DataRepresentationTemplate::Template5_200(tmpl) => read_data_7_200(
                reader,
                data.body_len() as usize,
                self.current_drs.as_ref().unwrap().number_of_values,
                tmpl,
            )?,
        };
        self.current_product_id
            .translate_values(&mut values, self.current_band);

        // Keep parsing and decoding every section so malformed input is still
        // reported, but do not retain millions of points for a product the
        // caller has explicitly excluded. The empty ProductData entry created
        // from section 4 remains available for selector validation and errors.
        if !self.retains_values_for(&self.current_product_id) {
            return Ok(());
        }

        let gds_tmpl = self.current_gds_tmpl.as_ref().unwrap();
        let product = self.products.get(&self.current_product_id).unwrap();
        let grid = resolve_product_grid(&self.current_product_id, product, gds_tmpl)?;

        let (x_first, y_first, power) = {
            let grid_lng_0 = (grid.lng_0 * MICRODEGREES_PER_DEGREE) as i32;
            let grid_lat_0 = (grid.lat_0 * MICRODEGREES_PER_DEGREE) as i32;
            if gds_tmpl.lo1.min(gds_tmpl.lo2) < grid_lng_0
                || gds_tmpl.la1.min(gds_tmpl.la2) < grid_lat_0
            {
                return Err(unsupported_grid(
                    &self.current_product_id,
                    gds_tmpl,
                    format!(
                        "its lower-left extent precedes the configured origin ({}, {})",
                        grid.lng_0, grid.lat_0
                    ),
                ));
            }

            let width = grid_cell_width(&self.current_product_id, gds_tmpl, &grid)?;
            let power = width.ilog2() as u8;
            let corner = grid_corner(&self.current_product_id, gds_tmpl, &grid, width)?;
            if let Some((x_shift, y_shift)) = corner.snap
                && !self.warned_grid_snap
            {
                self.warned_grid_snap = true;
                tracing::warn!(
                    product = %self.current_product_id.path(),
                    width,
                    shift = format!("({x_shift}, {y_shift}) base cells"),
                    "grid corner is not a multiple of the cell width; snapping this product's grids west and south"
                );
            }
            validate_grid_extent(
                &self.current_product_id,
                gds_tmpl,
                corner.x,
                corner.y,
                width,
            )?;
            (corner.x, corner.y, power)
        };
        let width = 1 << power;
        let product = self
            .products
            .entry(self.current_product_id.clone())
            .or_default();
        product.grid = Some(grid);

        {
            let band = &mut product.band_specs[self.current_band as usize];
            use itertools::MinMaxResult;
            match values.iter().filter(|&v| *v != i32::MIN).minmax() {
                MinMaxResult::NoElements => {}
                MinMaxResult::OneElement(&val) => {
                    band.min = band.min.map_or(val, |v| v.min(val)).into();
                    band.max = band.max.map_or(val, |v| v.max(val)).into();
                }
                MinMaxResult::MinMax(&min, &max) => {
                    band.min = band.min.map_or(min, |v| v.min(min)).into();
                    band.max = band.max.map_or(max, |v| v.max(max)).into();
                }
            }
        }

        let mut value_iter = values.into_iter();

        if let Some(bitmap) = &self.current_bitmap {
            // bitmap is used
            let mut bitmap_iter = bitmap.iter();
            for j in 0..gds_tmpl.n_j {
                let y = match gds_tmpl.scanning_mode {
                    0 => y_first - j * width,
                    64 => y_first + j * width,
                    _ => unimplemented!("Unsupported scanning mode: {}", gds_tmpl.scanning_mode),
                } as u16;
                for i in 0..gds_tmpl.n_i {
                    let present = bitmap_iter.next().ok_or_else(|| {
                        tinygrib2::Error::InvalidData(format!(
                            "bitmap for {} has fewer entries than its {}x{} grid",
                            self.current_product_id.path(),
                            gds_tmpl.n_i,
                            gds_tmpl.n_j
                        ))
                    })?;
                    if !present {
                        continue;
                    };
                    let value = match value_iter.next().ok_or_else(|| {
                        tinygrib2::Error::InvalidData(format!(
                            "data for {} has fewer values than its bitmap",
                            self.current_product_id.path()
                        ))
                    })? {
                        i32::MIN => continue,
                        v => v,
                    };
                    product.points.push(PointValue {
                        x: (x_first + i * width) as u16,
                        y,
                        point_id: 0, // dummy
                        value,
                        band_idx: self.current_band,
                        point_power: power,
                    });
                }
            }
        } else {
            // no bitmap
            for j in 0..gds_tmpl.n_j {
                let y = match gds_tmpl.scanning_mode {
                    0 => y_first - j * width,
                    64 => y_first + j * width,
                    _ => unimplemented!("Unsupported scanning mode: {}", gds_tmpl.scanning_mode),
                } as u16;
                for i in 0..gds_tmpl.n_i {
                    let value = match value_iter.next().ok_or_else(|| {
                        tinygrib2::Error::InvalidData(format!(
                            "data for {} has fewer values than its {}x{} grid",
                            self.current_product_id.path(),
                            gds_tmpl.n_i,
                            gds_tmpl.n_j
                        ))
                    })? {
                        i32::MIN => continue,
                        v => v,
                    };
                    product.points.push(PointValue {
                        x: (x_first + i * width) as u16,
                        y,
                        point_id: 0, // dummy
                        value,
                        band_idx: self.current_band,
                        point_power: power,
                    });
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn grid_template(d_i: u32, d_j: u32) -> GridDefinitionTemplate3_0 {
        GridDefinitionTemplate3_0 {
            shape_of_earth: 0,
            scale_factor_of_radius: 0,
            scale_value_of_radius: 0,
            scale_factor_of_major_axis: 0,
            scale_value_of_major_axis: 0,
            scale_factor_of_minor_axis: 0,
            scale_value_of_minor_axis: 0,
            n_i: 0,
            n_j: 0,
            basic_angle: 0,
            subdivisions_of_basic_angle: 0,
            la1: 0,
            lo1: 0,
            resolution_and_component_flags: 0x30,
            la2: 0,
            lo2: 0,
            d_i,
            d_j,
            scanning_mode: 0,
        }
    }

    fn lfm_isobaric_wind() -> GpvProductIdentifier {
        GpvProductIdentifier {
            kind: GpvProductElement::LfmWind,
            surface: FixedSurface::IsobaricSurface(1000, 0),
            generating_process: GeneratingProcessType::Forecast,
            ..Default::default()
        }
    }

    #[test]
    fn read_bitmap_is_msb_first_and_truncates_partial_byte() {
        let mut reader = Cursor::new([0b1010_0110, 0b1100_0000, 0xff]);

        let bitmap = read_bitmap(&mut reader, 10).unwrap();

        assert_eq!(reader.position(), 2);
        assert_eq!(
            bitmap.iter().collect::<Vec<_>>(),
            vec![
                true, false, true, false, false, true, true, false, true, true
            ]
        );
    }

    #[test]
    fn a_finer_lfm_grid_is_derived_from_its_grib_increments() {
        let product_id = lfm_isobaric_wind();
        let template = grid_template(25_000, 20_000);
        let grid = resolve_product_grid(&product_id, &ProductData::default(), &template).unwrap();

        assert_eq!(grid.lng_denom, 40.0);
        assert_eq!(grid.lat_denom, 50.0);
        assert_eq!(grid_cell_width(&product_id, &template, &grid).unwrap(), 1);
    }

    /// A corner that does not sit on a cell boundary is snapped visibly rather
    /// than being shifted with no diagnostic.
    #[test]
    fn a_corner_off_the_cell_boundary_is_snapped_and_reported() {
        // A 0.05 degree base grid, so a 0.1 degree product has width 2 and
        // needs an even corner.
        let base = LngLatGrid {
            lng_0: 120.,
            lat_0: 20.,
            lng_denom: 20.,
            lat_denom: 20.,
        };
        let corner = |lo1, la1| {
            let mut template = grid_template(100_000, 100_000);
            template.lo1 = lo1;
            template.la1 = la1;
            template
        };
        let product_id = lfm_isobaric_wind();

        // 120.00 and 20.00 are two cells from the origin: aligned.
        let even = corner(120_000_000 - 1, 20_000_000 - 1);
        assert_eq!(
            grid_corner(&product_id, &even, &base, 2).unwrap(),
            GridCorner {
                x: 0,
                y: 0,
                snap: None,
            }
        );
        let further = corner(120_100_000 - 1, 20_100_000 - 1);
        assert_eq!(
            grid_corner(&product_id, &further, &base, 2).unwrap(),
            GridCorner {
                x: 2,
                y: 2,
                snap: None,
            }
        );

        // 120.05 is one base cell in, which a width-2 grid cannot start on.
        // It is snapped down rather than refused, because a supported product
        // lands here; the snap is what the warning reports.
        let odd = corner(120_050_000 - 1, 20_000_000 - 1);
        assert_eq!(
            grid_corner(&product_id, &odd, &base, 2).unwrap(),
            GridCorner {
                x: 0,
                y: 0,
                snap: Some((1, 0)),
            }
        );
        // At width 1 the same corner is exactly where it says it is.
        assert_eq!(
            grid_corner(&product_id, &odd, &base, 1).unwrap(),
            GridCorner {
                x: 1,
                y: 0,
                snap: None,
            }
        );

        // A corner before the origin is still refused outright.
        let before = corner(119_000_000 - 1, 20_000_000 - 1);
        assert!(grid_corner(&product_id, &before, &base, 1).is_err());
    }

    #[test]
    fn a_non_dyadic_grid_reports_an_error_instead_of_reaching_ilog2() {
        let product_id = lfm_isobaric_wind();
        let template = grid_template(150_000, 120_000);
        let grid = resolve_product_grid(&product_id, &ProductData::default(), &template).unwrap();

        let error = grid_cell_width(&product_id, &template, &grid).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("cell width 3 is not a power of two")
        );
    }

    fn interval(year: u16, month: u8, day: u8, hour: u8, minute: u8) -> TimeInterval {
        TimeInterval {
            year,
            month,
            day,
            hour,
            minute,
            second: 0,
            time_ranges: vec![tinygrib2::templates::TimeRange {
                total_number_of_data_values_missing: 0,
                statistical_process: 1,
                type_of_time_increment: 2,
                indicator_of_unit_of_time: 0,
                length_of_the_time_range: 60,
                indicator_of_unit_of_length_of_time_range: 0,
                time_increment: 0,
            }],
        }
    }

    fn forecast_template(forecast_time: i32, unit: u8) -> ProductDefinitionTemplate4_0 {
        ProductDefinitionTemplate4_0 {
            parameter_category: 1,
            parameter_number: 8,
            type_of_generating_process: 2,
            background_process: 0,
            generating_process_identifier: 0,
            hours_after_data_cutoff: 0,
            minutes_after_data_cutoff: 0,
            indicator_of_unit_of_time_range: unit,
            forecast_time,
            type_of_first_fixed_surface: 1,
            scale_factor_of_first_fixed_surface: 0,
            scaled_value_of_first_fixed_surface: 0,
            type_of_second_fixed_surface: 255,
            scale_factor_of_second_fixed_surface: 0,
            scaled_value_of_second_fixed_surface: 0,
        }
    }

    /// The two ways a valid time can be reached, chosen by whether the product
    /// covers a period. Testing the conversion alone let the caller go back to
    /// reading `forecast_time` without anything failing.
    #[test]
    fn a_period_ends_the_field_but_an_instant_is_offset_from_the_reference() {
        let reference = Utc.with_ymd_and_hms(2019, 10, 12, 9, 0, 0).unwrap();

        // A one-hour accumulation whose period runs 09:00-10:00 is valid at
        // 10:00, which is what `FH01` in the file name means. The forecast time
        // marks the start and must not be used.
        let accumulation = interval(2019, 10, 12, 10, 0);
        assert_eq!(
            valid_datetime(reference, &forecast_template(0, 0), Some(&accumulation)).unwrap(),
            Utc.with_ymd_and_hms(2019, 10, 12, 10, 0, 0).unwrap()
        );
        // The last message of the same file: period 14:00-15:00, so +6h.
        assert_eq!(
            valid_datetime(
                reference,
                &forecast_template(300, 0),
                Some(&interval(2019, 10, 12, 15, 0))
            )
            .unwrap(),
            Utc.with_ymd_and_hms(2019, 10, 12, 15, 0, 0).unwrap()
        );

        // An instantaneous product has no period, and is offset from the
        // reference by its forecast time.
        assert_eq!(
            valid_datetime(reference, &forecast_template(30, 0), None).unwrap(),
            Utc.with_ymd_and_hms(2019, 10, 12, 9, 30, 0).unwrap()
        );
        assert_eq!(
            valid_datetime(reference, &forecast_template(6, 1), None).unwrap(),
            Utc.with_ymd_and_hms(2019, 10, 12, 15, 0, 0).unwrap()
        );
        assert_eq!(
            valid_datetime(reference, &forecast_template(2, 2), None).unwrap(),
            Utc.with_ymd_and_hms(2019, 10, 14, 9, 0, 0).unwrap()
        );
    }

    /// A period that cannot be read is not the same as no period at all.
    /// Falling back to the forecast time would hand back a plausible time that
    /// is quietly a whole accumulation period wrong.
    #[test]
    fn an_unreadable_period_is_reported_rather_than_replaced() {
        let reference = Utc.with_ymd_and_hms(2019, 10, 12, 9, 0, 0).unwrap();
        let broken = interval(2019, 13, 40, 25, 0);

        let error = valid_datetime(reference, &forecast_template(0, 0), Some(&broken)).unwrap_err();
        assert!(
            matches!(error, tinygrib2::Error::InvalidData(_)),
            "expected InvalidData, got {error:?}"
        );
    }

    #[test]
    fn an_unsupported_time_unit_is_reported_rather_than_panicking() {
        let reference = Utc.with_ymd_and_hms(2019, 10, 12, 9, 0, 0).unwrap();
        let error = valid_datetime(reference, &forecast_template(1, 13), None).unwrap_err();
        assert!(matches!(error, tinygrib2::Error::UnsupportedData(_)));
    }

    /// A statistical template promises a time range; a truncated one used to
    /// index straight into an empty list.
    #[test]
    fn a_statistical_product_without_a_time_range_is_reported() {
        let mut empty = interval(2019, 10, 12, 10, 0);
        empty.time_ranges.clear();
        assert!(first_statistical_process(&empty).is_err());
        assert_eq!(
            first_statistical_process(&interval(2019, 10, 12, 10, 0)).unwrap(),
            1
        );
    }

    #[test]
    fn zero_grid_increments_report_an_explicit_error() {
        let product_id = lfm_isobaric_wind();
        let template = grid_template(0, 20_000);

        let error =
            resolve_product_grid(&product_id, &ProductData::default(), &template).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("grid increments must be positive")
        );
    }

    #[test]
    fn zero_point_counts_report_an_explicit_error() {
        let product_id = lfm_isobaric_wind();
        let mut template = grid_template(20_000, 20_000);
        template.n_j = 1;

        let error = validate_grid_extent(&product_id, &template, 0, 0, 1).unwrap_err();

        assert!(error.to_string().contains("longitude point count is zero"));
    }

    #[test]
    fn an_extent_outside_the_point_coordinate_range_is_refused() {
        let product_id = lfm_isobaric_wind();
        let mut template = grid_template(20_000, 20_000);
        template.n_i = 2;
        template.n_j = 1;

        let error =
            validate_grid_extent(&product_id, &template, u32::from(u16::MAX), 0, 1).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("exceeds the u16 coordinate range")
        );
    }

    #[test]
    fn a_southward_extent_cannot_underflow_the_grid_origin() {
        let product_id = lfm_isobaric_wind();
        let mut template = grid_template(20_000, 20_000);
        template.n_i = 1;
        template.n_j = 2;

        let error = validate_grid_extent(&product_id, &template, 0, 0, 1).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("latitude extent precedes the configured origin")
        );
    }

    #[test]
    fn retained_product_matches_exact_selector_parts() {
        let mut product_id = GpvProductIdentifier {
            kind: GpvProductElement::HiresNowcastIntensity,
            ..Default::default()
        };
        let reader = GridSquareMessageReader::retaining_product(Some("hrnowc/intensity"));

        assert!(reader.retains_values_for(&product_id));

        product_id.kind = GpvProductElement::HiresNowcastIntensityError;
        assert!(!reader.retains_values_for(&product_id));
    }

    #[test]
    fn no_retained_product_filter_keeps_every_product() {
        let reader = GridSquareMessageReader::retaining_product(None);
        let product_id = GpvProductIdentifier {
            kind: GpvProductElement::HiresNowcastIntensityError,
            ..Default::default()
        };

        assert!(reader.retains_values_for(&product_id));
    }
}
