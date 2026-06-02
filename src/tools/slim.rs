use serde_json::Value;

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
}
