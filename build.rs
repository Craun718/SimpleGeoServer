use std::fs;
use std::io::Write;
use std::path::Path;

fn main() {
    // Determine the path to crslist.json (relative to workspace root)
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    // crates/simple-geo-server/ -> src-tauri/ -> program/
    let workspace_root = Path::new(&manifest_dir)
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent());
    let json_path = match workspace_root {
        Some(dir) => dir.join("public").join("crslist.json"),
        None => {
            println!(
                "cargo:warning=Could not resolve workspace root from {}",
                manifest_dir
            );
            return;
        }
    };

    if !json_path.exists() {
        println!("cargo:warning=crslist.json not found at {:?}", json_path);
        return;
    }

    let content = fs::read_to_string(&json_path).expect("read crslist.json");
    let parsed: serde_json::Value = serde_json::from_str(&content).expect("parse crslist.json");

    let mut entries: Vec<(u16, String)> = Vec::new();
    if let Some(arr) = parsed.as_array() {
        for entry in arr {
            let code = entry.get("code").and_then(|v| v.as_str());
            let name = entry.get("name").and_then(|v| v.as_str());
            if let (Some(c), Some(n)) = (code, name) {
                if let Ok(code_num) = c.parse::<u16>() {
                    entries.push((code_num, n.to_string()));
                }
            }
        }
    }

    entries.sort_by_key(|(code, _)| *code);
    entries.dedup_by_key(|(code, _)| *code);

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let dest = Path::new(&out_dir).join("crs_names.rs");
    let mut f = fs::File::create(&dest).unwrap();

    writeln!(f, "static CRS_NAME_TABLE: &[(u16, &str)] = &[").unwrap();
    for (code, name) in &entries {
        writeln!(f, "    ({}, {:?}),", code, name).unwrap();
    }
    writeln!(f, "];").unwrap();

    println!("cargo:rerun-if-changed={}", json_path.display());
}
