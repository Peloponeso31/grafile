//! Temporal and spatial value types.
//!
//! The representations follow Neo4j's model: a proleptic Gregorian civil
//! date, nanosecond-precision times with an optional fixed offset, instants
//! with an optional IANA time zone name, and durations made of months, days
//! and seconds (which never fold into each other). Named time zones are
//! resolved through the bundled IANA database exposed by [`zone`].

pub mod civil;
pub mod duration;
pub mod zone;
