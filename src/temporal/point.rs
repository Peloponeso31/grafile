//! Spatial points: 2D and 3D cartesian (`srid` 7203/9157) and WGS-84
//! geodetic (`srid` 4326/4979) coordinates.
//!
//! Only the shape the road map promises is implemented: construction,
//! component accessors and `distance()` (Euclidean for cartesian points,
//! haversine on the mean Earth radius for geodetic ones).

use std::fmt;

/// Cartesian 2D SRID.
pub const CARTESIAN_2D: i32 = 7_203;
/// Cartesian 3D SRID.
pub const CARTESIAN_3D: i32 = 9_157;
/// WGS-84 geodetic 2D SRID.
pub const WGS84_2D: i32 = 4_326;
/// WGS-84 geodetic 3D SRID.
pub const WGS84_3D: i32 = 4_979;

/// Mean Earth radius in meters, used by the haversine distance.
const EARTH_RADIUS_METERS: f64 = 6_371_008.8;

/// A point value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    srid: i32,
    x: f64,
    y: f64,
    z: Option<f64>,
}

impl Point {
    /// Build a point; `z` is only accepted for the 3D SRIDs.
    pub fn new(srid: i32, x: f64, y: f64, z: Option<f64>) -> Option<Self> {
        let is_3d = z.is_some();
        let expected = match srid {
            CARTESIAN_2D | CARTESIAN_3D | WGS84_2D | WGS84_3D => srid,
            _ => return None,
        };
        if is_3d != (expected == CARTESIAN_3D || expected == WGS84_3D) {
            return None;
        }
        Some(Self { srid, x, y, z })
    }

    /// SRID (spatial reference identifier).
    pub const fn srid(self) -> i32 {
        self.srid
    }

    /// First coordinate (cartesian `x`, or longitude for WGS-84).
    pub const fn x(self) -> f64 {
        self.x
    }

    /// Second coordinate (cartesian `y`, or latitude for WGS-84).
    pub const fn y(self) -> f64 {
        self.y
    }

    /// Third coordinate, when the point is 3D.
    pub const fn z(self) -> Option<f64> {
        self.z
    }

    /// Longitude for geodetic points.
    pub const fn longitude(self) -> f64 {
        self.x
    }

    /// Latitude for geodetic points.
    pub const fn latitude(self) -> f64 {
        self.y
    }

    /// Height for geodetic 3D points.
    pub const fn height(self) -> Option<f64> {
        self.z
    }

    /// Whether the SRID uses geodetic (WGS-84) coordinates.
    pub const fn is_geographic(self) -> bool {
        matches!(self.srid, WGS84_2D | WGS84_3D)
    }

    /// The CRS name.
    pub const fn crs(self) -> &'static str {
        match self.srid {
            CARTESIAN_2D => "cartesian",
            CARTESIAN_3D => "cartesian-3d",
            WGS84_2D => "wgs-84",
            _ => "wgs-84-3d",
        }
    }

    /// Distance to another point in meters (cartesian units for cartesian
    /// SRIDs). Mixing cartesian and geodetic points is an error.
    pub fn distance(self, other: Self) -> Result<f64, String> {
        if self.is_geographic() != other.is_geographic() {
            return Err(
                "cannot compute the distance between a cartesian and a geodetic point".into(),
            );
        }
        if self.is_geographic() {
            Ok(haversine(self, other))
        } else {
            // 3D cartesian points use all three coordinates; a 2D and a 3D
            // point differ only by the missing height.
            let dx = self.x - other.x;
            let dy = self.y - other.y;
            let dz = self.z.unwrap_or(0.0) - other.z.unwrap_or(0.0);
            Ok((dx * dx + dy * dy + dz * dz).sqrt())
        }
    }
}

/// Haversine great-circle distance in meters.
fn haversine(a: Point, b: Point) -> f64 {
    let latitude_a = a.latitude().to_radians();
    let latitude_b = b.latitude().to_radians();
    let delta_latitude = latitude_b - latitude_a;
    let delta_longitude = (b.longitude() - a.longitude()).to_radians();
    let haversine = (delta_latitude / 2.0).sin().powi(2)
        + latitude_a.cos() * latitude_b.cos() * (delta_longitude / 2.0).sin().powi(2);
    2.0 * haversine.sqrt().asin() * EARTH_RADIUS_METERS
}

impl PartialOrd for Point {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Eq for Point {}

impl Ord for Point {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.srid
            .cmp(&other.srid)
            .then_with(|| self.x.total_cmp(&other.x))
            .then_with(|| self.y.total_cmp(&other.y))
            .then_with(|| match (self.z, other.z) {
                (Some(a), Some(b)) => a.total_cmp(&b),
                (Some(_), None) => std::cmp::Ordering::Greater,
                (None, Some(_)) => std::cmp::Ordering::Less,
                (None, None) => std::cmp::Ordering::Equal,
            })
    }
}

impl std::hash::Hash for Point {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.srid.hash(state);
        self.x.to_bits().hash(state);
        self.y.to_bits().hash(state);
        self.z.map(f64::to_bits).hash(state);
    }
}

impl fmt::Display for Point {
    /// Neo4j prints the implicit SRIDs without the key: `point({x: 1.0,
    /// y: 2.0})`; geodetic and 3D variants name their extra fields.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = String::from("point({");
        match self.srid {
            CARTESIAN_2D => {}
            CARTESIAN_3D => out.push_str("srid: 9157, "),
            WGS84_2D => out.push_str("srid: 4326, "),
            _ => out.push_str("srid: 4979, "),
        }
        out.push_str(&format!("x: {:.1}, y: {:.1}", self.x, self.y));
        if let Some(z) = self.z {
            out.push_str(&format!(", z: {z:.1}"));
        }
        out.push_str("})");
        f.write_str(&out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cartesian(x: f64, y: f64) -> Point {
        Point::new(CARTESIAN_2D, x, y, None).expect("point")
    }

    fn geodetic(longitude: f64, latitude: f64) -> Point {
        Point::new(WGS84_2D, longitude, latitude, None).expect("point")
    }

    #[test]
    fn euclidean_distance() {
        let a = cartesian(0.0, 0.0);
        let b = cartesian(3.0, 4.0);
        assert_eq!(a.distance(b), Ok(5.0));
    }

    #[test]
    fn haversine_matches_known_lengths() {
        // One degree of latitude is about 111.2 km on the mean sphere.
        let a = geodetic(12.0, 12.0);
        let b = geodetic(12.0, 13.0);
        let distance = a.distance(b).expect("distance");
        assert!((distance - 111_195.0).abs() < 100.0, "{distance}");
        // Identical points are zero meters apart.
        assert_eq!(a.distance(a), Ok(0.0));
    }

    #[test]
    fn rejects_mixed_frames_and_bad_srids() {
        assert!(cartesian(0.0, 0.0).distance(geodetic(0.0, 0.0)).is_err());
        assert!(Point::new(1234, 0.0, 0.0, None).is_none());
        assert!(Point::new(CARTESIAN_2D, 0.0, 0.0, Some(1.0)).is_none());
        assert!(Point::new(CARTESIAN_3D, 0.0, 0.0, Some(1.0)).is_some());
    }

    #[test]
    fn displays_like_neo4j() {
        assert_eq!(cartesian(1.0, 2.0).to_string(), "point({x: 1.0, y: 2.0})");
        assert_eq!(
            geodetic(12.0, 14.0).to_string(),
            "point({srid: 4326, x: 12.0, y: 14.0})"
        );
    }
}
