use crate::types::StoredCredential;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// Query the set of providers with active credentials in omp's SQLite database (~/.omp/agent/agent.db).
pub fn omp_authenticated_providers() -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    let home = match directories::BaseDirs::new() {
        Some(d) => d.home_dir().to_path_buf(),
        None => return set,
    };
    let db_path = home.join(".omp").join("agent").join("agent.db");
    if !db_path.exists() {
        return set;
    }

    if let Ok(output) = Command::new("sqlite3")
        .arg(&db_path)
        .arg("SELECT DISTINCT provider FROM auth_credentials WHERE disabled_cause IS NULL;")
        .output()
    {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    set.insert(trimmed.to_string());
                }
            }
        }
    }
    set
}

/// Check if omp CLI has active credentials for a provider.
pub fn check_omp_credential(provider_id: &str) -> Option<(bool, String)> {
    let active = omp_authenticated_providers();
    if active.contains(provider_id) {
        Some((true, "omp OAuth".to_string()))
    } else {
        None
    }
}
/// Retrieve raw token string from omp if available.
pub fn get_omp_token(provider_id: &str) -> Option<String> {
    let output = Command::new("omp")
        .args(["token", provider_id, "--raw"])
        .output()
        .ok()?;

    if output.status.success() {
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}

fn credentials_file_path() -> Option<PathBuf> {
    if let Some(custom) = std::env::var_os("KATHAMO_DATA_DIR").filter(|v| !v.is_empty()) {
        return Some(
            PathBuf::from(custom)
                .join("config")
                .join("credentials.json"),
        );
    }
    directories::BaseDirs::new()
        .map(|dirs| dirs.config_dir().join("kathamo").join("credentials.json"))
}

/// Load credentials map from Kathamo's credentials store.
pub fn load_local_credentials() -> HashMap<String, StoredCredential> {
    let Some(path) = credentials_file_path() else {
        return HashMap::new();
    };

    if !path.exists() {
        return HashMap::new();
    }

    let Ok(data) = fs::read_to_string(&path) else {
        return HashMap::new();
    };

    serde_json::from_str(&data).unwrap_or_default()
}

/// Save a credential for a provider in Kathamo's credentials store.
pub fn save_local_credential(provider_id: &str, cred: StoredCredential) -> Result<(), String> {
    let path = credentials_file_path()
        .ok_or_else(|| "Could not determine config directory".to_string())?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create config dir: {e}"))?;
    }

    let mut map = load_local_credentials();
    map.insert(provider_id.to_string(), cred);

    let json = serde_json::to_string_pretty(&map)
        .map_err(|e| format!("Failed to serialize credentials: {e}"))?;

    fs::write(&path, json).map_err(|e| format!("Failed to write credentials file: {e}"))?;
    Ok(())
}

/// Retrieve a stored credential for a provider (checking local store first, then omp).
/// Retrieve a credential from local store only (fast, no external CLI calls).
pub fn get_local_credential(provider_id: &str) -> Option<StoredCredential> {
    let local_map = load_local_credentials();
    local_map.get(provider_id).cloned()
}

/// Retrieve a stored credential for a provider (checking local store first, then omp if active).
pub fn get_credential(provider_id: &str) -> Option<StoredCredential> {
    if let Some(cred) = get_local_credential(provider_id) {
        return Some(cred);
    }

    let active = omp_authenticated_providers();
    if active.contains(provider_id) {
        if let Some(token) = get_omp_token(provider_id) {
            return Some(StoredCredential::OAuth {
                access: token,
                refresh: None,
                expires: None,
                email: None,
                account_id: None,
                org_id: None,
                org_name: None,
                authorized_at: None,
            });
        }
    }

    None
}

/// Save an API key for a provider to local storage, process environment, and OMP credential store.
pub fn save_api_key(provider_id: &str, key: &str) -> Result<(), String> {
    let key = key.trim();
    if key.is_empty() {
        return Err("API key cannot be empty".to_string());
    }

    // 1. Save to local Kathamo credentials store
    let cred = StoredCredential::ApiKey {
        key: key.to_string(),
        source: Some("Stored API key".to_string()),
    };
    save_local_credential(provider_id, cred)?;

    // 2. Set environment variable in the process if known
    if let Some(spec) = crate::catalog::get_provider(provider_id) {
        for var in &spec.env_vars {
            unsafe {
                std::env::set_var(var, key);
            }
        }
    }
    let upper = provider_id.replace('-', "_").to_uppercase();
    let fallback_var = format!("{upper}_API_KEY");
    unsafe {
        std::env::set_var(&fallback_var, key);
    }
    // 3. Import into OMP credentials store via `omp auth import -`
    let import_doc = serde_json::json!({
        "version": 1,
        "credentials": [
            {
                "provider": provider_id,
                "credential": {
                    "type": "api_key",
                    "key": key,
                    "source": "login"
                }
            }
        ]
    });

    if let Ok(mut child) = Command::new("omp")
        .args(["auth", "import", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            let _ = stdin.write_all(import_doc.to_string().as_bytes());
        }
        let _ = child.wait();
    }

    Ok(())
}

/// Disconnect a provider: remove its stored credentials from local storage and OMP agent.db,
/// and unset its environment variables in the current process.
pub fn disconnect_provider(provider_id: &str) -> Result<(), String> {
    // 1. Remove from local Kathamo credentials file
    if let Some(path) = credentials_file_path() {
        if path.exists() {
            let mut map = load_local_credentials();
            map.remove(provider_id);
            if let Ok(json) = serde_json::to_string_pretty(&map) {
                let _ = fs::write(&path, json);
            }
        }
    }

    // 2. Remove from OMP SQLite store (~/.omp/agent/agent.db)
    if let Some(dirs) = directories::BaseDirs::new() {
        let db_path = dirs.home_dir().join(".omp").join("agent").join("agent.db");
        if db_path.exists() {
            let _ = Command::new("sqlite3")
                .arg(&db_path)
                .arg(format!(
                    "DELETE FROM auth_credentials WHERE provider='{provider_id}';"
                ))
                .output();
        }
    }

    // 3. Unset environment variables in the running process
    if let Some(spec) = crate::catalog::get_provider(provider_id) {
        for var in &spec.env_vars {
            unsafe {
                std::env::remove_var(var);
            }
        }
    }
    let upper = provider_id.replace('-', "_").to_uppercase();
    let fallback_var = format!("{upper}_API_KEY");
    unsafe {
        std::env::remove_var(&fallback_var);
    }

    Ok(())
}

/// Restore all locally stored API keys into the process environment variables.
pub fn restore_stored_credentials() {
    let creds = load_local_credentials();
    for (provider_id, cred) in creds {
        if let StoredCredential::ApiKey { key, .. } = cred {
            if let Some(spec) = crate::catalog::get_provider(&provider_id) {
                for var in &spec.env_vars {
                    if std::env::var(var).is_err() {
                        unsafe {
                            std::env::set_var(var, &key);
                        }
                    }
                }
            }
            let upper = provider_id.replace('-', "_").to_uppercase();
            let fallback_var = format!("{upper}_API_KEY");
            if std::env::var(&fallback_var).is_err() {
                unsafe {
                    std::env::set_var(&fallback_var, &key);
                }
            }
        }
    }
}
