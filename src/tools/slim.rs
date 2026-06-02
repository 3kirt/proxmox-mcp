use chrono::DateTime;
use serde_json::Value;

/// Object keys whose integer value is a UNIX epoch (seconds). For each one we
/// add a human-readable `<key>_iso` sibling so timestamps aren't opaque epochs.
const EPOCH_KEYS: &[&str] = &["ctime", "starttime", "endtime"];

/// Recursively add an ISO 8601 `<key>_iso` sibling next to every known epoch
/// field (see `EPOCH_KEYS`). The original numeric field is left untouched.
pub fn humanize_value(v: Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, val) in map {
                let val = humanize_value(val);
                let iso = if EPOCH_KEYS.contains(&k.as_str()) {
                    val.as_i64().and_then(epoch_to_iso)
                } else {
                    None
                };
                let iso_key = iso.is_some().then(|| format!("{k}_iso"));
                out.insert(k, val);
                // Emit the `<key>_iso` sibling right after its source field.
                if let (Some(key), Some(iso)) = (iso_key, iso) {
                    out.insert(key, Value::String(iso));
                }
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.into_iter().map(humanize_value).collect()),
        other => other,
    }
}

/// Format an epoch-seconds value as RFC 3339 / ISO 8601 (UTC). Values below
/// 2001-09-09 are treated as non-timestamps (e.g. a `0` sentinel) and skipped.
fn epoch_to_iso(secs: i64) -> Option<String> {
    if secs < 1_000_000_000 {
        return None;
    }
    DateTime::from_timestamp(secs, 0).map(|dt| dt.to_rfc3339())
}

/// Recursively remove null-valued fields from every object in a response.
/// Proxmox returns many always-present optional fields as `null`; dropping
/// them trims payloads without losing information.
pub fn slim_value(v: Value) -> Value {
    match v {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k, slim_value(v)))
                .collect(),
        ),
        Value::Array(arr) => Value::Array(arr.into_iter().map(slim_value).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn drops_null_fields_recursively() {
        let v = json!({
            "name": "vm1",
            "lock": null,
            "net": { "model": "virtio", "rate": null }
        });
        assert_eq!(
            slim_value(v),
            json!({ "name": "vm1", "net": { "model": "virtio" } })
        );
    }

    #[test]
    fn slims_inside_arrays() {
        let v = json!([{ "id": 1, "x": null }, { "id": 2, "x": null }]);
        assert_eq!(slim_value(v), json!([{ "id": 1 }, { "id": 2 }]));
    }

    #[test]
    fn passes_primitives_and_top_level_null() {
        assert_eq!(slim_value(json!(42)), json!(42));
        assert_eq!(slim_value(json!("x")), json!("x"));
        assert_eq!(slim_value(json!(null)), json!(null));
    }

    #[test]
    fn humanize_adds_iso_sibling_for_known_epoch_keys() {
        // 1700000000 = 2023-11-14T22:13:20Z
        let v = json!({ "ctime": 1_700_000_000, "name": "snap1" });
        let out = humanize_value(v);
        assert_eq!(out["ctime"], json!(1_700_000_000));
        assert_eq!(out["ctime_iso"], json!("2023-11-14T22:13:20+00:00"));
        assert_eq!(out["name"], json!("snap1"));
    }

    #[test]
    fn humanize_recurses_into_arrays_and_objects() {
        let v = json!([{ "starttime": 1_700_000_000, "endtime": 1_700_000_060 }]);
        let out = humanize_value(v);
        assert_eq!(out[0]["starttime_iso"], json!("2023-11-14T22:13:20+00:00"));
        assert_eq!(out[0]["endtime_iso"], json!("2023-11-14T22:14:20+00:00"));
    }

    #[test]
    fn humanize_skips_non_epoch_and_unknown_keys() {
        // `uptime` is a duration, not an epoch; `endtime` of 0 is a sentinel.
        let v = json!({ "uptime": 3600, "endtime": 0 });
        let out = humanize_value(v.clone());
        assert_eq!(out, v);
        assert!(out.get("endtime_iso").is_none());
    }
}
