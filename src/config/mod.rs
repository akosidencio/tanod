//! Configuration loading: parse, then refuse anything unsafe.

pub mod schema;
pub mod units;
pub mod validation;

pub use schema::*;
pub use units::{Bytes, Dur};
pub use validation::ValidationError;

/// The configuration schema version this binary understands.
///
/// `version:` is required in every config file and checked against this. It is
/// not decoration: the format is pre-1.0, and the alternative to a version
/// field is a future release silently reinterpreting a key that used to mean
/// something else. A file naming a version this build does not know is
/// **refused**, with both numbers in the message — the same rule as every
/// other unimplemented key here.
///
/// Compatibility rules and the migration notes live in `docs/CONFIG-SCHEMA.md`.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("reading {path}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing {path}")]
    Parse {
        path: String,
        // Boxed: serde_saphyr's error carries position and context and is
        // large enough to bloat every Ok value on this Result.
        #[source]
        source: Box<serde_saphyr::Error>,
    },
    #[error("invalid configuration in {path}")]
    Invalid {
        path: String,
        #[source]
        source: ValidationError,
    },
}

/// Read, parse and validate a config file. All three failures name the file.
pub fn load(path: &str) -> Result<Config, LoadError> {
    let text = std::fs::read_to_string(path).map_err(|source| LoadError::Io {
        path: path.to_string(),
        source,
    })?;
    let mut cfg: Config = serde_saphyr::from_str(&text).map_err(|source| LoadError::Parse {
        path: path.to_string(),
        source: Box::new(source),
    })?;
    if let Some(file) = cfg.overload.page.file.clone() {
        cfg.overload.page.template =
            Some(read_overload_page(&file).map_err(|source| LoadError::Io { path: file, source })?);
    }
    validation::validate(&cfg).map_err(|source| LoadError::Invalid {
        path: path.to_string(),
        source,
    })?;
    Ok(cfg)
}

/// Read at most one byte past the limit, so validation can refuse an
/// oversized file without this having read all of it.
fn read_overload_page(path: &str) -> std::io::Result<String> {
    use std::io::Read;
    let mut text = String::new();
    std::fs::File::open(path)?
        .take(validation::MAX_OVERLOAD_PAGE_BYTES as u64 + 1)
        .read_to_string(&mut text)?;
    Ok(text)
}
