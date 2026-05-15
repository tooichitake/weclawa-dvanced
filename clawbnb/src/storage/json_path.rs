//! In-memory `serde_json::Value` path-walk helpers used by:
//!
//! - `cli/config.rs` (operator escape-hatch CLI — `weclawbot config set
//!   /ai/timeoutMs 600000` style)
//! - `wechat_menu/apply.rs::set_field` (per-user settings.json mutations
//!   from the WeChat `/menu` console)
//!
//! Consolidating walk + intermediate-object creation here means edge
//! cases (overwriting a scalar with a nested path, etc.) have one
//! consistent implementation.
//!
//! File I/O is **not** done here — callers compose these helpers with
//! their own atomic-write step.
//!
//! v7.0 housekeeping: removed `array_add_unique` / `array_remove` /
//! `descend_to_array` — every caller was dead after the WeChat menu
//! dropped list-management commands. See `wechat_menu/apply.rs` for a
//! migration note if list ops come back.

use serde_json::{Map, Value};

/// Set the leaf at `path` to `new_value`. Creates intermediate objects as
/// needed. If a path segment exists but isn't an object, it's overwritten
/// with an empty object before descending.
///
/// Empty `path` is rejected.
pub fn set_at(root: &mut Value, path: &[&str], new_value: Value) -> Result<(), String> {
    if path.is_empty() {
        return Err("empty key path".into());
    }
    if !root.is_object() {
        *root = Value::Object(Map::new());
    }
    let obj = root.as_object_mut().unwrap();
    descend_and_set(obj, path, new_value);
    Ok(())
}

fn descend_and_set(obj: &mut Map<String, Value>, path: &[&str], value: Value) {
    let key = path[0].to_string();
    if path.len() == 1 {
        obj.insert(key, value);
        return;
    }
    let entry = obj.entry(key).or_insert_with(|| Value::Object(Map::new()));
    let nested = match entry {
        Value::Object(map) => map,
        other => {
            *other = Value::Object(Map::new());
            other.as_object_mut().unwrap()
        }
    };
    descend_and_set(nested, &path[1..], value);
}

/// Set via a `/`-separated JSON pointer string. Mainly for the operator
/// CLI which takes user-typed pointers.
pub fn set_at_pointer(
    root: &mut Value,
    pointer: &str,
    new_value: Value,
) -> Result<(), String> {
    let parts: Vec<&str> = pointer.trim_start_matches('/').split('/').collect();
    set_at(root, &parts, new_value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn set_at_creates_intermediates() {
        let mut v = json!({});
        set_at(&mut v, &["a", "b", "c"], json!(42)).unwrap();
        assert_eq!(v, json!({"a": {"b": {"c": 42}}}));
    }

    #[test]
    fn set_at_preserves_siblings() {
        let mut v = json!({"a": {"b": 1, "c": 2}});
        set_at(&mut v, &["a", "c"], json!(99)).unwrap();
        assert_eq!(v, json!({"a": {"b": 1, "c": 99}}));
    }

    #[test]
    fn set_at_overwrites_scalar_to_object() {
        let mut v = json!({"a": 5});
        set_at(&mut v, &["a", "b"], json!("hi")).unwrap();
        assert_eq!(v, json!({"a": {"b": "hi"}}));
    }

    #[test]
    fn set_at_pointer_round_trip() {
        let mut v = json!({});
        set_at_pointer(&mut v, "/ai/timeoutMs", json!(600000)).unwrap();
        assert_eq!(v, json!({"ai": {"timeoutMs": 600000}}));
    }

    #[test]
    fn empty_path_errors() {
        let mut v = json!({});
        assert!(set_at(&mut v, &[], json!(0)).is_err());
    }
}
