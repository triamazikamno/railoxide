use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const BUILD_GIT_SHORT_HASH: &str = "RAILOXIDE_BUILD_GIT_SHORT_HASH";
const EXTENSION_BUNDLE: &str = "RAILOXIDE_EXTENSION_BUNDLE";

fn main() {
    println!("cargo::rerun-if-env-changed={BUILD_GIT_SHORT_HASH}");
    track_git_refs();

    let short_hash = injected_git_short_hash()
        .or_else(git_short_hash)
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo::rustc-env=RAILOXIDE_GIT_SHORT_HASH={short_hash}");

    embed_extension_bundle();
}

/// Embeds the packaged browser extension so the gateway can serve it.
fn embed_extension_bundle() {
    println!("cargo::rerun-if-env-changed={EXTENSION_BUNDLE}");
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set"));
    let bundle = env::var_os(EXTENSION_BUNDLE).map_or_else(
        || manifest_dir.join("../../target/browser-extension.zip"),
        PathBuf::from,
    );
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set"));
    let embedded = out_dir.join("browser-extension.zip");
    let copied = if bundle.is_file() {
        // Registering a missing path would rerun this script, and rebuild the crate, every time.
        println!("cargo::rerun-if-changed={}", bundle.display());
        fs::copy(&bundle, &embedded).map(|_| ())
    } else {
        fs::write(&embedded, [])
    };
    copied.unwrap_or_else(|error| panic!("failed to stage {}: {error}", embedded.display()));

    let manifest = manifest_dir.join("../../extensions/railoxide/manifest.json");
    println!("cargo::rerun-if-changed={}", manifest.display());
    let version = extension_version(&manifest);
    println!("cargo::rustc-env=RAILOXIDE_EXTENSION_VERSION={version}");
}

fn extension_version(manifest: &Path) -> String {
    let text = fs::read_to_string(manifest)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", manifest.display()));
    let version = text
        .split_once("\"version\": \"")
        .and_then(|(_, rest)| rest.split_once('"'))
        .map_or_else(
            || panic!("{} must declare \"version\"", manifest.display()),
            |(version, _)| version,
        );

    assert!(
        !version.is_empty()
            && version
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b'.'),
        "{} declares an unsupported version {version}",
        manifest.display()
    );
    version.to_owned()
}

fn injected_git_short_hash() -> Option<String> {
    let value = env::var_os(BUILD_GIT_SHORT_HASH)?;
    let mut hash = value
        .into_string()
        .unwrap_or_else(|_| panic!("{BUILD_GIT_SHORT_HASH} must be valid UTF-8"));

    assert!(
        hash.len() == 7 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "{BUILD_GIT_SHORT_HASH} must contain exactly seven hexadecimal characters"
    );
    hash.make_ascii_lowercase();
    Some(hash)
}

fn track_git_refs() {
    let Some(manifest_dir) = env::var_os("CARGO_MANIFEST_DIR") else {
        return;
    };
    let git_path = PathBuf::from(manifest_dir).join("../../.git");

    if git_path.is_file() {
        println!("cargo::rerun-if-changed={}", git_path.display());
    }

    let Some(git_dir) = resolve_git_dir(&git_path) else {
        return;
    };

    let head_path = git_dir.join("HEAD");
    println!("cargo::rerun-if-changed={}", head_path.display());

    let Ok(head) = fs::read_to_string(&head_path) else {
        return;
    };

    let Some(ref_name) = active_ref(&head) else {
        return;
    };

    let common_dir_path = git_dir.join("commondir");
    let common_dir = fs::read_to_string(&common_dir_path).map_or_else(
        |_| git_dir.clone(),
        |common_dir| {
            println!("cargo::rerun-if-changed={}", common_dir_path.display());
            resolve_relative_path(&git_dir, common_dir.trim())
        },
    );

    let ref_path = common_dir.join(ref_name);
    println!("cargo::rerun-if-changed={}", ref_path.display());

    if !ref_path.exists() {
        println!(
            "cargo::rerun-if-changed={}",
            common_dir.join("packed-refs").display()
        );
    }
}

fn resolve_git_dir(git_path: &Path) -> Option<PathBuf> {
    if git_path.is_dir() {
        return Some(git_path.to_path_buf());
    }

    let git_file = fs::read_to_string(git_path).ok()?;
    let git_dir = git_file.trim().strip_prefix("gitdir:")?.trim();
    if git_dir.is_empty() {
        return None;
    }

    Some(resolve_relative_path(git_path.parent()?, git_dir))
}

fn resolve_relative_path(base: &Path, path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

fn active_ref(head: &str) -> Option<&str> {
    head.trim()
        .strip_prefix("ref:")
        .map(str::trim)
        .filter(|ref_name| !ref_name.is_empty())
}

fn git_short_hash() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let hash = String::from_utf8(output.stdout).ok()?;
    let hash = hash.trim();
    (!hash.is_empty()).then(|| hash.chars().take(7).collect())
}
