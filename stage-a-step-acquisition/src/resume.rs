//! Resume only explicit, finalized acquisitions with all local artifacts present.
use crate::protocol::Protocol;
use serde_json::Value;
use std::{collections::BTreeSet, path::Path};

pub(crate) fn completed(
    dir: &Path,
    id: &str,
    hash: &str,
    plan: &Protocol,
) -> Result<BTreeSet<usize>, String> {
    let mut found = BTreeSet::new();
    match std::fs::symlink_metadata(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(found),
        Err(e) => return Err(e.to_string()),
        Ok(m) if !m.is_dir() => return Err("measurement folder must be a real directory".into()),
        Ok(_) => {}
    }
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        if !entry.file_type().map_err(|e| e.to_string())?.is_file()
            || !entry.file_name().to_string_lossy().ends_with(".a2.json")
        {
            continue;
        }
        let bytes = std::fs::read(entry.path()).map_err(|e| e.to_string())?;
        let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        if value["experiment"] != "A2"
            || value["measurement_id"] != id
            || value["protocol_sha256"] != hash
            || value["evidence"]["acquisition_complete"] != true
            || !value["evidence"]["failure"].is_null()
        {
            continue;
        }
        let Some(index) = value["protocol_row"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .and_then(|n| n.checked_sub(1))
        else {
            continue;
        };
        let Some(point) = plan.points.get(index) else {
            continue;
        };
        if serde_json::to_value(point).map_err(|e| e.to_string())? != value["point"] {
            continue;
        }
        let fields = [
            ("raw_path", ".raw"),
            ("camera_configuration_sidecar_path", ".toml"),
            ("pdq_path", ".pdq"),
            ("pd_sidecar_path", ".pd.json"),
        ];
        if fields.iter().all(|(key, suffix)| {
            value["evidence"][key]
                .as_str()
                .is_some_and(|p| p.ends_with(suffix))
                && local_artifact(dir, &value["evidence"][key])
        }) {
            found.insert(index);
        }
    }
    Ok(found)
}

fn local_artifact(dir: &Path, value: &Value) -> bool {
    let Some(path) = value.as_str() else {
        return false;
    };
    let Some(name) = path
        .rsplit(['/', '\\'])
        .next()
        .filter(|n| !n.is_empty() && *n != "." && *n != "..")
    else {
        return false;
    };
    std::fs::symlink_metadata(dir.join(name)).is_ok_and(|m| m.is_file() && m.len() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn duplicate_conditions_are_distinct_rows_and_partial_records_do_not_count() {
        let mut plan =
            crate::protocol::parse(include_str!("../../plugins/stage-a-a2/protocols/a2_drive_sync_smoke.toml")).unwrap();
        plan.points = vec![plan.points[0].clone(); 3];
        let dir = tempfile::tempdir().unwrap();
        for suffix in ["raw", "toml", "pdq", "pd.json"] {
            std::fs::write(dir.path().join(format!("capture.{suffix}")), b"data").unwrap();
        }
        let mut value = json!({"experiment":"A2", "measurement_id":"id", "protocol_sha256":"hash",
            "protocol_row":2, "point":plan.points[1], "evidence":{
                "acquisition_complete":true, "failure":null,
                "raw_path":r"C:\old\capture.raw", "camera_configuration_sidecar_path":"capture.toml",
                "pdq_path":"capture.pdq", "pd_sidecar_path":"capture.pd.json"}});
        let path = dir.path().join("capture.a2.json");
        let check = |value: &Value| {
            std::fs::write(&path, value.to_string()).unwrap();
            completed(dir.path(), "id", "hash", &plan).unwrap()
        };
        assert_eq!(check(&value), BTreeSet::from([1]));
        value["evidence"]["acquisition_complete"] = Value::Null;
        assert!(check(&value).is_empty());
        value["evidence"]["acquisition_complete"] = Value::Bool(true);
        value["evidence"]["failure"] = Value::from("aborted");
        assert!(check(&value).is_empty());
        value["evidence"]["failure"] = Value::Null;
        value["protocol_row"] = json!(0);
        assert!(check(&value).is_empty());
        value["protocol_row"] = json!(2);
        value["point"]["label"] = json!("different");
        assert!(check(&value).is_empty());
        std::fs::write(&path, "truncated{").unwrap();
        assert!(completed(dir.path(), "id", "hash", &plan)
            .unwrap()
            .is_empty());
    }
}
