use std::f64::consts::FRAC_PI_2;

/// Converts a geographic coordinate to normalized Web Mercator coordinates.
#[inline]
pub(crate) fn lnglat_to_web_mercator(lng: f64, lat: f64) -> (f64, f64) {
    let mx = (lng + 180.0) / 360.0;
    (mx, lat_to_web_mercator_y(lat))
}

#[inline]
pub(crate) fn lat_to_web_mercator_y(lat: f64) -> f64 {
    let my = ((90.0 + lat).to_radians() / 2.0).tan().ln().to_degrees();
    (-my + 180.0) / 360.0
}

/// Converts normalized Web Mercator coordinates to a geographic coordinate.
#[inline]
pub(crate) fn web_mercator_to_lnglat(mx: f64, my: f64) -> (f64, f64) {
    let lng = mx * 360.0 - 180.0;
    let lat = my * 360.0 - 180.0;
    let lat = -(2.0 * lat.to_radians().exp().atan() - FRAC_PI_2).to_degrees();
    (lng, lat)
}

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

    /// Grows the box by `longitude` and `latitude` degrees on every side.
    pub fn expanded(&self, longitude: f64, latitude: f64) -> Self {
        Self {
            min: LngLat::new(self.min.lng - longitude, self.min.lat - latitude),
            max: LngLat::new(self.max.lng + longitude, self.max.lat + latitude),
        }
    }

    pub fn intersects_box(&self, target: &Self) -> bool {
        self.min.lng <= target.max.lng
            && self.max.lng >= target.min.lng
            && self.min.lat <= target.max.lat
            && self.max.lat >= target.min.lat
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_mercator_roundtrip() {
        for (lng, lat) in [(136.08, 37.39), (0.3, 0.2), (0.0, 0.0)] {
            let (mx, my) = lnglat_to_web_mercator(lng, lat);
            let (actual_lng, actual_lat) = web_mercator_to_lnglat(mx, my);
            assert!((lng - actual_lng).abs() < 1e-13);
            assert!((lat - actual_lat).abs() < 1e-13);
        }
    }
}
