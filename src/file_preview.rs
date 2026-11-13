use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

const MAX_BYTES: u64 = 4 * 1024 * 1024;

pub struct FileContent {
    pub text: String,
    pub deleted: bool,
}

pub fn read(root: PathBuf, path: String) -> Result<FileContent, String> {
    let full_path = root.join(&path);
    let (bytes, deleted) = match std::fs::File::open(&full_path) {
        Ok(file) => {
            let mut bytes = Vec::new();
            file.take(MAX_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| format!("Could not read file: {e}"))?;
            (bytes, false)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let object = format!("HEAD:{path}");
            let size = Command::new("git")
                .current_dir(&root)
                .args(["cat-file", "-s", &object])
                .output()
                .map_err(|e| format!("Could not read deleted file: {e}"))?;
            if !size.status.success() {
                return Err("File no longer exists in the working tree or HEAD.".into());
            }
            let size: u64 = String::from_utf8_lossy(&size.stdout)
                .trim()
                .parse()
                .map_err(|_| "Could not determine file size.")?;
            if size > MAX_BYTES {
                return Err("File is larger than the 4 MiB preview limit.".into());
            }
            let output = Command::new("git")
                .current_dir(&root)
                .args(["show", "--no-ext-diff", &object])
                .output()
                .map_err(|e| format!("Could not read deleted file: {e}"))?;
            if !output.status.success() {
                return Err("Could not read the file from HEAD.".into());
            }
            (output.stdout, true)
        }
        Err(error) => return Err(format!("Could not open file: {error}")),
    };
    if bytes.len() as u64 > MAX_BYTES {
        return Err("File is larger than the 4 MiB preview limit.".into());
    }
    if bytes.contains(&0) {
        return Err("Binary files cannot be shown in the text editor.".into());
    }
    let text = String::from_utf8(bytes).map_err(|_| "File is not valid UTF-8 text.")?;
    Ok(FileContent { text, deleted })
}

/// Resolve local paths and file URLs without routing network URLs into the editor.
pub fn local_link_path(target: &str, base: &Path) -> Option<PathBuf> {
    if target.is_empty() || target.starts_with('#') {
        return None;
    }
    let (target, _, _) = terminal::hyperlink::parse_path_with_position(target);
    let url = match url::Url::parse(&target) {
        Ok(url) => url,
        Err(url::ParseError::RelativeUrlWithoutBase) => url::Url::from_directory_path(base)
            .ok()?
            .join(&target)
            .ok()?,
        Err(_) => return None,
    };
    if url.scheme() != "file" {
        return None;
    }
    url.to_file_path().ok()
}

pub fn language(path: &str) -> String {
    let name = Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if name.eq_ignore_ascii_case("makefile") {
        return "make".into();
    }
    match Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "js" | "mjs" | "cjs" | "jsx" => "javascript".into(),
        "ts" | "mts" | "cts" => "typescript".into(),
        "h" => "c".into(),
        "hh" | "hpp" | "cxx" | "cc" => "cpp".into(),
        "sh" | "zsh" => "bash".into(),
        extension => extension.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    #[test]
    fn resolves_local_links_without_intercepting_web_urls() {
        let base = Path::new("/tmp/my-project");
        assert_eq!(
            local_link_path("src/App.svelte", base),
            Some(base.join("src/App.svelte"))
        );
        assert_eq!(
            local_link_path("file:///tmp/hello%20world.rs", base),
            Some(PathBuf::from("/tmp/hello world.rs"))
        );
        assert_eq!(
            local_link_path("src/main.rs:12:3", base),
            Some(base.join("src/main.rs"))
        );
        assert_eq!(local_link_path("https://example.com/a.md", base), None);
        assert_eq!(local_link_path("#section", base), None);
    }

    #[test]
    fn reads_text_rejects_binary_and_loads_deleted_from_head() {
        let dir = std::env::temp_dir().join(format!(
            "kathamo-file-preview-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .current_dir(&dir)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "Test"]);
        git(&["config", "user.email", "test@example.com"]);
        std::fs::write(dir.join("file.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("binary.bin"), [0, 1, 2]).unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "base"]);
        let live = read(dir.clone(), "file.rs".into()).unwrap();
        assert_eq!(live.text, "fn main() {}\n");
        assert!(!live.deleted);
        assert!(
            read(dir.clone(), "binary.bin".into())
                .err()
                .unwrap()
                .contains("Binary")
        );
        std::fs::remove_file(dir.join("file.rs")).unwrap();
        let deleted = read(dir.clone(), "file.rs".into()).unwrap();
        assert_eq!(deleted.text, "fn main() {}\n");
        assert!(deleted.deleted);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
