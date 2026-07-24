#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LngLat {
    pub lng: f64,
    pub lat: f64,
}

impl LngLat {
    pub fn new(lng: f64, lat: f64) -> Self {
        Self { lng, lat }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LngLatBox {
    min: LngLat,
    max: LngLat,
}

impl LngLatBox {
    pub fn new(mut min: LngLat, mut max: LngLat) -> Self {
        if min.lng > max.lng {
            core::mem::swap(&mut min.lng, &mut max.lng);
        }
        if min.lat > max.lat {
            core::mem::swap(&mut min.lat, &mut max.lat);
        }
        Self { min, max }
    }

    pub fn intersects_box(&self, target: &Self) -> bool {
        self.min.lng <= target.max.lng
            && self.max.lng >= target.min.lng
            && self.min.lat <= target.max.lat
            && self.max.lat >= target.min.lat
    }
}
