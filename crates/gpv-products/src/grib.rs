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
        ProductDefinitionTemplate4_50031, read_data_7_0, read_data_7_3, read_data_7_200,
    },
};

use crate::model::BandSpec;
use crate::products::{
    Ensemble, FixedSurface, GeneratingProcessType, GpvProductElement, GpvProductIdentifier,
    get_product_id_and_band,
};
use crate::products::{PointValue, ProductData};

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

#[derive(Default)]
pub struct GridSquareMessageReader {
    ids: Option<IdentificationSectionHeader>,
    current_gds_tmpl: Option<GridDefinitionTemplate3_0>,
    current_product_id: GpvProductIdentifier,
    current_band: u8,
    current_drs: Option<DataRepresentationSectionHeader>,
    current_drs_tmpl: Option<DataRepresentationTemplate>,
    current_bitmap: Option<BitVec>,
    pub products: HashMap<GpvProductIdentifier, ProductData>,
    /// Track latest reference_datetime for each kind path
    pub latest_reference_times: HashMap<String, DateTime<Utc>>,
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
        assert_eq!(gds.template_number, 0);
        let tmpl = GridDefinitionTemplate3_0::read(reader)?;
        assert_eq!(tmpl.resolution_and_component_flags, 0x30);
        assert!(tmpl.scanning_mode == 0 || tmpl.scanning_mode == 64); // +i, -j
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
                            let stat_process = tmpl11.interval.time_ranges[0].statistical_process;
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
                    (
                        tmpl0,
                        Some(tmpl8.interval.time_ranges[0].statistical_process),
                        None,
                    )
                }
                _ => unreachable!("template 4.{:#?} is not supported yet", pds.template_number),
            };
            let datetime = match tmpl0.indicator_of_unit_of_time_range {
                0 => reference_datetime + Duration::minutes(tmpl0.forecast_time as i64),
                1 => reference_datetime + Duration::hours(tmpl0.forecast_time as i64),
                2 => reference_datetime + Duration::days(tmpl0.forecast_time as i64),
                v => unimplemented!("{}", v),
            };
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

        let gds_tmpl = self.current_gds_tmpl.as_ref().unwrap();
        let grid = self.current_product_id.grid();

        let (x_first, y_first, power) = {
            assert!(
                gds_tmpl.lo1.min(gds_tmpl.lo2) >= (grid.lng_0 * 1_000_000.) as i32,
                "violate: {} >= {}",
                gds_tmpl.lo1.min(gds_tmpl.lo2),
                grid.lng_0
            );
            assert!(
                gds_tmpl.la1.min(gds_tmpl.la2) >= (grid.lat_0 * 1_000_000.) as i32,
                "violate: {} >= {}",
                gds_tmpl.la1.min(gds_tmpl.la2),
                grid.lat_0
            );

            let x_first = ((gds_tmpl.lo1 as f64 + 1. - 1_000_000. * grid.lng_0)
                * grid.lng_denom as f64
                / 1_000_000.) as i32;
            let y_last = ((gds_tmpl.la1 as f64 + 1. - 1_000_000. * grid.lat_0)
                * grid.lat_denom as f64
                / 1_000_000.) as i32;
            let width = ((gds_tmpl.d_i as f64 + 1.) / (1_000_000. / grid.lng_denom as f64)) as i32;
            let power = width.ilog2() as u8;

            // Note: check grid alignment
            {
                let diff = (gds_tmpl.lo1 as f64 + 1. - 1_000_000. * grid.lng_0)
                    * grid.lng_denom as f64
                    * width as f64
                    / 1_000_000.;
                assert!(
                    diff.fract() < 0.01,
                    "invalid grid alignment: {diff} {width}",
                );
            }

            let x_first = (x_first - x_first % width) as u32;
            let y_last = (y_last - y_last % width) as u32;
            (x_first, y_last, power)
        };
        let width = 1 << power;
        let product = self
            .products
            .entry(self.current_product_id.clone())
            .or_default();

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
                    if !bitmap_iter.next().unwrap() {
                        continue;
                    };
                    let value = match value_iter.next().unwrap() {
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
                    let value = match value_iter.next().unwrap() {
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
}
