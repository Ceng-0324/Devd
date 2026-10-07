use std::collections::BTreeMap;

use serde::{
    de::{DeserializeOwned, Error as _},
    Deserialize, Deserializer,
};
use serde_yaml::{Mapping, Value};
use thiserror::Error;

use super::{ConfigValidationError, DevdConfig, ServiceConfig};

#[derive(Debug, Error)]
pub enum ProfileError {
    #[error("invalid profile name '{0}': start with an ASCII letter, digit, or underscore; use only ASCII letters, digits, '_', '-', or '.'")]
    InvalidName(String),
    #[error("unknown profile '{name}'; available profiles: {available}")]
    Unknown { name: String, available: String },
    #[error("profile '{name}': {source}")]
    Parse {
        name: String,
        #[source]
        source: serde_yaml::Error,
    },
    #[error("profile '{name}': {source}")]
    Validation {
        name: String,
        #[source]
        source: ConfigValidationError,
    },
}

/// Also used by control commands, which must not read the configuration file.
pub fn validate_profile_name(name: &str) -> Result<(), ProfileError> {
    if !name
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
    {
        return Err(ProfileError::InvalidName(name.to_owned()));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ConfigDocument {
    version: String,
    #[serde(default, deserialize_with = "deserialize_named_maps")]
    services: BTreeMap<String, Mapping>,
    #[serde(default, deserialize_with = "deserialize_named_maps")]
    profiles: BTreeMap<String, Profile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    #[serde(default, deserialize_with = "deserialize_named_maps")]
    services: BTreeMap<String, Mapping>,
}

fn deserialize_named_maps<'de, D, T>(deserializer: D) -> Result<BTreeMap<String, T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    // Value rejects duplicate keys recursively; map deserialization alone can
    // silently overwrite them and accepts null as an empty map.
    let value = Value::deserialize(deserializer)?;
    let map = value
        .as_mapping()
        .ok_or_else(|| D::Error::custom("expected a mapping"))?;
    if map.values().any(|entry| !entry.is_mapping()) {
        return Err(D::Error::custom("named definitions must be mappings"));
    }
    serde_yaml::from_value(value).map_err(D::Error::custom)
}

impl ConfigDocument {
    pub(super) fn parse(contents: &str) -> Result<(Self, DevdConfig), serde_yaml::Error> {
        let mut document: Self = serde_yaml::from_str(contents)?;
        for service in document.services.values_mut() {
            normalize_service(service)?;
        }
        // Parse the base and every override strictly, even if not selected.
        let base = Self::decode(document.version.clone(), document.services.clone())?;
        for (name, profile) in &mut document.profiles {
            validate_profile_name(name).map_err(serde_yaml::Error::custom)?;
            for (service_name, service) in &mut profile.services {
                let result = (|| {
                    normalize_service(service)?;
                    for (key, value) in service.iter() {
                        if value.is_null()
                            && !matches!(
                                key.as_str(),
                                Some("cwd" | "env-file" | "healthcheck" | "limits")
                            )
                        {
                            return Err(serde_yaml::Error::custom(format!(
                                "field {key:?} cannot be null"
                            )));
                        }
                    }
                    let mut partial = service.clone();
                    partial
                        .entry(Value::from("command"))
                        .or_insert(Value::from("profile-placeholder"));
                    serde_yaml::from_value::<ServiceConfig>(Value::Mapping(partial))?;
                    Ok::<_, serde_yaml::Error>(())
                })();
                result.map_err(|error| {
                    serde_yaml::Error::custom(format!(
                        "profiles.{name}.services.{service_name}: {error}"
                    ))
                })?;
            }
        }
        Ok((document, base))
    }

    pub(super) fn select(self, name: &str) -> Result<DevdConfig, ProfileError> {
        validate_profile_name(name)?;
        let Self {
            version,
            mut services,
            profiles,
        } = self;
        let profile = profiles.get(name).ok_or_else(|| ProfileError::Unknown {
            name: name.to_owned(),
            available: if profiles.is_empty() {
                "(none)".to_owned()
            } else {
                profiles.keys().cloned().collect::<Vec<_>>().join(", ")
            },
        })?;
        for (service_name, overrides) in &profile.services {
            let service = services.entry(service_name.clone()).or_default();
            for (key, value) in overrides {
                // Only env and restart are field-wise overlays. Tagged probes
                // and dependency lists must replace the entire old value.
                if matches!(key.as_str(), Some("env" | "restart")) {
                    if let (Some(Value::Mapping(base)), Value::Mapping(patch)) =
                        (service.get_mut(key), value)
                    {
                        base.extend(patch.clone());
                        continue;
                    }
                }
                service.insert(key.clone(), value.clone());
            }
        }
        Self::decode(version, services).map_err(|source| ProfileError::Parse {
            name: name.to_owned(),
            source,
        })
    }

    fn decode(
        version: String,
        services: BTreeMap<String, Mapping>,
    ) -> Result<DevdConfig, serde_yaml::Error> {
        let services = services
            .into_iter()
            .map(|(name, fields)| {
                serde_yaml::from_value(Value::Mapping(fields))
                    .map(|service| (name.clone(), service))
                    .map_err(|error| serde_yaml::Error::custom(format!("services.{name}: {error}")))
            })
            .collect::<Result<_, _>>()?;
        Ok(DevdConfig { version, services })
    }
}

fn normalize_service(service: &mut Mapping) -> Result<(), serde_yaml::Error> {
    normalize_aliases(
        service,
        &[
            ("env_file", "env-file"),
            ("monitor_requires", "monitor-requires"),
            ("depends_on", "depends-on"),
            ("restart_on_dep_recovery", "restart-on-dep-recovery"),
        ],
    )?;
    if let Some(Value::Mapping(restart)) = service.get_mut(Value::from("restart")) {
        normalize_aliases(
            restart,
            &[
                ("initial_delay", "initial-delay"),
                ("max_delay", "max-delay"),
                ("max_attempts", "max-attempts"),
            ],
        )?;
    }
    Ok(())
}

fn normalize_aliases(map: &mut Mapping, aliases: &[(&str, &str)]) -> Result<(), serde_yaml::Error> {
    for (alias, canonical) in aliases {
        if let Some(value) = map.remove(Value::from(*alias)) {
            if map.insert(Value::from(*canonical), value).is_some() {
                return Err(serde_yaml::Error::custom(format!(
                    "duplicate field '{canonical}' (alias '{alias}')"
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_profile_aliases_and_mapping_order_preserve_effective_config() {
        let yaml = r#"
version: '1'
services:
  app:
    command: sleep 60
    env_file: base.env
    restart: {initial_delay: 2s, max_attempts: 9}
profiles:
  dev:
    services:
      app:
        env-file: dev.env
        restart: {max-attempts: 5}
"#;
        let reordered = r#"
profiles:
  dev:
    services:
      app:
        restart: {max_attempts: 5}
        env_file: dev.env
services:
  app:
    restart: {max-attempts: 9, initial-delay: 2s}
    env-file: base.env
    command: sleep 60
version: '1'
"#;
        let (first, base) = ConfigDocument::parse(yaml).unwrap();
        let (second, reordered_base) = ConfigDocument::parse(reordered).unwrap();
        assert_eq!(base, reordered_base);
        let effective = first.select("dev").unwrap();
        assert_eq!(effective, second.select("dev").unwrap());
        assert_eq!(effective.services["app"].restart.max_attempts, 5);
        assert_eq!(
            effective.services["app"].restart.initial_delay,
            base.services["app"].restart.initial_delay
        );
        assert_eq!(
            effective.services["app"].env_file.as_deref(),
            Some(std::path::Path::new("dev.env"))
        );
    }
}
