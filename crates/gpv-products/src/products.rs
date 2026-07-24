use chrono::{DateTime, Utc};
use tinygrib2::templates::{GridDefinitionTemplate3_0, ProductDefinitionTemplate4_0};

use crate::model::{Aggregation, BandSpec, LngLatGrid};

#[derive(Debug)]
pub struct PointValue {
    pub x: u16,
    pub y: u16,
    /// Hilbert-curve encoded point id
    pub point_id: u32,
    /// Point scale (power of 2) for mixed resolution data (e.g. Highres Nowcast)
    ///
    /// 0 = 1x, 2 = 4x
    pub point_power: u8,
    // Band index
    pub band_idx: u8,
    // Point value for this band
    pub value: i32,
}

#[derive(Debug, Default)]
pub struct ProductData {
    pub points: Vec<PointValue>,
    pub band_specs: Vec<BandSpec>,
}

pub fn get_product_id_and_band(
    tmpl0: &ProductDefinitionTemplate4_0,
    gds_tmpl: &GridDefinitionTemplate3_0,
    statistical_process: Option<u8>,
    reference_datetime: DateTime<Utc>,
    datetime: DateTime<Utc>,
    ensemble: Option<Ensemble>,
) -> (GpvProductIdentifier, u8) {
    use GpvProductElement::*;
    let generating_process = GeneratingProcessType::from(tmpl0.type_of_generating_process);
    let surface = {
        let v = tmpl0.scaled_value_of_first_fixed_surface;
        let s = tmpl0.scale_factor_of_first_fixed_surface;
        match tmpl0.type_of_first_fixed_surface {
            255 => FixedSurface::None,
            1 => FixedSurface::Surface,
            3 => FixedSurface::CloudTops,
            100 => FixedSurface::IsobaricSurface(v, s),
            101 => FixedSurface::Msl,
            103 => FixedSurface::Altitude(v, s),
            160 => FixedSurface::DepthBelowSeaLevel(v, s),
            200 => FixedSurface::TankTotal,
            201 => FixedSurface::Tank(v),
            n => unimplemented!("typeOfFirstFixedSurface {} is not yet supported", n),
        }
    };

    let (kind, band) = match (
        tmpl0.background_process,
        tmpl0.parameter_category,
        tmpl0.parameter_number,
        statistical_process,
    ) {
        // GSM-GPV (全球域)
        (2, 0, 0, _) => (GsmTemperature, 0),
        (2, 1, 1, _) => (GsmHumidity, 0),
        (2, 1, 8, _) => (GsmPrecipitation, 0),
        (2, 2, 2, _) => (GsmWind, 0),
        (2, 2, 3, _) => (GsmWind, 1),
        (2, 2, 8, _) => (GsmUpdraft, 0),
        (2, 3, 0, _) => (GsmPressure, 0),
        (2, 3, 1, _) => (GsmPressureMsl, 0),
        (2, 3, 5, _) => (GsmAltitude, 0),
        (2, 4, 7, _) => (GsmRadiation, 0),
        (2, 6, 1, _) => (GsmCloud, 0),
        (2, 6, 3, _) => (GsmCloud, 1),
        (2, 6, 4, _) => (GsmCloud, 2),
        (2, 6, 5, _) => (GsmCloud, 3),
        // 天気分布予報
        (31, 0, 0, Some(2)) => (BunpuMaxTemperature, 0),
        (31, 0, 0, Some(3)) => (BunpuMinTemperature, 0),
        (31, 1, 204, Some(1)) => (BunpuPrecipitation, 0),
        (31, 1, 233, Some(1)) => (BunpuSnowfall, 0),
        (31, 191, 192, Some(196)) => (BunpuWeather, 0),
        (31, 0, 0, None) => match tmpl0.hours_after_data_cutoff {
            // 天気分布予報
            3 => (BunpuTemperature, 0),
            // MSM-GPV
            0 => (MsmTemperature, 0),
            _ => unimplemented!(),
        },
        // MSM-GPV
        (31, 0, 0, _) => (MsmTemperature, 0),
        (31, 1, 1, _) => (MsmHumidity, 0),
        (31, 1, 8, _) => (MsmPrecipitation, 0),
        (31, 2, 2, _) => (MsmWind, 0),
        (31, 2, 3, _) => (MsmWind, 1),
        (31, 2, 8, _) => (MsmUpdraft, 0),
        (31, 3, 0, _) => (MsmPressure, 0),
        (31, 3, 1, _) => (MsmPressureMsl, 0),
        (31, 3, 5, _) => (MsmAltitude, 0),
        (31, 4, 7, _) => (MsmRadiation, 0),
        (31, 6, 1, _) => (MsmCloud, 0),
        (31, 6, 3, _) => (MsmCloud, 1),
        (31, 6, 4, _) => (MsmCloud, 2),
        (31, 6, 5, _) => (MsmCloud, 3),
        // LFM-GPV
        (41, 0, 0, _) => (LfmTemperature, 0),
        (41, 1, 1, _) => (LfmHumidity, 0),
        (41, 1, 8, _) => (LfmPrecipitation, 0),
        (41, 2, 2, _) => (LfmWind, 0),
        (41, 2, 3, _) => (LfmWind, 1),
        (41, 2, 8, _) => (LfmUpdraft, 0),
        (41, 3, 0, _) => (LfmPressure, 0),
        (41, 3, 1, _) => (LfmPressureMsl, 0),
        (41, 3, 5, _) => (LfmAltitude, 0),
        (41, 4, 7, _) => (LfmRadiation, 0),
        (41, 6, 1, _) => (LfmCloud, 0),
        (41, 6, 3, _) => (LfmCloud, 1),
        (41, 6, 4, _) => (LfmCloud, 2),
        (41, 6, 5, _) => (LfmCloud, 3),
        // 三十分大気解析GPV
        (52, 0, 0, _) => (Atm30minTemperature, 0),
        (52, 2, 2, _) => (Atm30minWind, 0),
        (52, 2, 3, _) => (Atm30minWind, 1),
        // メソアンサンブル (MEPS)
        (61, 0, 0, _) => (MepsTemperature, 0),
        (61, 1, 1, _) => (MepsHumidity, 0),
        (61, 1, 8, _) => (MepsPrecipitation, 0),
        (61, 2, 2, _) => (MepsWind, 0),
        (61, 2, 3, _) => (MepsWind, 1),
        (61, 2, 8, _) => (MepsUpdraft, 0),
        (61, 3, 0, _) => (MepsPressure, 0),
        (61, 3, 1, _) => (MepsPressureMsl, 0),
        (61, 3, 5, _) => (MepsAltitude, 0),
        (61, 4, 7, _) => (MepsRadiation, 0),
        // 解析雨量・降水短時間予報・降水15時間予報
        // 速報版解析雨量・速報版降水短時間予報
        (150, 1, 200, _) => {
            if tmpl0.forecast_time < 360 {
                (Precipitaiton, 0)
            } else {
                (Precipitaiton15h, 0)
            }
        }
        // 土壌雨量指数
        (150, 1, 206, _) => (Dojoshisu, 0),
        // 土壌雨量指数
        (150, 1, 215, _) => (Hyomenshisu, 0),
        // 大雨警報（浸水害）・洪水警報の危険度分布 (浸水キキクル、洪水キキクル)
        (150, 1, 216, _) => (KikikuruInundation, 0),
        (150, 1, 217, _) => (KikikuruFlood, 0),
        (150, 1, 218, _) => (KikikuruTougou, 0),
        // 解析積雪深・解析降雪量・降雪短時間予報
        (150, 1, 232, _) => (SnowDepth, 0),
        (150, 1, 233, _) => (Snowfall, 0),
        // 大雨警報（土砂災害）の危険度分布 (土砂キキクル)
        (160, 1, 208, _) => (KikikuruDosha, 0),
        // 高解像度降水ナウキャスト or 5分毎全国合成レーダー降水強度・エコー頂高度GPV
        (151, 1, 8, Some(1)) => (HiresNowcastPrecip, 0),
        (151, 1, 8, Some(195)) => (HiresNowcastPrecipError, 0),
        (151, 1, 203, Some(196)) => (HiresNowcastIntensity, 0),
        (151, 1, 214, Some(196)) => (HiresNowcastIntensityError, 0),
        (151, 15, 192, _) => (NowcastEchoTops, 0),
        // 竜巻発生確度ナウキャスト
        (153, 193, 0, _) => (TornadoNowcast, 0),
        // 雷ナウキャスト
        (154, 193, 1, _) => (ThunderNowcast, 0),
        // 台風の暴風域に入る確率 (格子点値)
        (170, 11, 192, _) => (TyphoonStorm, 0),
        // 推計気象分布
        (205, 0, 0, _) => (SuikeiTemperature, 0),
        (205, 6, 33, _) => (SuikeiSunshine, 0),  // value
        (205, 6, 194, _) => (SuikeiSunshine, 1), // error
        (205, 191, 192, _) => (SuikeiWeather, 0),
        (210, 3, 0, _) => match tmpl0.type_of_generating_process {
            // 北西太平洋高解像度日別海面水温解析格子点資料
            0 => (SstDaily, 0),
            // 北西太平洋海面水温予報格子点資料
            1 | 2 => (Sst, 0),
            // ひまわりによる海面水温格子点資料
            8 => (SstHimawari, 0),
            _ => unimplemented!(),
        },
        // 本近海海流予報格子点資料
        (210, 1, 2, _) => (Current, 0), // u
        (210, 1, 3, _) => (Current, 1), // v
        // 日本沿岸海洋監視予測システムGPV (MOVE-JPN)
        (215, 1, 2, _) => match gds_tmpl.lo2 > 180_000_000 {
            true => (OceanNpCurrent, 0),  // u
            false => (OceanJpCurrent, 0), // u
        },
        (215, 1, 3, _) => match gds_tmpl.lo2 > 180_000_000 {
            true => (OceanNpCurrent, 1),  // v
            false => (OceanJpCurrent, 1), // v
        },
        (215, 3, 1, _) => match gds_tmpl.lo2 > 180_000_000 {
            true => (OceanNpHeight, 0),
            false => (OceanJpHeight, 0),
        },
        (215, 4, 15, _) => match gds_tmpl.lo2 > 180_000_000 {
            true => (OceanNpTemperature, 0),
            false => (OceanJpTemperature, 0),
        },
        (215, 4, 192, _) => match gds_tmpl.lo2 > 180_000_000 {
            true => (OceanNpSalinity, 0),
            false => (OceanJpSalinity, 0),
        },
        // 日本沿岸海況監視予測システム海氷GPV
        (215, 2, 0, _) => (OceanJpIceCover, 0),
        (215, 2, 1, _) => (OceanJpIceThickness, 0),
        (215, 2, 4, _) => (OceanJpIceDrift, 0),
        (215, 2, 5, _) => (OceanJpIceDrift, 1),
        // 全球波浪数値予報モデルGPV (GWM)
        (220, 0, 3, _) => (GwmWave, 0),  // height
        (220, 0, 10, _) => (GwmWave, 1), // direction
        (220, 0, 11, _) => (GwmWave, 2), // period
        (220, 2, 2, _) => (GwmWind, 0),
        (220, 2, 3, _) => (GwmWind, 1),
        (220, 0, 5, _) => (GwmWindWave, 0), // height
        (220, 0, 4, _) => (GwmWindWave, 1), // direction
        (220, 0, 6, _) => (GwmWindWave, 2), // period
        (220, 0, 47, _) => (GwmSwell1, 0),  // height
        (220, 0, 53, _) => (GwmSwell1, 1),  // direction
        (220, 0, 50, _) => (GwmSwell1, 2),  // period
        (220, 0, 48, _) => (GwmSwell2, 0),  // height
        (220, 0, 54, _) => (GwmSwell2, 1),  // direction
        (220, 0, 51, _) => (GwmSwell2, 2),  // period
        // 沿岸波浪モデルGPV (CWM)
        (221, 0, 3, _) => (CwmWave, 0),  // height
        (221, 0, 10, _) => (CwmWave, 1), // direction
        (221, 0, 11, _) => (CwmWave, 2), // period
        (221, 2, 2, _) => (CwmWind, 0),
        (221, 2, 3, _) => (CwmWind, 1),
        (221, 0, 5, _) => (CwmWindWave, 0), // height
        (221, 0, 4, _) => (CwmWindWave, 1), // direction
        (221, 0, 6, _) => (CwmWindWave, 2), // period
        (221, 0, 47, _) => (CwmSwell1, 0),  // height
        (221, 0, 53, _) => (CwmSwell1, 1),  // direction
        (221, 0, 50, _) => (CwmSwell1, 2),  // period
        (221, 0, 48, _) => (CwmSwell2, 0),  // height
        (221, 0, 54, _) => (CwmSwell2, 1),  // direction
        (221, 0, 51, _) => (CwmSwell2, 2),  // period
        // 波浪アンサンブル数値予報モデルGPV (WEM)
        (223, 0, 3, _) => (WemWave, 0),  // height
        (223, 0, 10, _) => (WemWave, 1), // direction
        (223, 0, 11, _) => (WemWave, 2), // period
        // 高潮予測GPV
        (225, 2, 2, _) => (TideWind, 0),
        (225, 2, 3, _) => (TideWind, 1),
        (225, 3, 1, _) => (TidePressure, 0),
        (225, 3, 200, _) => (TideAstronomical, 0),
        (225, 3, 201, _) => match tmpl0.type_of_generating_process {
            40 => (TideGuidance, 0),
            _ => (Tide, 0),
        },
        // 黄砂解析予測モデルGPV
        (250, 13, 192, _) => (KousaLow, 0),
        (250, 13, 193, _) => (KousaColumn, 0),
        // 紫外線情報
        (251, 14, 0, _) => (Ozone, 0),
        (252, 4, 50, _) => (UvIndexClear, 0),
        (252, 4, 51, _) => (UvIndex, 0),
        // 高分解能雲情報
        (255, 6, 201, _) => (HiresCloud, 0),
        (255, 6, 202, _) => (HiresCloudIce, 0),
        (255, 6, 12, _) => (HrCloudAltitude, 0),
        (255, 6, 8, _) => (HiresCloudType, 0),
        (255, 6, 200, _) => (HiresCloudQc, 0),
        _ => unimplemented!("{:#?}", tmpl0),
    };

    let product = GpvProductIdentifier {
        kind,
        datetime,
        reference_datetime,
        generating_process,
        surface,
        ensemble,
        variant: None,
    };
    (product, band)
}

pub fn get_product_by_data_kind_and_value_kind(
    data_kind: &str,
    value_kind: &str,
) -> GpvProductElement {
    use GpvProductElement::*;
    match (data_kind, value_kind) {
        // ATM 30min
        ("atm30min", "temp") => Atm30minTemperature,
        ("atm30min", "wind") => Atm30minWind,

        // Bunpu (天気分布予報)
        ("tenkibunpu", "maxtemp") => BunpuMaxTemperature,
        ("tenkibunpu", "mintemp") => BunpuMinTemperature,
        ("tenkibunpu", "precip") => BunpuPrecipitation,
        ("tenkibunpu", "snowfall") => BunpuSnowfall,
        ("tenkibunpu", "temp") => BunpuTemperature,
        ("tenkibunpu", "weather") => BunpuWeather,

        // Current
        ("current", "current") => Current,

        // CWM (沿岸波浪モデル)
        ("cwm", "swell1") => CwmSwell1,
        ("cwm", "swell2") => CwmSwell2,
        ("cwm", "wave") => CwmWave,
        ("cwm", "wind") => CwmWind,
        ("cwm", "windwave") => CwmWindWave,

        // 土壌雫
        ("dojoshisu", "") => Dojoshisu,

        // GSM (全球スペクトルモデル)
        ("gsm", "altitude") => GsmAltitude,
        ("gsm", "cloud") => GsmCloud,
        ("gsm", "humidity") => GsmHumidity,
        ("gsm", "precip") => GsmPrecipitation,
        ("gsm", "pressure") => GsmPressure,
        ("gsm", "pressure-msl") => GsmPressureMsl,
        ("gsm", "radiation") => GsmRadiation,
        ("gsm", "temp") => GsmTemperature,
        ("gsm", "updraft") => GsmUpdraft,
        ("gsm", "wind") => GsmWind,

        // GWM (全球波浪モデル)
        ("gwm", "swell1") => GwmSwell1,
        ("gwm", "swell2") => GwmSwell2,
        ("gwm", "wave") => GwmWave,
        ("gwm", "wind") => GwmWind,
        ("gwm", "windwave") => GwmWindWave,

        // 高解像度ナウキャスト
        ("hrnowc", "intensity") => HiresNowcastIntensity,
        ("hrnowc", "intensity-error") => HiresNowcastIntensityError,
        ("hrnowc", "precip") => HiresNowcastPrecip,
        ("hrnowc", "precip-error") => HiresNowcastPrecipError,
        ("hrnowc", "echotops") => NowcastEchoTops,

        // 高解像度雲
        ("hrcloud", "cloud") => HiresCloud,
        ("hrcloud", "altitude") => HrCloudAltitude,
        ("hrcloud", "type") => HiresCloudType,
        ("hrcloud", "ice") => HiresCloudIce,
        ("hrcloud", "qc") => HiresCloudQc,

        // 表面雫
        ("hyomenshisu", "") => Hyomenshisu,

        // 危険度
        ("kikikuru-dosha", "") => KikikuruDosha,
        ("kikikuru", "flood") => KikikuruFlood,
        ("kikikuru", "inundation") => KikikuruInundation,
        ("kikikuru", "tougou") => KikikuruTougou,

        // 降水短時間予報
        ("kousa", "column") => KousaColumn,
        ("kousa", "low") => KousaLow,

        // LFM (局地数値予報モデル)
        ("lfm", "altitude") => LfmAltitude,
        ("lfm", "cloud") => LfmCloud,
        ("lfm", "humidity") => LfmHumidity,
        ("lfm", "precip") => LfmPrecipitation,
        ("lfm", "pressure") => LfmPressure,
        ("lfm", "mslpressure") => LfmPressureMsl,
        ("lfm", "radiation") => LfmRadiation,
        ("lfm", "temp") => LfmTemperature,
        ("lfm", "updraft") => LfmUpdraft,
        ("lfm", "wind") => LfmWind,

        // MEPS (メソアンサンブル)
        ("meps", "altitude") => MepsAltitude,
        ("meps", "humidity") => MepsHumidity,
        ("meps", "precip") => MepsPrecipitation,
        ("meps", "pressure") => MepsPressure,
        ("meps", "mslpressure") => MepsPressureMsl,
        ("meps", "radiation") => MepsRadiation,
        ("meps", "temp") => MepsTemperature,
        ("meps", "updraft") => MepsUpdraft,
        ("meps", "wind") => MepsWind,

        // MSM (メソスケールモデル)
        ("msm", "altitude") => MsmAltitude,
        ("msm", "cloud") => MsmCloud,
        ("msm", "humidity") => MsmHumidity,
        ("msm", "precip") => MsmPrecipitation,
        ("msm", "pressure") => MsmPressure,
        ("msm", "mslpressure") => MsmPressureMsl,
        ("msm", "radiation") => MsmRadiation,
        ("msm", "temp") => MsmTemperature,
        ("msm", "updraft") => MsmUpdraft,
        ("msm", "wind") => MsmWind,

        // 海洋データ
        ("ocean-jp-ice", "cover") => OceanJpIceCover,
        ("ocean-jp-ice", "drift") => OceanJpIceDrift,
        ("ocean-jp-ice", "thickness") => OceanJpIceThickness,
        ("ocean-jp", "current") => OceanJpCurrent,
        ("ocean-jp", "height") => OceanJpHeight,
        ("ocean-jp", "salinity") => OceanJpSalinity,
        ("ocean-jp", "temp") => OceanJpTemperature,
        ("ocean-np", "current") => OceanNpCurrent,
        ("ocean-np", "height") => OceanNpHeight,
        ("ocean-np", "salinity") => OceanNpSalinity,
        ("ocean-np", "temp") => OceanNpTemperature,

        // 降水量
        ("precipitation", "") => Precipitaiton,
        ("precipitation-15h", "") => Precipitaiton15h,

        // 積雪
        ("snow", "snowdepth") => SnowDepth,
        ("snow", "snowfall") => Snowfall,

        // 海面水温
        ("sst", "temp") => Sst,
        ("sst-daily", "temp") => SstDaily,
        ("sst-himawari", "temp") => SstHimawari,

        // 推計気象分布
        ("suikei", "sunshine") => SuikeiSunshine,
        ("suikei", "temp") => SuikeiTemperature,
        ("suikei", "weather") => SuikeiWeather,

        // 雷ナウキャスト
        ("thunder-nowc", "") => ThunderNowcast,

        // 潮汐
        ("tide", "tide") => Tide,
        ("tide", "astronomical") => TideAstronomical,
        ("tide-guidance", "guidance") => TideGuidance,
        ("tide", "pressure") => TidePressure,
        ("tide", "wind") => TideWind,

        // 竜巻ナウキャスト
        ("tornado-nowc", "") => TornadoNowcast,

        // 台風
        ("typhoon-storm", "") => TyphoonStorm,

        // 紫外線
        ("uv", "uvi") => UvIndex,
        ("uv", "uvic") => UvIndexClear,
        ("uv", "ozone") => Ozone,

        // 波浪
        ("wem", "wave") => WemWave,

        // 不明な組み合わせ
        _ => Unknown,
    }
}

pub fn get_surface_by_surface_str(surface_str: Option<&str>) -> FixedSurface {
    use FixedSurface::*;
    if let Some(surface_suffix) = surface_str {
        match surface_suffix {
            "" => None,
            "surf" => Surface,
            "msl" => Msl,
            "ct" => CloudTops,
            "tank-total" => TankTotal,
            _ => {
                // Handle pattern-based suffixes
                if let Some(alt_str) = surface_suffix.strip_prefix("alt")
                    && let Ok(value) = alt_str.parse::<u32>()
                {
                    return Altitude(value, 0);
                }

                if let Some(p_str) = surface_suffix.strip_prefix("p")
                    && let Ok(value) = p_str.parse::<u32>()
                {
                    return IsobaricSurface(value, 0);
                }

                if let Some(dsl_str) = surface_suffix.strip_prefix("dsl") {
                    // Handle depth below sea level format: "dsl123p45" -> 123.45
                    let value_str = dsl_str.replace("p", ".");
                    if let Ok(float_value) = value_str.parse::<f32>() {
                        // Convert back to original format with scale factor
                        let scale_factor = if float_value.fract() == 0.0 {
                            0
                        } else {
                            // Calculate scale factor based on decimal places
                            let decimal_places = format!("{}", float_value.fract()).len() - 2; // subtract "0."
                            decimal_places as i8
                        };
                        let scaled_value = (float_value * 10f32.powi(scale_factor as i32)) as u32;
                        return DepthBelowSeaLevel(scaled_value, scale_factor);
                    }
                }

                if let Some(tank_str) = surface_suffix.strip_prefix("tank-")
                    && let Ok(value) = tank_str.parse::<u32>()
                {
                    return Tank(value);
                }

                // Return None for unknown suffixes
                None
            }
        }
    } else {
        None
    }
}

pub fn get_generating_process_by_generating_process_str(
    generating_process_str: &str,
) -> GeneratingProcessType {
    use GeneratingProcessType::*;
    match generating_process_str {
        "analysis" => Analysis,
        "init" => Initialization,
        "forecast" => Forecast,
        "ens-forecast" => EnsembleForecast,
        "observation" => Observation,
        _ => {
            // Handle "other-{v}" pattern
            if let Some(other_str) = generating_process_str.strip_prefix("other-")
                && let Ok(value) = other_str.parse::<u8>()
            {
                return Other(value);
            }
            // Return Missing for unknown strings
            Missing
        }
    }
}

#[derive(Default, Debug, PartialEq, Eq, Hash, Clone)]
pub struct GpvProductIdentifier {
    pub kind: GpvProductElement,
    pub datetime: DateTime<Utc>,
    pub reference_datetime: DateTime<Utc>,
    pub generating_process: GeneratingProcessType,
    pub surface: FixedSurface,
    pub ensemble: Option<Ensemble>,
    /// Other variant specifier
    pub variant: Option<String>,
}

impl GpvProductIdentifier {
    /// Gets product path parts as (base_prefix, suffix)
    pub fn path_parts(&self) -> (&str, &str) {
        use GpvProductElement::*;
        match &self.kind {
            Unknown => unreachable!(),
            Atm30minTemperature => ("atm30min", "temp"),
            Atm30minWind => ("atm30min", "wind"),
            BunpuMaxTemperature => ("tenkibunpu", "maxtemp"),
            BunpuMinTemperature => ("tenkibunpu", "mintemp"),
            BunpuPrecipitation => ("tenkibunpu", "precip"),
            BunpuSnowfall => ("tenkibunpu", "snowfall"),
            BunpuTemperature => ("tenkibunpu", "temp"),
            BunpuWeather => ("tenkibunpu", "weather"),
            Current => ("current", "current"),
            CwmSwell1 => ("cwm", "swell1"),
            CwmSwell2 => ("cwm", "swell2"),
            CwmWave => ("cwm", "wave"),
            CwmWind => ("cwm", "wind"),
            CwmWindWave => ("cwm", "windwave"),
            Dojoshisu => ("dojoshisu", ""),
            GsmAltitude => ("gsm", "altitude"),
            GsmCloud => ("gsm", "cloud"),
            GsmHumidity => ("gsm", "humidity"),
            GsmPrecipitation => ("gsm", "precip"),
            GsmPressure => ("gsm", "pressure"),
            GsmPressureMsl => ("gsm", "pressure-msl"),
            GsmRadiation => ("gsm", "radiation"),
            GsmTemperature => ("gsm", "temp"),
            GsmUpdraft => ("gsm", "updraft"),
            GsmWind => ("gsm", "wind"),
            GwmSwell1 => ("gwm", "swell1"),
            GwmSwell2 => ("gwm", "swell2"),
            GwmWave => ("gwm", "wave"),
            GwmWind => ("gwm", "wind"),
            GwmWindWave => ("gwm", "windwave"),
            HiresNowcastIntensity => ("hrnowc", "intensity"),
            HiresNowcastIntensityError => ("hrnowc", "intensity-error"),
            HiresNowcastPrecip => ("hrnowc", "precip"),
            HiresNowcastPrecipError => ("hrnowc", "precip-error"),
            HiresCloud => ("hrcloud", "cloud"),
            HrCloudAltitude => ("hrcloud", "altitude"),
            HiresCloudType => ("hrcloud", "type"),
            HiresCloudIce => ("hrcloud", "ice"),
            HiresCloudQc => ("hrcloud", "qc"),
            Hyomenshisu => ("hyomenshisu", ""),
            KikikuruDosha => ("kikikuru-dosha", ""),
            KikikuruFlood => ("kikikuru", "flood"),
            KikikuruInundation => ("kikikuru", "inundation"),
            KikikuruTougou => ("kikikuru", "tougou"),
            KousaColumn => ("kousa", "column"),
            KousaLow => ("kousa", "low"),
            LfmAltitude => ("lfm", "altitude"),
            LfmCloud => ("lfm", "cloud"),
            LfmHumidity => ("lfm", "humidity"),
            LfmPrecipitation => ("lfm", "precip"),
            LfmPressure => ("lfm", "pressure"),
            LfmPressureMsl => ("lfm", "mslpressure"),
            LfmRadiation => ("lfm", "radiation"),
            LfmTemperature => ("lfm", "temp"),
            LfmUpdraft => ("lfm", "updraft"),
            LfmWind => ("lfm", "wind"),
            MepsAltitude => ("meps", "altitude"),
            MepsHumidity => ("meps", "humidity"),
            MepsPrecipitation => ("meps", "precip"),
            MepsPressure => ("meps", "pressure"),
            MepsPressureMsl => ("meps", "mslpressure"),
            MepsRadiation => ("meps", "radiation"),
            MepsTemperature => ("meps", "temp"),
            MepsUpdraft => ("meps", "updraft"),
            MepsWind => ("meps", "wind"),
            MsmAltitude => ("msm", "altitude"),
            MsmCloud => ("msm", "cloud"),
            MsmHumidity => ("msm", "humidity"),
            MsmPrecipitation => ("msm", "precip"),
            MsmPressure => ("msm", "pressure"),
            MsmPressureMsl => ("msm", "mslpressure"),
            MsmRadiation => ("msm", "radiation"),
            MsmTemperature => ("msm", "temp"),
            MsmUpdraft => ("msm", "updraft"),
            MsmWind => ("msm", "wind"),
            NowcastEchoTops => ("hrnowc", "echotops"),
            OceanJpIceCover => ("ocean-jp-ice", "cover"),
            OceanJpIceDrift => ("ocean-jp-ice", "drift"),
            OceanJpIceThickness => ("ocean-jp-ice", "thickness"),
            OceanJpCurrent => ("ocean-jp", "current"),
            OceanJpHeight => ("ocean-jp", "height"),
            OceanJpSalinity => ("ocean-jp", "salinity"),
            OceanJpTemperature => ("ocean-jp", "temp"),
            OceanNpCurrent => ("ocean-np", "current"),
            OceanNpHeight => ("ocean-np", "height"),
            OceanNpSalinity => ("ocean-np", "salinity"),
            OceanNpTemperature => ("ocean-np", "temp"),
            Precipitaiton => ("precipitation", ""),
            Precipitaiton15h => ("precipitation-15h", ""),
            SnowDepth => ("snow", "snowdepth"),
            Snowfall => ("snow", "snowfall"),
            Sst => ("sst", "temp"),
            SstDaily => ("sst-daily", "temp"),
            SstHimawari => ("sst-himawari", "temp"),
            SuikeiSunshine => ("suikei", "sunshine"),
            SuikeiTemperature => ("suikei", "temp"),
            SuikeiWeather => ("suikei", "weather"),
            ThunderNowcast => ("thunder-nowc", ""),
            Tide => ("tide", "tide"),
            TideAstronomical => ("tide", "astronomical"),
            TideGuidance => ("tide-guidance", "guidance"),
            TidePressure => ("tide", "pressure"),
            TideWind => ("tide", "wind"),
            TornadoNowcast => ("tornado-nowc", ""),
            TyphoonStorm => ("typhoon-storm", ""),
            UvIndex => ("uv", "uvi"),
            UvIndexClear => ("uv", "uvic"),
            Ozone => ("uv", "ozone"),
            WemWave => ("wem", "wave"),
        }
    }

    /// Gets the "{generating_process}/{kind}/" path
    pub fn kind_path(&self) -> String {
        let gen_process = self.generating_process.prefix();
        let (kind, _) = self.path_parts();
        format!("{gen_process}/{kind}/")
    }

    /// Gets the full path of the product
    pub fn path(&self) -> String {
        let kind_path = self.kind_path();
        let reference_datetime = &self.reference_datetime.format("%Y%m%d%H%M");
        let target_datetime = &self.datetime.format("%Y%m%d%H%M");
        let surface = self
            .surface
            .suffix()
            .map_or("".to_string(), |s| format!("_{s}"));
        let path = format!("{kind_path}{reference_datetime}/{target_datetime}{surface}");
        let (_, subproduct) = self.path_parts();
        let path = match subproduct {
            "" => path,
            s => format!("{path}_{s}"),
        };
        let path = match &self.variant {
            Some(v) => format!("{path}_{v}"),
            None => path,
        };
        match self.ensemble {
            Some(p) => format!("{}-ens{}-{}", path, p.ensemble_type, p.perturbation_number),
            None => path,
        }
    }

    pub fn bands(&self) -> Vec<&str> {
        use GpvProductElement::*;
        match self.kind {
            GsmWind | MsmWind | LfmWind | CwmWind | Atm30minWind | TideWind | MepsWind
            | Current | OceanJpCurrent | OceanNpCurrent | OceanJpIceDrift => {
                vec!["u", "v"]
            }
            GsmCloud | MsmCloud | LfmCloud => vec!["total", "low", "medium", "high"],
            SuikeiSunshine => vec!["value", "error"],
            GwmWave | GwmWindWave | GwmSwell1 | GwmSwell2 | CwmWave | CwmWindWave | CwmSwell1
            | CwmSwell2 | WemWave => {
                vec!["height", "direction", "period"]
            }
            _ => vec!["value"],
        }
    }

    /// Recommended aggregation method for the product
    pub fn aggregation(&self) -> Aggregation {
        use GpvProductElement::*;
        match self.kind {
            BunpuMinTemperature => Aggregation::Min,
            Atm30minTemperature | Atm30minWind | BunpuTemperature | Current | CwmSwell1
            | CwmSwell2 | CwmWave | CwmWind | CwmWindWave | GsmAltitude | GsmCloud
            | GsmHumidity | GsmPrecipitation | GsmPressure | GsmPressureMsl | GsmRadiation
            | GsmTemperature | GsmWind | GwmSwell1 | GwmSwell2 | GwmWave | GwmWind
            | GwmWindWave | HrCloudAltitude | LfmAltitude | LfmCloud | LfmHumidity
            | LfmPrecipitation | LfmPressure | LfmPressureMsl | LfmRadiation | LfmTemperature
            | LfmWind | MepsAltitude | MepsHumidity | MepsPrecipitation | MepsPressure
            | MepsPressureMsl | MepsRadiation | MepsTemperature | MepsWind | MsmAltitude
            | MsmCloud | MsmHumidity | MsmPrecipitation | MsmPressure | MsmPressureMsl
            | MsmRadiation | MsmTemperature | MsmWind | OceanJpCurrent | OceanJpHeight
            | OceanJpSalinity | OceanJpTemperature | OceanNpCurrent | OceanNpHeight
            | OceanNpSalinity | OceanNpTemperature | Sst | SstDaily | SstHimawari
            | SuikeiSunshine | SuikeiTemperature | TidePressure | TideWind | WemWave
            | OceanJpIceDrift | NowcastEchoTops => Aggregation::RoughAvg,
            HiresCloudQc => Aggregation::BitOr,
            _ => Aggregation::Max,
        }
    }

    pub fn grid(&self) -> &LngLatGrid {
        use GpvProductElement::*;
        match &self.kind {
            Unknown => unreachable!(),
            HiresNowcastIntensity
            | HiresNowcastIntensityError
            | HiresNowcastPrecip
            | HiresNowcastPrecipError => &LngLatGrid {
                lng_0: 120. + 1. / 320. / 2.,
                lat_0: 20. + 1. / 480. / 2.,
                lng_denom: 320.,
                lat_denom: 480.,
            },
            Dojoshisu | Hyomenshisu | TideGuidance | Tide | TideAstronomical | TidePressure
            | TideWind | ThunderNowcast | KikikuruDosha | KikikuruFlood | KikikuruInundation
            | KikikuruTougou | SuikeiTemperature | SuikeiWeather | SuikeiSunshine
            | NowcastEchoTops => &LngLatGrid {
                lng_0: 110. + 1. / 80. / 2.,
                lat_0: 10. + 1. / 120. / 2.,
                lng_denom: 80.,
                lat_denom: 120.,
            },
            Precipitaiton => &LngLatGrid {
                lng_0: 110. + 1. / 80. / 2.,
                lat_0: 10. + 1. / 120. / 2.,
                lng_denom: 80.,
                lat_denom: 120.,
            },
            Precipitaiton15h => &LngLatGrid {
                lng_0: 110. + 1. / 16. / 2.,
                lat_0: 10. + 1. / 20. / 2.,
                lng_denom: 16.,
                lat_denom: 20.,
            },
            SstHimawari => &LngLatGrid {
                lng_0: 110. + 1. / 50. / 2.,
                lat_0: 0. + 1. / 50. / 2.,
                lng_denom: 50.,
                lat_denom: 50.,
            },
            HiresCloud | HiresCloudIce | HrCloudAltitude | HiresCloudType | HiresCloudQc => {
                &LngLatGrid {
                    lng_0: 110.,
                    lat_0: 0.,
                    lng_denom: 50.,
                    lat_denom: 50.,
                }
            }
            LfmTemperature | LfmHumidity | LfmPrecipitation | LfmAltitude | LfmCloud
            | LfmPressure | LfmPressureMsl | LfmRadiation | LfmWind | LfmUpdraft => {
                match self.surface {
                    FixedSurface::IsobaricSurface(_, _) => &LngLatGrid {
                        lng_0: 120.,
                        lat_0: 20.,
                        lng_denom: 20.,
                        lat_denom: 25.,
                    },
                    _ => &LngLatGrid {
                        lng_0: 120.,
                        lat_0: 20.,
                        lng_denom: 40.,
                        lat_denom: 50.,
                    },
                }
            }
            Atm30minTemperature | Atm30minWind => &LngLatGrid {
                lng_0: 120.,
                lat_0: 20.,
                lng_denom: 40.,
                lat_denom: 50.,
            },
            OceanJpHeight | OceanJpSalinity | OceanJpTemperature | OceanJpIceCover
            | OceanJpIceThickness => &LngLatGrid {
                lng_0: 117. - 1. / 33.,
                lat_0: 20. - 1. / 50.,
                lng_denom: 33.,
                lat_denom: 50.,
            },
            OceanJpCurrent | OceanJpIceDrift => &LngLatGrid {
                lng_0: 117. - 3. / 33. / 2.,
                lat_0: 20. - 3. / 50. / 2.,
                lng_denom: 33.,
                lat_denom: 50.,
            },
            MsmTemperature | MsmHumidity | MsmPrecipitation | MsmAltitude | MsmCloud
            | MsmPressure | MsmPressureMsl | MsmRadiation | MsmWind | MsmUpdraft
            | MepsTemperature | MepsHumidity | MepsPrecipitation | MepsAltitude | MepsPressure
            | MepsPressureMsl | MepsRadiation | MepsWind | MepsUpdraft => match self.surface {
                FixedSurface::IsobaricSurface(_, _) => &LngLatGrid {
                    lng_0: 120.,
                    lat_0: 20.,
                    lng_denom: 8.,
                    lat_denom: 10.,
                },
                _ => &LngLatGrid {
                    lng_0: 120.,
                    lat_0: 20.,
                    lng_denom: 16.,
                    lat_denom: 20.,
                },
            },
            CwmWave | CwmWind | CwmWindWave | CwmSwell1 | CwmSwell2 => &LngLatGrid {
                lng_0: 120.,
                lat_0: 20.,
                lng_denom: 20.,
                lat_denom: 20.,
            },
            BunpuTemperature | BunpuWeather | BunpuPrecipitation | BunpuSnowfall
            | BunpuMinTemperature | BunpuMaxTemperature => &LngLatGrid {
                lng_0: 120. + 1. / 16. / 2.,
                lat_0: 20. + 1. / 20. / 2.,
                lng_denom: 16.,
                lat_denom: 20.,
            },
            Snowfall | SnowDepth => &LngLatGrid {
                lng_0: 110. + 1. / 16. / 2.,
                lat_0: 20. + 1. / 20. / 2.,
                lng_denom: 16.,
                lat_denom: 20.,
            },
            OceanNpHeight | OceanNpSalinity | OceanNpTemperature => &LngLatGrid {
                lng_0: 99. - 1. / 11.,
                lat_0: 0.,
                lng_denom: 11.,
                lat_denom: 10.,
            },
            OceanNpCurrent => &LngLatGrid {
                lng_0: 90. - 3. / 11. / 2.,
                lat_0: 0. - 1. / 10. / 2.,
                lng_denom: 11.,
                lat_denom: 10.,
            },
            SstDaily => &LngLatGrid {
                lng_0: 100. + 1. / 10. / 2.,
                lat_0: 0. + 1. / 10. / 2.,
                lng_denom: 10.,
                lat_denom: 10.,
            },
            TornadoNowcast => &LngLatGrid {
                lng_0: 110. + 1. / 8. / 2.,
                lat_0: 20. + 1. / 12. / 2.,
                lng_denom: 8.,
                lat_denom: 12.,
            },
            UvIndex | UvIndexClear => &LngLatGrid {
                lng_0: 120.,
                lat_0: 20.,
                lng_denom: 4.,
                lat_denom: 5.,
            },
            Sst | Current => &LngLatGrid {
                lng_0: 100. + 1. / 4. / 2.,
                lat_0: 0. + 1. / 4. / 2.,
                lng_denom: 4.,
                lat_denom: 4.,
            },
            GwmWave | GwmWind | GwmWindWave | GwmSwell1 | GwmSwell2 => &LngLatGrid {
                lng_0: 0.,
                lat_0: -75.,
                lng_denom: 4.,
                lat_denom: 4.,
            },
            KousaLow | KousaColumn => &LngLatGrid {
                lng_0: 80.,
                lat_0: 20.,
                lng_denom: 2.,
                lat_denom: 2.,
            },
            WemWave => &LngLatGrid {
                lng_0: 0.,
                lat_0: -90.,
                lng_denom: 2.,
                lat_denom: 2.,
            },
            GsmTemperature | GsmHumidity | GsmPrecipitation | GsmAltitude | GsmCloud
            | GsmPressure | GsmPressureMsl | GsmRadiation | GsmWind | GsmUpdraft => {
                match self.surface {
                    FixedSurface::IsobaricSurface(v, _) if v <= 70 => &LngLatGrid {
                        lng_0: 0.,
                        lat_0: -90.,
                        lng_denom: 1.,
                        lat_denom: 1.,
                    },
                    _ => &LngLatGrid {
                        lng_0: 0.,
                        lat_0: -90.,
                        lng_denom: 2.,
                        lat_denom: 2.,
                    },
                }
            }
            TyphoonStorm => &LngLatGrid {
                lng_0: 120.,
                lat_0: 20.,
                lng_denom: 2.,
                lat_denom: 2.5,
            },
            Ozone => &LngLatGrid {
                lng_0: 120.,
                lat_0: 20.,
                lng_denom: 0.8,
                lat_denom: 0.8,
            },
        }
    }

    pub fn translate_values(&self, values: &mut Vec<i32>, _band: u8) {
        use GpvProductElement::*;
        match self.kind {
            KikikuruDosha => {
                for value in values {
                    if *value == 0 || *value <= -2 {
                        *value = i32::MIN
                    }
                }
            }
            CwmWave | GwmWave => {
                for value in values {
                    if *value == 0 || *value == -10 {
                        *value = i32::MIN
                    }
                }
            }
            HiresCloud => {
                for value in values {
                    if *value == 200 {
                        *value = i32::MIN
                    }
                }
            }
            BunpuPrecipitation
            | BunpuSnowfall
            | Dojoshisu
            | HiresCloudIce
            | HiresCloudQc
            | HiresCloudType
            | HiresNowcastIntensity
            | HiresNowcastIntensityError
            | HiresNowcastPrecip
            | HiresNowcastPrecipError
            | Hyomenshisu
            | KikikuruFlood
            | KikikuruInundation
            | KikikuruTougou
            | NowcastEchoTops
            | OceanJpIceCover
            | OceanJpIceThickness
            | Precipitaiton
            | Precipitaiton15h
            | SnowDepth
            | Snowfall
            | TyphoonStorm => {
                for value in values {
                    if *value == 0 {
                        *value = i32::MIN
                    }
                }
            }
            _ => {}
        }
    }

    /// Zoom level at which chucks are generated
    pub fn base_z(&self) -> u8 {
        (self.grid().lat_denom * 360. * 2. / 512.).log2().ceil() as u8
    }
}

#[derive(Default, Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub enum GpvProductElement {
    #[default]
    Unknown,
    Atm30minWind,
    Atm30minTemperature,
    BunpuMaxTemperature,
    BunpuMinTemperature,
    BunpuPrecipitation,
    BunpuSnowfall,
    BunpuTemperature,
    BunpuWeather,
    CwmSwell1,
    CwmSwell2,
    CwmWave,
    CwmWind,
    CwmWindWave,
    Dojoshisu,
    GwmSwell1,
    GwmSwell2,
    GwmWave,
    GwmWind,
    GwmWindWave,
    WemWave,
    GsmAltitude,
    GsmCloud,
    GsmHumidity,
    GsmPrecipitation,
    GsmPressure,
    GsmPressureMsl,
    GsmRadiation,
    GsmTemperature,
    GsmUpdraft,
    GsmWind,
    HiresNowcastIntensity,
    HiresNowcastPrecip,
    HiresNowcastPrecipError,
    HiresNowcastIntensityError,
    HiresCloud,
    HiresCloudIce,
    HrCloudAltitude,
    HiresCloudType,
    HiresCloudQc,
    Hyomenshisu,
    KikikuruDosha,
    KikikuruFlood,
    KikikuruInundation,
    KikikuruTougou,
    KousaColumn,
    KousaLow,
    LfmAltitude,
    LfmCloud,
    LfmHumidity,
    LfmPrecipitation,
    LfmPressure,
    LfmPressureMsl,
    LfmRadiation,
    LfmTemperature,
    LfmUpdraft,
    LfmWind,
    MsmAltitude,
    MsmCloud,
    MsmHumidity,
    MsmPrecipitation,
    MsmPressure,
    MsmPressureMsl,
    MsmRadiation,
    MsmTemperature,
    MsmUpdraft,
    MsmWind,
    MepsAltitude,
    MepsHumidity,
    MepsPrecipitation,
    MepsPressure,
    MepsPressureMsl,
    MepsRadiation,
    MepsTemperature,
    MepsUpdraft,
    MepsWind,
    NowcastEchoTops,
    Ozone,
    Precipitaiton,
    Precipitaiton15h,
    SuikeiSunshine,
    SuikeiTemperature,
    SuikeiWeather,
    ThunderNowcast,
    Tide,
    TideAstronomical,
    TideGuidance,
    TidePressure,
    TideWind,
    TornadoNowcast,
    TyphoonStorm,
    UvIndex,
    UvIndexClear,
    SstDaily,
    Current,
    SstHimawari,
    OceanJpHeight,
    OceanJpTemperature,
    OceanJpSalinity,
    OceanJpCurrent,
    OceanJpIceCover,
    OceanJpIceThickness,
    OceanJpIceDrift,
    OceanNpHeight,
    OceanNpTemperature,
    OceanNpSalinity,
    OceanNpCurrent,
    Sst,
    Snowfall,
    SnowDepth,
}

#[derive(Default, Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub enum FixedSurface {
    #[default]
    None,
    Surface,
    Msl,
    Altitude(u32, i8),
    IsobaricSurface(u32, i8),
    DepthBelowSeaLevel(u32, i8),
    CloudTops,
    TankTotal,
    Tank(u32),
}

impl FixedSurface {
    pub fn suffix(&self) -> Option<String> {
        Some(match self {
            FixedSurface::None => return None,
            FixedSurface::Surface => "surf".to_string(),
            FixedSurface::Msl => "msl".to_string(),
            FixedSurface::Altitude(v, _) => format!("alt{v}"),
            FixedSurface::IsobaricSurface(v, _) => format!("p{v}"),
            FixedSurface::DepthBelowSeaLevel(v, s) => {
                let s = ((*v as f32) / (10f32).powi(*s as i32)).to_string();
                format!("dsl{}", s.replace(".", "p"))
            }
            FixedSurface::CloudTops => "ct".to_string(),
            FixedSurface::TankTotal => "tank-total".to_string(),
            FixedSurface::Tank(n) => format!("tank-{n}"),
        })
    }
}

#[derive(Default, Debug, PartialEq, Eq, Hash, Clone, Copy)]
#[repr(u8)]
pub enum GeneratingProcessType {
    Analysis = 0,
    Initialization = 1,
    Forecast = 2,
    EnsembleForecast = 4,
    Observation = 8,
    Other(u8),
    #[default]
    Missing = 255,
}

impl From<u8> for GeneratingProcessType {
    fn from(value: u8) -> Self {
        match value {
            0 => GeneratingProcessType::Analysis,
            1 => GeneratingProcessType::Initialization,
            2 => GeneratingProcessType::Forecast,
            4 => GeneratingProcessType::EnsembleForecast,
            8 => GeneratingProcessType::Observation,
            255 => GeneratingProcessType::Missing,
            _ => GeneratingProcessType::Other(value),
        }
    }
}

impl GeneratingProcessType {
    pub fn prefix(&self) -> String {
        match self {
            GeneratingProcessType::Analysis => "analysis".to_string(),
            GeneratingProcessType::Initialization => "init".to_string(),
            GeneratingProcessType::Forecast => "forecast".to_string(),
            GeneratingProcessType::EnsembleForecast => "ens-forecast".to_string(),
            GeneratingProcessType::Observation => "observation".to_string(),
            GeneratingProcessType::Other(v) => format!("other-{v}"),
            GeneratingProcessType::Missing => unreachable!(),
        }
    }
}

#[derive(Default, Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub struct Ensemble {
    pub perturbation_number: u8,
    pub ensemble_type: u8,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn test_path_parts_roundtrip() {
        let all_elements = vec![
            GpvProductElement::Atm30minTemperature,
            GpvProductElement::Atm30minWind,
            GpvProductElement::BunpuMaxTemperature,
            GpvProductElement::BunpuMinTemperature,
            GpvProductElement::BunpuPrecipitation,
            GpvProductElement::BunpuSnowfall,
            GpvProductElement::BunpuTemperature,
            GpvProductElement::BunpuWeather,
            GpvProductElement::Current,
            GpvProductElement::CwmSwell1,
            GpvProductElement::CwmSwell2,
            GpvProductElement::CwmWave,
            GpvProductElement::CwmWind,
            GpvProductElement::CwmWindWave,
            GpvProductElement::Dojoshisu,
            GpvProductElement::GsmAltitude,
            GpvProductElement::GsmCloud,
            GpvProductElement::GsmHumidity,
            GpvProductElement::GsmPrecipitation,
            GpvProductElement::GsmPressure,
            GpvProductElement::GsmPressureMsl,
            GpvProductElement::GsmRadiation,
            GpvProductElement::GsmTemperature,
            GpvProductElement::GsmUpdraft,
            GpvProductElement::GsmWind,
            GpvProductElement::GwmSwell1,
            GpvProductElement::GwmSwell2,
            GpvProductElement::GwmWave,
            GpvProductElement::GwmWind,
            GpvProductElement::GwmWindWave,
            GpvProductElement::HiresNowcastIntensity,
            GpvProductElement::HiresNowcastIntensityError,
            GpvProductElement::HiresNowcastPrecip,
            GpvProductElement::HiresNowcastPrecipError,
            GpvProductElement::HiresCloud,
            GpvProductElement::HrCloudAltitude,
            GpvProductElement::HiresCloudType,
            GpvProductElement::HiresCloudIce,
            GpvProductElement::HiresCloudQc,
            GpvProductElement::Hyomenshisu,
            GpvProductElement::KikikuruDosha,
            GpvProductElement::KikikuruFlood,
            GpvProductElement::KikikuruInundation,
            GpvProductElement::KikikuruTougou,
            GpvProductElement::KousaColumn,
            GpvProductElement::KousaLow,
            GpvProductElement::LfmAltitude,
            GpvProductElement::LfmCloud,
            GpvProductElement::LfmHumidity,
            GpvProductElement::LfmPrecipitation,
            GpvProductElement::LfmPressure,
            GpvProductElement::LfmPressureMsl,
            GpvProductElement::LfmRadiation,
            GpvProductElement::LfmTemperature,
            GpvProductElement::LfmUpdraft,
            GpvProductElement::LfmWind,
            GpvProductElement::MepsAltitude,
            GpvProductElement::MepsHumidity,
            GpvProductElement::MepsPrecipitation,
            GpvProductElement::MepsPressure,
            GpvProductElement::MepsPressureMsl,
            GpvProductElement::MepsRadiation,
            GpvProductElement::MepsTemperature,
            GpvProductElement::MepsUpdraft,
            GpvProductElement::MepsWind,
            GpvProductElement::MsmAltitude,
            GpvProductElement::MsmCloud,
            GpvProductElement::MsmHumidity,
            GpvProductElement::MsmPrecipitation,
            GpvProductElement::MsmPressure,
            GpvProductElement::MsmPressureMsl,
            GpvProductElement::MsmRadiation,
            GpvProductElement::MsmTemperature,
            GpvProductElement::MsmUpdraft,
            GpvProductElement::MsmWind,
            GpvProductElement::NowcastEchoTops,
            GpvProductElement::OceanJpIceCover,
            GpvProductElement::OceanJpIceDrift,
            GpvProductElement::OceanJpIceThickness,
            GpvProductElement::OceanJpCurrent,
            GpvProductElement::OceanJpHeight,
            GpvProductElement::OceanJpSalinity,
            GpvProductElement::OceanJpTemperature,
            GpvProductElement::OceanNpCurrent,
            GpvProductElement::OceanNpHeight,
            GpvProductElement::OceanNpSalinity,
            GpvProductElement::OceanNpTemperature,
            GpvProductElement::Precipitaiton,
            GpvProductElement::Precipitaiton15h,
            GpvProductElement::SnowDepth,
            GpvProductElement::Snowfall,
            GpvProductElement::Sst,
            GpvProductElement::SstDaily,
            GpvProductElement::SstHimawari,
            GpvProductElement::SuikeiSunshine,
            GpvProductElement::SuikeiTemperature,
            GpvProductElement::SuikeiWeather,
            GpvProductElement::ThunderNowcast,
            GpvProductElement::Tide,
            GpvProductElement::TideAstronomical,
            GpvProductElement::TideGuidance,
            GpvProductElement::TidePressure,
            GpvProductElement::TideWind,
            GpvProductElement::TornadoNowcast,
            GpvProductElement::TyphoonStorm,
            GpvProductElement::UvIndex,
            GpvProductElement::UvIndexClear,
            GpvProductElement::Ozone,
            GpvProductElement::WemWave,
        ];

        for element in all_elements {
            // Skip Unknown as it's not meant to be roundtripped
            if element == GpvProductElement::Unknown {
                continue;
            }

            // Create a dummy product identifier to get path_parts
            let product_id = GpvProductIdentifier {
                kind: element,
                datetime: Utc::now(),
                reference_datetime: Utc::now(),
                generating_process: GeneratingProcessType::Forecast,
                surface: FixedSurface::Surface,
                ensemble: None,
                variant: None,
            };

            let (data_kind, value_kind) = product_id.path_parts();

            // Convert back using the reverse function
            let converted_element = get_product_by_data_kind_and_value_kind(data_kind, value_kind);

            assert_eq!(
                converted_element, element,
                "Element mapping check failed for element: {element:?}"
            );
        }
    }

    #[test]
    fn test_unknown_mappings() {
        // Test that unknown combinations return Unknown
        let unknown_cases = vec![
            ("nonexistent", "type"),
            ("msm", "nonexistent"),
            ("", ""),
            ("invalid", "invalid"),
        ];

        for (data_kind, value_kind) in unknown_cases {
            let result = get_product_by_data_kind_and_value_kind(data_kind, value_kind);
            assert_eq!(
                result,
                GpvProductElement::Unknown,
                "Unknown mapping: ({data_kind}, {value_kind}) should return Unknown, got {result:?}"
            );
        }
    }

    #[test]
    fn test_surface_suffix_roundtrip() {
        // Test that FixedSurface variants can be converted to suffix and back
        let test_surfaces = vec![
            FixedSurface::None,
            FixedSurface::Surface,
            FixedSurface::Msl,
            FixedSurface::CloudTops,
            FixedSurface::TankTotal,
            FixedSurface::Altitude(1500, 0),
            // Note: Altitude and IsobaricSurface scale factors are not preserved in suffix()
            // so we only test with scale factor 0
            FixedSurface::IsobaricSurface(1000, 0),
            FixedSurface::IsobaricSurface(850, 0),
            FixedSurface::IsobaricSurface(500, 0),
            FixedSurface::DepthBelowSeaLevel(10, 0),
            FixedSurface::DepthBelowSeaLevel(125, 1), // 12.5 - this one preserves scale factor
            FixedSurface::Tank(1),
            FixedSurface::Tank(255),
        ];

        let mut successful_roundtrips = 0;
        let mut failed_roundtrips = Vec::new();

        for surface in test_surfaces {
            let suffix = surface.suffix();

            match suffix {
                Some(suffix_str) => {
                    let converted_surface = get_surface_by_surface_str(Some(&suffix_str));
                    if converted_surface == surface {
                        successful_roundtrips += 1;
                    } else {
                        failed_roundtrips.push((surface, suffix_str, converted_surface));
                    }
                }
                None => {
                    // None suffix should map to empty string
                    let converted_surface = get_surface_by_surface_str(Some(""));
                    if converted_surface == surface {
                        successful_roundtrips += 1;
                    } else {
                        failed_roundtrips.push((surface, "".to_string(), converted_surface));
                    }
                }
            }
        }

        // Report results
        if !failed_roundtrips.is_empty() {
            println!("Failed surface roundtrips:");
            for (original, suffix, converted) in &failed_roundtrips {
                println!("  {original:?} -> {suffix} -> {converted:?}");
            }
        }

        println!("Successful surface roundtrips: {successful_roundtrips}");
        println!("Failed surface roundtrips: {}", failed_roundtrips.len());

        // Assert that all conversions work
        assert_eq!(
            failed_roundtrips.len(),
            0,
            "Some surface types failed roundtrip conversion"
        );
    }

    #[test]
    fn test_specific_surface_mappings() {
        // Test specific known surface mappings
        let test_cases = vec![
            ("", FixedSurface::None),
            ("surf", FixedSurface::Surface),
            ("msl", FixedSurface::Msl),
            ("ct", FixedSurface::CloudTops),
            ("tank-total", FixedSurface::TankTotal),
            ("alt1500", FixedSurface::Altitude(1500, 0)),
            ("p1000", FixedSurface::IsobaricSurface(1000, 0)),
            ("p850", FixedSurface::IsobaricSurface(850, 0)),
            ("dsl10", FixedSurface::DepthBelowSeaLevel(10, 0)),
            ("tank-1", FixedSurface::Tank(1)),
        ];

        for (suffix, expected) in test_cases {
            let result = get_surface_by_surface_str(Some(suffix));
            assert_eq!(
                result, expected,
                "Failed surface mapping: {suffix} should map to {expected:?}, got {result:?}"
            );
        }
    }

    #[test]
    fn test_unknown_surface_suffixes() {
        // Test that unknown surface suffixes return None
        let unknown_cases = vec![
            "unknown", "invalid", "p",      // incomplete pressure
            "alt",    // incomplete altitude
            "dsl",    // incomplete depth
            "tank-",  // incomplete tank
            "xyz123", // invalid pattern
        ];

        for suffix in unknown_cases {
            let result = get_surface_by_surface_str(Some(suffix));
            assert_eq!(
                result,
                FixedSurface::None,
                "Unknown surface suffix {suffix} should return None, got {result:?}"
            );
        }
    }

    #[test]
    fn test_none_surface_suffix() {
        // Test that FixedSurface::None returns None suffix
        let none_surface = FixedSurface::None;
        assert_eq!(
            none_surface.suffix(),
            None,
            "FixedSurface::None should return None suffix"
        );
    }

    #[test]
    fn test_generating_process_roundtrip() {
        // Test that GeneratingProcessType variants can be converted to string and back
        let test_processes = vec![
            GeneratingProcessType::Analysis,
            GeneratingProcessType::Initialization,
            GeneratingProcessType::Forecast,
            GeneratingProcessType::EnsembleForecast,
            GeneratingProcessType::Observation,
            GeneratingProcessType::Other(42),
            GeneratingProcessType::Other(255),
        ];

        let mut successful_roundtrips = 0;
        let mut failed_roundtrips = Vec::new();

        for process in test_processes {
            // Create a dummy product identifier to get the gen_process string
            let product_id = GpvProductIdentifier {
                kind: GpvProductElement::HiresNowcastIntensity,
                datetime: Utc::now(),
                reference_datetime: Utc::now(),
                generating_process: process,
                surface: FixedSurface::Surface,
                ensemble: None,
                variant: None,
            };

            let gen_process_str = product_id.generating_process.prefix();

            // Convert back using the reverse function
            let converted_process =
                get_generating_process_by_generating_process_str(&gen_process_str);

            if converted_process == process {
                successful_roundtrips += 1;
            } else {
                failed_roundtrips.push((process, gen_process_str.to_string(), converted_process));
            }
        }

        // Report results
        if !failed_roundtrips.is_empty() {
            println!("Failed generating process roundtrips:");
            for (original, gen_process_str, converted) in &failed_roundtrips {
                println!("  {original:?} -> {gen_process_str} -> {converted:?}");
            }
        }

        println!("Successful generating process roundtrips: {successful_roundtrips}");
        println!(
            "Failed generating process roundtrips: {}",
            failed_roundtrips.len()
        );

        // Assert that all conversions work
        assert_eq!(
            failed_roundtrips.len(),
            0,
            "Some generating process types failed roundtrip conversion"
        );
    }

    #[test]
    fn test_specific_generating_process_mappings() {
        // Test specific known generating process mappings
        let test_cases = vec![
            ("analysis", GeneratingProcessType::Analysis),
            ("init", GeneratingProcessType::Initialization),
            ("forecast", GeneratingProcessType::Forecast),
            ("ens-forecast", GeneratingProcessType::EnsembleForecast),
            ("observation", GeneratingProcessType::Observation),
            ("other-42", GeneratingProcessType::Other(42)),
            ("other-255", GeneratingProcessType::Other(255)),
        ];

        for (process_str, expected) in test_cases {
            let result = get_generating_process_by_generating_process_str(process_str);
            assert_eq!(
                result, expected,
                "Failed generating process mapping: {process_str} should map to {expected:?}, got {result:?}"
            );
        }
    }

    #[test]
    fn test_unknown_generating_process_strings() {
        // Test that unknown generating process strings return Missing
        let unknown_cases = vec!["unknown", "invalid", "", "xyz", "test"];

        for process_str in unknown_cases {
            let result = get_generating_process_by_generating_process_str(process_str);
            assert_eq!(
                result,
                GeneratingProcessType::Missing,
                "Unknown generating process string {process_str} should return Missing, got {result:?}"
            );
        }
    }
}
