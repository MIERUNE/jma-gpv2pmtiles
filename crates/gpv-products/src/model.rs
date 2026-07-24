#[derive(Debug, Clone, Default)]
pub struct BandSpec {
    pub name: String,
    pub reference_value: f32,
    pub binary_scale: i8,
    pub decimal_scale: i8,
    pub max: Option<i32>,
    pub min: Option<i32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LngLatGrid {
    pub lng_0: f64,
    pub lat_0: f64,
    pub lng_denom: f32,
    pub lat_denom: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aggregation {
    Max,
    Min,
    RoughAvg,
    BitOr,
}
