use std::path::{Path, PathBuf};

pub(super) fn is_yaml(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|s| s.to_str()),
        Some("yaml" | "yml")
    )
}

pub(super) fn paths(dir: &Path) -> Vec<PathBuf> {
    let paths: Vec<_> = ["config.toml", "config.yaml", "config.yml"]
        .iter()
        .map(|name| dir.join(name))
        .filter(|path| path.symlink_metadata().is_ok())
        .collect();
    if paths.is_empty() {
        vec![dir.join("config.toml")]
    } else {
        paths
    }
}

pub(super) fn select(dir: &Path) -> Result<PathBuf, String> {
    let paths = paths(dir);
    if paths.len() > 1 {
        return Err(format!(
            "conflicting config files: {}; keep one config file at this level",
            paths
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(paths.into_iter().next().unwrap())
}

pub(super) fn parse(path: &Path, text: &str) -> Result<toml::Value, String> {
    if !is_yaml(path) {
        return super::parse_doc(text).map_err(|e| e.to_string());
    }
    let value: serde_yaml::Value = serde_yaml::from_str(text).map_err(|e| e.to_string())?;
    if value.is_null()
        && text.lines().all(|line| {
            matches!(
                line.split('#').next().unwrap_or("").trim(),
                "" | "---" | "..."
            )
        })
    {
        return Ok(toml::Value::Table(toml::Table::new()));
    }
    if !value.is_mapping() {
        return Err("config must be a mapping of setting names to values".into());
    }
    convert(value, "config")
}

fn convert(value: serde_yaml::Value, location: &str) -> Result<toml::Value, String> {
    use serde_yaml::Value;
    Ok(match value {
        Value::Null => {
            return Err(format!(
                "{location}: null is not supported; omit the setting to use its default"
            ));
        }
        Value::Bool(v) => toml::Value::Boolean(v),
        Value::Number(v) => {
            if let Some(v) = v.as_i64() {
                toml::Value::Integer(v)
            } else if v.as_u64().is_some() {
                return Err(format!(
                    "{location}: integer is outside the signed 64-bit range"
                ));
            } else {
                toml::Value::Float(
                    v.as_f64()
                        .ok_or_else(|| format!("{location}: invalid number"))?,
                )
            }
        }
        Value::String(v) => toml::Value::String(v),
        Value::Sequence(values) => toml::Value::Array(
            values
                .into_iter()
                .enumerate()
                .map(|(i, value)| convert(value, &format!("{location}[{i}]")))
                .collect::<Result<_, _>>()?,
        ),
        Value::Mapping(values) => {
            let mut table = toml::Table::new();
            for (key, value) in values {
                let Value::String(key) = key else {
                    return Err(format!("{location}: mapping keys must be strings"));
                };
                if key == "<<" {
                    return Err(format!(
                        "{location}: YAML merge keys are not supported; use explicit settings"
                    ));
                }
                let value = convert(value, &format!("{location}.{key}"))?;
                table.insert(key, value);
            }
            toml::Value::Table(table)
        }
        Value::Tagged(_) => return Err(format!("{location}: YAML tags are not supported")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_supports_empty_documents_and_anchors() {
        for text in ["", "# comment\n", "---\n# comment\n...\n", "{}"] {
            assert_eq!(
                parse(Path::new("config.yaml"), text).unwrap(),
                toml::Value::Table(toml::Table::new())
            );
        }
        let value = parse(
            Path::new("config.yml"),
            "aliases:\n  po: &pods pods\n  p: *pods\n",
        )
        .unwrap();
        assert_eq!(value["aliases"]["po"], value["aliases"]["p"]);
    }

    #[test]
    fn yaml_rejects_ambiguous_or_unsupported_values() {
        for text in [
            "null",
            "~",
            "[]",
            "readonly: true\nreadonly: false",
            "readonly: null",
            "keys: {table: {down: [null]}}",
            "aliases: {1: pods}",
            "aliases: {po: !custom pods}",
            "readonly: true\n---\nreadonly: false",
            "aliases: {<<: {po: pods}}",
            "mouse_scroll_lines: 18446744073709551615",
            "aliases: {po: *missing}",
        ] {
            assert!(parse(Path::new("config.yaml"), text).is_err(), "{text}");
        }
    }

    #[test]
    fn documented_settings_have_the_same_values_and_validation_in_yaml() {
        let mut count = 0;
        for doc in [
            include_str!("../../docs/configuration.md"),
            include_str!("../../docs/keybindings.md"),
            include_str!("../../docs/providers.md"),
            include_str!("../../docs/views.md"),
            include_str!("../../docs/safety.md"),
            include_str!("../../docs/debugging.md"),
            include_str!("../../docs/plugins.md"),
        ] {
            let examples: Vec<_> = if doc.contains("```toml\n") {
                doc.split("```toml\n")
                    .skip(1)
                    .map(|s| s.split("```").next().unwrap())
                    .collect()
            } else {
                vec![doc]
            };
            for text in examples {
                let Ok(toml) = super::super::parse_doc(text) else {
                    continue;
                };
                let yaml = serde_yaml::to_string(&toml).unwrap();
                for name in ["config.yaml", "config.yml"] {
                    let parsed = parse(Path::new(name), &yaml).unwrap();
                    assert_eq!(toml, parsed, "{text}");
                    assert_eq!(
                        super::super::validate(text).is_ok(),
                        super::super::validate_file(Path::new(name), &yaml).is_ok(),
                        "{text}"
                    );
                }
                count += 1;
            }
        }
        assert!(count >= 15, "checked {count} examples");
    }
}
