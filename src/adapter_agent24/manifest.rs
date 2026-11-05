//! Validate published manifest semantics while preserving its exact handshake bytes.

use std::collections::BTreeSet;

use serde_yaml::{Mapping, Value};

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    // Do not include parser diagnostics: they may contain manifest values.
    #[error("{origin} manifest is invalid YAML")]
    InvalidYaml { origin: &'static str },
    #[error("{origin} manifest must be a mapping with string top-level keys")]
    InvalidMapping { origin: &'static str },
    #[error("manifest differs from compiled baseline at top-level keys: {keys}")]
    Mismatch { keys: String },
}

fn parse(text: &str, origin: &'static str) -> Result<Mapping, ManifestError> {
    let value: Value =
        serde_yaml::from_str(text).map_err(|_| ManifestError::InvalidYaml { origin })?;
    match value {
        Value::Mapping(mapping) if mapping.keys().all(|key| key.as_str().is_some()) => Ok(mapping),
        _ => Err(ManifestError::InvalidMapping { origin }),
    }
}

/// Only a missing file permits fallback. The embedded manifest is always the baseline;
/// only the published manifest's top-level integrity metadata is excluded from comparison.
pub fn manifest_to_send(embedded: &str, on_disk: Option<&str>) -> Result<String, ManifestError> {
    let Some(on_disk) = on_disk else {
        return Ok(embedded.to_owned());
    };
    let baseline = parse(embedded, "embedded")?;
    let mut published = parse(on_disk, "on-disk")?;
    published.remove(Value::String("integrity".into()));
    if baseline != published {
        let keys: BTreeSet<_> = baseline
            .keys()
            .chain(published.keys())
            .filter(|key| baseline.get(*key) != published.get(*key))
            .filter_map(Value::as_str)
            .collect();
        return Err(ManifestError::Mismatch {
            keys: keys.into_iter().collect::<Vec<_>>().join(", "),
        });
    }
    Ok(on_disk.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMBEDDED: &str =
        "version: '1.0'\nkernel_capabilities: [models, events]\nspawn:\n  command: bin/sin90\n";

    #[test]
    fn missing_file_returns_embedded() {
        assert_eq!(manifest_to_send(EMBEDDED, None).unwrap(), EMBEDDED);
    }

    #[test]
    fn integrity_and_reformatting_preserve_disk_bytes() {
        let disk = "# published\nspawn: {command: bin/sin90}\nintegrity:\n  files: {bin/sin90: abc}\nkernel_capabilities:\n- models\n- events\nversion: \"1.0\"\n";
        assert_eq!(manifest_to_send(EMBEDDED, Some(disk)).unwrap(), disk);
    }

    #[test]
    fn business_changes_are_rejected_without_disclosing_values() {
        for (old, new, key) in [
            ("'1.0'", "secret-version", "version"),
            ("models, events", "secret-capability", "kernel_capabilities"),
            ("bin/sin90", "secret-command", "spawn"),
        ] {
            let disk = EMBEDDED.replace(old, new);
            let error = manifest_to_send(EMBEDDED, Some(&disk)).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("manifest differs from compiled baseline at top-level keys: {key}")
            );
        }
    }

    #[test]
    fn added_removed_and_multiple_keys_are_reported() {
        let error =
            manifest_to_send(EMBEDDED, Some("version: other\nextra: secret\n")).unwrap_err();
        assert_eq!(error.to_string(), "manifest differs from compiled baseline at top-level keys: extra, kernel_capabilities, spawn, version");
    }

    #[test]
    fn invalid_yaml_is_rejected() {
        assert!(matches!(
            manifest_to_send(EMBEDDED, Some("version: [")),
            Err(ManifestError::InvalidYaml { origin: "on-disk" })
        ));
    }

    #[test]
    fn invalid_mapping_is_rejected() {
        for disk in ["", "[]", "42", "1: secret"] {
            assert!(matches!(
                manifest_to_send(EMBEDDED, Some(disk)),
                Err(ManifestError::InvalidMapping { .. })
            ));
        }
    }

    #[test]
    fn actual_compiled_manifest_accepts_integrity_and_rejects_wrong_model_access() {
        let embedded = crate::ai::MANIFEST_YAML;
        let disk = format!(
            "{}\nintegrity: {{files: {{}}}}\n",
            serde_yaml::to_string(&parse(embedded, "embedded").unwrap()).unwrap()
        );
        assert_eq!(manifest_to_send(embedded, Some(&disk)).unwrap(), disk);
        let mut changed = parse(&disk, "on-disk").unwrap();
        let wrong = if cfg!(feature = "remote-allowed-manifest") {
            "local_only"
        } else {
            "remote_allowed"
        };
        changed.insert(
            Value::String("model_access".into()),
            Value::String(wrong.into()),
        );
        let disk = serde_yaml::to_string(&changed).unwrap();
        assert_eq!(
            manifest_to_send(embedded, Some(&disk))
                .unwrap_err()
                .to_string(),
            "manifest differs from compiled baseline at top-level keys: model_access"
        );
    }
}
