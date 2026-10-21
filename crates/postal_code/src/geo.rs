//! Great-circle distance and centroid helpers.
//!
//! Ported 1:1 from `postal-code-api/src/postal/geo.ts`.

/// Mean Earth radius in kilometres, per the IUGG mean radius.
const EARTH_RADIUS_KM: f64 = 6371.0088;

#[derive(Debug, Clone, Copy)]
pub struct Coordinate {
    pub latitude: f64,
    pub longitude: f64,
}

fn to_radians(degrees: f64) -> f64 {
    degrees * std::f64::consts::PI / 180.0
}

/// Great-circle distance between two WGS84 points, in kilometres.
///
/// Uses the haversine formula, which is numerically stable for the small
/// distances this service reports (two postal-code centroids are rarely more
/// than a few hundred kilometres apart). A plain spherical law-of-cosines
/// implementation loses precision below ~1 km, which is exactly the range that
/// matters here.
pub fn haversine_km(from: Coordinate, to: Coordinate) -> f64 {
    let d_lat = to_radians(to.latitude - from.latitude);
    let d_lon = to_radians(to.longitude - from.longitude);
    let lat1 = to_radians(from.latitude);
    let lat2 = to_radians(to.latitude);

    let a = (d_lat / 2.0).sin().powi(2)
        + (d_lon / 2.0).sin().powi(2) * lat1.cos() * lat2.cos();
    let c = 2.0 * a.sqrt().min(1.0).asin();
    EARTH_RADIUS_KM * c
}

/// Mean of a set of points; `None` when no point carries finite coordinates.
pub fn centroid(points: &[Coordinate]) -> Option<Coordinate> {
    let usable: Vec<&Coordinate> = points
        .iter()
        .filter(|p| p.latitude.is_finite() && p.longitude.is_finite())
        .collect();
    if usable.is_empty() {
        return None;
    }
    let mut lat = 0.0;
    let mut lon = 0.0;
    for p in &usable {
        lat += p.latitude;
        lon += p.longitude;
    }
    let n = usable.len() as f64;
    Some(Coordinate { latitude: lat / n, longitude: lon / n })
}

/// Round to `decimals` decimal places, the way `Math.round(v * 10^d) / 10^d`
/// does in the original service.
pub fn round(value: f64, decimals: u32) -> f64 {
    let factor = 10f64.powi(decimals as i32);
    (value * factor).round() / factor
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn haversine_matches_known_distances() {
        // One degree of latitude is ~111.19 km on the IUGG mean sphere.
        let d = haversine_km(
            Coordinate { latitude: 0.0, longitude: 0.0 },
            Coordinate { latitude: 1.0, longitude: 0.0 },
        );
        assert!((d - 111.195).abs() < 0.01, "got {d}");

        // Identical points are zero apart.
        let same = Coordinate { latitude: 35.6895, longitude: 139.6917 };
        assert!(haversine_km(same, same).abs() < 1e-9);

        // Tokyo -> Osaka is roughly 392 km great-circle.
        let tokyo = Coordinate { latitude: 35.6895, longitude: 139.6917 };
        let osaka = Coordinate { latitude: 34.6937, longitude: 135.5023 };
        let d = haversine_km(tokyo, osaka);
        assert!((d - 392.0).abs() < 5.0, "got {d}");
    }

    #[test]
    fn centroid_averages_finite_points_only() {
        let mean = centroid(&[
            Coordinate { latitude: 10.0, longitude: 20.0 },
            Coordinate { latitude: 30.0, longitude: 40.0 },
        ])
        .unwrap();
        assert_eq!(mean.latitude, 20.0);
        assert_eq!(mean.longitude, 30.0);
        assert!(centroid(&[]).is_none());
    }

    #[test]
    fn round_matches_the_original_semantics() {
        assert_eq!(round(1.23456, 3), 1.235);
        assert_eq!(round(1.23449, 3), 1.234);
        assert_eq!(round(-0.0004, 3), 0.0);
    }
}
