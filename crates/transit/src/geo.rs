//! Great-circle distance and the bounding-box prefilter for
//! `/transit/stops/nearby`. Port of `parsers/geo.ts`.

/// Mean Earth radius in metres, as used by the haversine formula.
pub const EARTH_RADIUS_METRES: f64 = 6_371_008.8;

fn to_radians(degrees: f64) -> f64 {
    degrees * std::f64::consts::PI / 180.0
}

/// Great-circle distance between two WGS84 points, in metres.
///
/// Haversine is used rather than the cheaper equirectangular approximation
/// because the numbers it returns are directly comparable to the `radius`
/// parameter a caller passes, and because it stays accurate at the latitudes
/// this service is asked about, where the flat approximation overstates
/// east–west distance by up to a third.
pub fn haversine_metres(latitude_a: f64, longitude_a: f64, latitude_b: f64, longitude_b: f64) -> f64 {
    let d_lat = to_radians(latitude_b - latitude_a);
    let d_lon = to_radians(longitude_b - longitude_a);
    let lat_a = to_radians(latitude_a);
    let lat_b = to_radians(latitude_b);

    let sin_lat = (d_lat / 2.0).sin();
    let sin_lon = (d_lon / 2.0).sin();
    let h = sin_lat * sin_lat + lat_a.cos() * lat_b.cos() * sin_lon * sin_lon;
    // `min(1.0, …)` guards against a rounding overshoot that would make
    // `asin(sqrt(h))` return NaN for antipodal points.
    2.0 * EARTH_RADIUS_METRES * h.min(1.0).sqrt().asin()
}

/// A latitude/longitude window that fully contains a radius around a point.
#[derive(Debug, Clone, Copy)]
pub struct BoundingBox {
    pub min_latitude: f64,
    pub max_latitude: f64,
    pub min_longitude: f64,
    pub max_longitude: f64,
}

/// Bounding box that is guaranteed to contain every point within `radius`.
///
/// Scanning a few thousand stops is already cheap, but for the larger feeds a
/// flat haversine over every stop dominates the request, and the box lets the
/// index reject almost all of them with four comparisons. The box is
/// deliberately loose: a circle inscribed in it is smaller than the requested
/// radius near the corners, and the exact haversine filter runs afterwards.
pub fn bounding_box(latitude: f64, longitude: f64, radius_metres: f64) -> BoundingBox {
    let latitude_delta = radius_metres / EARTH_RADIUS_METRES / std::f64::consts::PI * 180.0;
    let cos_latitude = to_radians(latitude).cos();
    // Near the poles a degree of longitude collapses to nothing. Clamp the
    // divisor so the box degrades to "the whole globe" instead of dividing by
    // zero; the exact filter still narrows the result.
    let longitude_delta = if cos_latitude.abs() < 1e-9 {
        180.0
    } else {
        latitude_delta / cos_latitude.abs()
    };

    BoundingBox {
        min_latitude: (latitude - latitude_delta).max(-90.0),
        max_latitude: (latitude + latitude_delta).min(90.0),
        min_longitude: longitude - longitude_delta,
        max_longitude: longitude + longitude_delta,
    }
}

/// True when a point falls inside the box (no wrap-around).
pub fn in_box(bounds: &BoundingBox, latitude: f64, longitude: f64) -> bool {
    latitude >= bounds.min_latitude
        && latitude <= bounds.max_latitude
        && longitude >= bounds.min_longitude
        && longitude <= bounds.max_longitude
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn haversine_matches_known_distances() {
        // One degree of latitude on the same meridian is one full circumference
        // fraction: 2πR/360 ≈ 111 195 m.
        let one_degree = haversine_metres(40.0, -73.99, 41.0, -73.99);
        assert!((one_degree - 111_194.9).abs() < 1.0, "{one_degree}");
        assert!(haversine_metres(12.3, 45.6, 12.3, 45.6) < 1e-6, "identical points");
        // A quarter of the globe along the equator: no NaN for extreme inputs.
        let quarter = haversine_metres(0.0, 0.0, 0.0, 90.0);
        let expected = std::f64::consts::PI * EARTH_RADIUS_METRES / 2.0;
        assert!((quarter - expected).abs() < 1.0, "{quarter}");
        let antipodal = haversine_metres(0.0, 0.0, 0.0, 180.0);
        assert!(!antipodal.is_nan(), "antipodal points do not produce NaN");
    }

    #[test]
    fn bounding_box_rejects_far_candidates_and_clamps_at_the_poles() {
        let bounds = bounding_box(40.75, -73.99, 1000.0);
        // 1000 m is ~0.0090° of latitude, so the box is ~0.0180° tall.
        let height = bounds.max_latitude - bounds.min_latitude;
        assert!(height > 0.017 && height < 0.019, "height = {height}");
        assert!(in_box(&bounds, 40.755, -73.99), "~556 m north is inside");
        assert!(!in_box(&bounds, 40.77, -73.99), "~2.2 km north is outside");
        assert!(!in_box(&bounds, 41.5, -73.99), "a far-away stop is outside");
        assert!(in_box(&bounds, 40.75, -73.99), "the centre is inside");

        // Near the poles the longitude divisor clamps and the box degrades to
        // the whole globe instead of producing NaN.
        let polar = bounding_box(89.999, 10.0, 50_000.0);
        assert!(polar.min_longitude.is_finite() && polar.max_longitude.is_finite());
        assert!(polar.min_longitude <= -170.0 && polar.max_longitude >= 170.0);
        assert!(in_box(&polar, 89.9995, -170.0));

        // Latitude is clamped to ±90.
        let over = bounding_box(89.9999, 0.0, 100_000.0);
        assert_eq!(over.max_latitude, 90.0);
    }
}
