use std::fmt;

use thiserror::Error;

use crate::model::Delta;

/// The identity of a format package: its name plus semver version.
///
/// Determinism is contract (ADR-0003): outputs carry this id and caches key
/// on it, so a version bump is a new projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PackageId {
    pub name: &'static str,
    pub version: &'static str,
}

impl fmt::Display for PackageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.name, self.version)
    }
}

/// A deterministic text rendering of a document, stamped with the package
/// that produced it. The original document is always kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Projection {
    pub package: PackageId,
    pub text: String,
}

/// Why a package could not handle a document it detected.
#[derive(Debug, Error)]
#[error("{package}: {reason}")]
pub struct PackageError {
    pub package: PackageId,
    pub reason: String,
}

/// How strongly a package claims a document.
///
/// When several packages claim the same document, the highest confidence
/// wins and ties break by package id — selection never depends on registry
/// construction order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Confidence {
    /// The path matches the format's naming convention; the content was
    /// not examined.
    Extension,
    /// The content itself was verified: magic bytes or structure.
    Content,
}

/// One format's support, shipped as its own versioned unit: the ecosystem's
/// public ABI (ADR-0003).
///
/// A package never gates a diff — when it is absent or fails, fidelity drops
/// to the text or binary rung instead. Determinism is contract: the same
/// bytes under the same package version must produce the same projection
/// and the same diff.
pub trait FormatPackage {
    /// The package's stable identity.
    fn id(&self) -> PackageId;

    /// How confidently this package claims the document at `path` with
    /// these bytes; `None` when it does not handle the document.
    fn detect(&self, path: &str, bytes: &[u8]) -> Option<Confidence>;

    /// Render the document to its text projection.
    fn project(&self, bytes: &[u8]) -> Result<Projection, PackageError>;

    /// The rich diff in the format's own terms, or `None` while the package
    /// ships no differ — the ladder then falls back to projected text.
    fn diff(&self, _before: &[u8], _after: &[u8]) -> Option<Result<Vec<Delta>, PackageError>> {
        None
    }
}

/// The package that claims the document most confidently; equal claims
/// break by package id, so selection is deterministic whatever order the
/// registry was built in.
pub fn detect_package<'a>(
    packages: &'a [Box<dyn FormatPackage>],
    path: &str,
    bytes: &[u8],
) -> Option<&'a dyn FormatPackage> {
    let mut best: Option<(Confidence, &'a dyn FormatPackage)> = None;
    for package in packages {
        let Some(confidence) = package.detect(path, bytes) else {
            continue;
        };
        let replaces = match best {
            None => true,
            Some((held, incumbent)) => {
                confidence > held || (confidence == held && precedes(package.id(), incumbent.id()))
            }
        };
        if replaces {
            best = Some((confidence, package.as_ref()));
        }
    }
    best.map(|(_, package)| package)
}

/// The tie-break order between equally confident packages: by name, then
/// by version.
fn precedes(candidate: PackageId, incumbent: PackageId) -> bool {
    (candidate.name, candidate.version) < (incumbent.name, incumbent.version)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        name: &'static str,
        extension: &'static str,
        confidence: Confidence,
    }

    impl FormatPackage for Fake {
        fn id(&self) -> PackageId {
            PackageId {
                name: self.name,
                version: "1.2.3",
            }
        }

        fn detect(&self, path: &str, _bytes: &[u8]) -> Option<Confidence> {
            path.ends_with(self.extension).then_some(self.confidence)
        }

        fn project(&self, _bytes: &[u8]) -> Result<Projection, PackageError> {
            Ok(Projection {
                package: self.id(),
                text: String::new(),
            })
        }
    }

    fn fake(name: &'static str, extension: &'static str, confidence: Confidence) -> Box<Fake> {
        Box::new(Fake {
            name,
            extension,
            confidence,
        })
    }

    #[test]
    fn package_id_displays_name_at_version() {
        let id = PackageId {
            name: "format-docx",
            version: "0.1.0",
        };
        assert_eq!(id.to_string(), "format-docx@0.1.0");
    }

    #[test]
    fn detection_picks_the_highest_confidence_whatever_the_registry_order() {
        let forward: Vec<Box<dyn FormatPackage>> = vec![
            fake("by-name", ".a", Confidence::Extension),
            fake("by-content", ".a", Confidence::Content),
        ];
        let backward: Vec<Box<dyn FormatPackage>> = vec![
            fake("by-content", ".a", Confidence::Content),
            fake("by-name", ".a", Confidence::Extension),
        ];

        for packages in [&forward, &backward] {
            let found = detect_package(packages, "doc.a", b"").unwrap();
            assert_eq!(found.id().name, "by-content");
        }
    }

    #[test]
    fn detection_ties_break_by_package_id_whatever_the_registry_order() {
        let forward: Vec<Box<dyn FormatPackage>> = vec![
            fake("zebra", ".a", Confidence::Content),
            fake("aardvark", ".a", Confidence::Content),
        ];
        let backward: Vec<Box<dyn FormatPackage>> = vec![
            fake("aardvark", ".a", Confidence::Content),
            fake("zebra", ".a", Confidence::Content),
        ];

        for packages in [&forward, &backward] {
            let found = detect_package(packages, "doc.a", b"").unwrap();
            assert_eq!(found.id().name, "aardvark");
        }
    }

    #[test]
    fn detection_yields_none_when_no_package_matches() {
        let packages: Vec<Box<dyn FormatPackage>> = vec![fake("first", ".a", Confidence::Content)];
        assert!(detect_package(&packages, "doc.z", b"").is_none());
    }

    #[test]
    fn diff_defaults_to_none_until_a_package_ships_a_differ() {
        let package = fake("first", ".a", Confidence::Content);
        assert!(package.diff(b"before", b"after").is_none());
    }
}
