//! Partial saves for settings-like objects: only the fields that were sent change, unknown
//! fields are ignored, and a field of the wrong type gets an error that names it
//! (`per_interval: must be a whole number.`), never a generic one.

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use crate::EngineError;

/// `current` with `patch` laid over it.
pub(crate) fn merge<T: Serialize + DeserializeOwned>(
    current: &T,
    patch: &Map<String, Value>,
) -> Result<T, EngineError> {
    let base = serde_json::to_value(current).map_err(|e| EngineError::Internal(e.to_string()))?;
    let mut merged = base.clone();
    for (k, v) in patch {
        let Some(slot) = merged.get_mut(k) else { continue };
        let mut one = base.clone();
        one[k.as_str()] = v.clone();
        if serde_json::from_value::<T>(one).is_err() {
            let want = match &base[k.as_str()] {
                Value::Bool(_) => "true or false",
                Value::Number(n) if n.is_u64() => "a whole number of 0 or more",
                Value::Number(_) => "a number",
                Value::Array(_) => "a list",
                _ => "text",
            };
            return Err(EngineError::Invalid(format!("{k}: must be {want}.")));
        }
        *slot = v.clone();
    }
    serde_json::from_value(merged).map_err(|e| EngineError::Invalid(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Cfg {
        on: bool,
        n: u32,
        name: String,
    }

    fn patch(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn merges_only_sent_fields_and_names_bad_ones() {
        let c = Cfg { on: true, n: 5, name: "x".into() };
        let out = merge(&c, &patch(json!({ "n": 9, "unknown": 1 }))).unwrap();
        assert_eq!(out, Cfg { n: 9, ..c.clone() });
        let err = |v: Value| merge(&c, &patch(v)).unwrap_err().to_string();
        assert_eq!(err(json!({ "n": -1 })), "n: must be a whole number of 0 or more.");
        assert_eq!(err(json!({ "n": 1.5 })), "n: must be a whole number of 0 or more.");
        assert_eq!(err(json!({ "n": "9" })), "n: must be a whole number of 0 or more.");
        assert_eq!(err(json!({ "on": "yes" })), "on: must be true or false.");
        assert_eq!(err(json!({ "name": null })), "name: must be text.");
    }
}
