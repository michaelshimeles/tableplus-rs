//! Which server a connection talks to: the engine and its version, for behavior that differs
//! between them (such as which EXPLAIN statements a server understands).

use std::fmt;

use crate::dialect::Dialect;

/// The database engine. MariaDB is separate from MySQL here because their EXPLAIN support
/// differs, even though they share a SQL dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Flavor {
    Postgres,
    MySql,
    MariaDb,
    Sqlite,
}

impl Flavor {
    pub fn name(self) -> &'static str {
        match self {
            Flavor::Postgres => "PostgreSQL",
            Flavor::MySql => "MySQL",
            Flavor::MariaDb => "MariaDB",
            Flavor::Sqlite => "SQLite",
        }
    }

    pub fn dialect(self) -> Dialect {
        match self {
            Flavor::Postgres => Dialect::Postgres,
            Flavor::MySql | Flavor::MariaDb => Dialect::MySql,
            Flavor::Sqlite => Dialect::Sqlite,
        }
    }
}

/// A server version, ordered numerically: `8.0.18 < 8.0.46 < 8.4.0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self { major, minor, patch }
    }

    /// Reads the leading numbers of a version string such as `8.0.18-log`,
    /// `10.6.28-MariaDB-ubu2004`, `17.6 (Debian 17.6-1)` or `18beta1`. Missing parts are zero.
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = [0u32; 3];
        let mut rest = text.trim_start();
        for (i, part) in parts.iter_mut().enumerate() {
            let digits = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
            if digits == 0 {
                return (i > 0).then(|| Self::new(parts[0], parts[1], parts[2]));
            }
            *part = rest[..digits].parse().ok()?;
            rest = &rest[digits..];
            match rest.strip_prefix('.') {
                Some(r) => rest = r,
                None => break,
            }
        }
        Some(Self::new(parts[0], parts[1], parts[2]))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// The engine and version behind a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ServerInfo {
    pub flavor: Flavor,
    pub version: Version,
    /// SQLite only: the library measures each plan step while a statement runs
    /// (`SQLITE_ENABLE_STMT_SCANSTATUS`), which Explain Analyze needs.
    pub scan_status: bool,
}

impl ServerInfo {
    pub const fn new(flavor: Flavor, version: Version) -> Self {
        Self { flavor, version, scan_status: false }
    }

    pub const fn with_scan_status(self, scan_status: bool) -> Self {
        Self { scan_status, ..self }
    }

    /// Builds the identity from the version text a server reports; an unreadable version
    /// counts as `0.0.0`, the oldest.
    pub fn from_version_text(flavor: Flavor, text: &str) -> Self {
        Self::new(flavor, Version::parse(text).unwrap_or_default())
    }
}

impl fmt::Display for ServerInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.flavor.name(), self.version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_reported_versions() {
        for (text, expected) in [
            ("8.0.18", Version::new(8, 0, 18)),
            ("5.7.44-log", Version::new(5, 7, 44)),
            ("10.6.28-MariaDB-ubu2004", Version::new(10, 6, 28)),
            ("11.4.13-MariaDB-ubu2404-log", Version::new(11, 4, 13)),
            ("17.6", Version::new(17, 6, 0)),
            ("16.15 (Debian 16.15-1.pgdg120+1)", Version::new(16, 15, 0)),
            ("18beta1", Version::new(18, 0, 0)),
            ("3.53.2", Version::new(3, 53, 2)),
        ] {
            assert_eq!(Version::parse(text), Some(expected), "{text}");
        }
        assert_eq!(Version::parse(""), None);
        assert_eq!(Version::parse("MariaDB"), None);
        assert_eq!(ServerInfo::from_version_text(Flavor::MySql, "garbage").version, Version::default());
    }
}
