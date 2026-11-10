//! Private process and state for the Kathamo provider snapshot.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::Command;

mod bundled {
    include!(concat!(env!("OUT_DIR"), "/provider.rs"));
    std::arch::global_asm!(
        include_str!(concat!(env!("OUT_DIR"), "/provider.s")),
        options(raw)
    );

    unsafe extern "C" {
        static kathamo_provider_payload: [u8; BUNDLE_SIZE];
    }

    pub(super) fn payload() -> (&'static [u8], u64, &'static str) {
        // The generated assembly defines this array as exactly BUNDLE_SIZE bytes.
        (unsafe { &kathamo_provider_payload }, UNPACKED_SIZE, VERSION)
    }
}

pub fn root() -> PathBuf {
    let data = std::env::var_os("KATHAMO_DATA_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            let dirs = directories::BaseDirs::new()?;
            Some(if cfg!(target_os = "macos") {
                dirs.home_dir().join("Library/Application Support/Kathamo")
            } else if cfg!(windows) {
                dirs.data_local_dir().join("Kathamo")
            } else {
                dirs.data_local_dir().join("kathamo")
            })
        })
        .unwrap_or_else(|| PathBuf::from(".kathamo"));
    std::path::absolute(data.join("provider")).expect("resolve Kathamo provider data directory")
}

pub fn agent_dir() -> PathBuf {
    root().join("agent")
}

pub fn executable() -> Result<PathBuf, String> {
    let (bundle, unpacked_size, version) = bundled::payload();
    let dir = root().join("runtime").join(version);
    let executable = dir.join(if cfg!(windows) {
        "kathamo-provider.exe"
    } else {
        "kathamo-provider"
    });
    if executable
        .metadata()
        .is_ok_and(|metadata| metadata.len() == unpacked_size)
    {
        return Ok(executable);
    }

    let unpack = || -> std::io::Result<()> {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            dir.join("LICENSE-OH-MY-PI"),
            include_str!("../LICENSE-OH-MY-PI"),
        )?;
        std::fs::write(
            dir.join("THIRD-PARTY-NOTICES-OH-MY-PI.txt"),
            include_str!("../THIRD-PARTY-NOTICES-OH-MY-PI.txt"),
        )?;
        let mut temporary = tempfile::NamedTempFile::new_in(&dir)?;
        std::io::copy(&mut flate2::read::GzDecoder::new(bundle), &mut temporary)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temporary
                .as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o700))?;
        }
        temporary.persist(&executable)?;
        Ok(())
    };
    unpack().map_err(|error| format!("Could not unpack the Kathamo provider runtime: {error}"))?;
    Ok(executable)
}

/// Child-only overrides. Login, RPC and credential management share this namespace.
pub fn environment() -> Result<Vec<(OsString, OsString)>, String> {
    let root = root();
    let home = root.join("home");
    std::fs::create_dir_all(&home)
        .map_err(|error| format!("Could not create the Kathamo provider directory: {error}"))?;
    Ok([
        ("HOME", home.clone().into_os_string()),
        ("USERPROFILE", home.into_os_string()),
        ("OMP_PROFILE", "".into()),
        ("PI_PROFILE", "".into()),
        ("OMP_AUTH_BROKER_URL", "".into()),
        ("OMP_AUTH_BROKER_TOKEN", "".into()),
        ("PI_CONFIG_FILES", "".into()),
        ("PI_CONFIG_DIR", ".kathamo-provider".into()),
        ("PI_CODING_AGENT_DIR", agent_dir().into_os_string()),
        ("XDG_CONFIG_HOME", root.join("config").into_os_string()),
        ("XDG_DATA_HOME", root.join("data").into_os_string()),
        ("XDG_STATE_HOME", root.join("state").into_os_string()),
        ("XDG_CACHE_HOME", root.join("cache").into_os_string()),
        ("APPDATA", root.join("config").into_os_string()),
        ("LOCALAPPDATA", root.join("data").into_os_string()),
    ]
    .into_iter()
    .map(|(key, value)| (key.into(), value))
    .collect())
}

pub fn inherits(key: &OsStr) -> bool {
    let key = key.to_string_lossy().to_ascii_uppercase();
    !key.starts_with("OMP_") && !key.starts_with("PI_")
}

pub fn command() -> Result<Command, String> {
    let mut command = Command::new(executable()?);
    for (key, _) in std::env::vars_os().filter(|(key, _)| !inherits(key)) {
        command.env_remove(key);
    }
    command.envs(environment()?);
    Ok(command)
}
