//! In-memory `serde_json::Value` path-walk helpers shared by:
//!
//! - `cli/config.rs` (operator escape-hatch CLI — `weclawbot config set
//!   /ai/timeoutMs 600000` style)
//! - `wechat_menu/apply.rs` (per-user settings.json mutations from the
//!   WeChat `/menu` console)
//!
//! Both used to carry hand-rolled "walk + create intermediate objects"
//! code; consolidating here means edge cases (overwriting a scalar with a
//! nested path, replacing a non-array with an array on first array op,
//! etc.) get one consistent implementation.
//!
//! File I/O is **not** done here — callers compose these helpers with
//! their own atomic-write step.

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

/// Append `value` to an array at `path` if not already present. Creates
/// intermediates as needed; if the leaf exists but isn't an array,
/// overwrites it with an empty array first.
pub fn array_add_unique(root: &mut Value, path: &[&str], value: Value) -> Result<(), String> {
    let arr = descend_to_array(root, path)?;
    if !arr.contains(&value) {
        arr.push(value);
    }
    Ok(())
}

/// Remove all occurrences of `value` from an array at `path`. Returns
/// whether anything was removed. No-op + `Ok(false)` if the leaf doesn't
/// exist or isn't an array.
pub fn array_remove(root: &mut Value, path: &[&str], value: &Value) -> Result<bool, String> {
    let arr = descend_to_array(root, path)?;
    let before = arr.len();
    arr.retain(|v| v != value);
    Ok(arr.len() != before)
}

fn descend_to_array<'a>(
    root: &'a mut Value,
    path: &[&str],
) -> Result<&'a mut Vec<Value>, String> {
    if path.is_empty() {
        return Err("empty key path".into());
    }
    if !root.is_object() {
        *root = Value::Object(Map::new());
    }
    let mut cur = root.as_object_mut().unwrap();
    for (i, key) in path.iter().enumerate() {
        let is_last = i == path.len() - 1;
        let entry = cur.entry((*key).to_string()).or_insert_with(|| {
            if is_last {
                Value::Array(Vec::new())
            } else {
                Value::Object(Map::new())
            }
        });
        if is_last {
            return match entry {
                Value::Array(arr) => Ok(arr),
                other => {
                    *other = Value::Array(Vec::new());
                    Ok(other.as_array_mut().unwrap())
                }
            };
        }
        cur = match entry {
            Value::Object(m) => m,
            other => {
                *other = Value::Object(Map::new());
                other.as_object_mut().unwrap()
            }
        };
    }
    unreachable!("loop returns on last iteration")
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
    fn array_add_unique_appends_once() {
        let mut v = json!({});
        array_add_unique(&mut v, &["allow"], json!("x")).unwrap();
        array_add_unique(&mut v, &["allow"], json!("x")).unwrap();
        array_add_unique(&mut v, &["allow"], json!("y")).unwrap();
        assert_eq!(v["allow"], json!(["x", "y"]));
    }

    #[test]
    fn array_remove_works() {
        let mut v = json!({"allow": ["x", "y", "x"]});
        let changed = array_remove(&mut v, &["allow"], &json!("x")).unwrap();
        assert!(changed);
        assert_eq!(v["allow"], json!(["y"]));
    }

    #[test]
    fn array_remove_noop_when_absent() {
        let mut v = json!({"allow": ["y"]});
        let changed = array_remove(&mut v, &["allow"], &json!("nope")).unwrap();
        assert!(!changed);
    }

    #[test]
    fn empty_path_errors() {
        let mut v = json!({});
        assert!(set_at(&mut v, &[], json!(0)).is_err());
    }
}
