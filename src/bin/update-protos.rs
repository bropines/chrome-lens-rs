//! Re-pin the vendored Lens protos to a Chromium commit.
//!
//!     cargo run --bin update-protos -- [--ref main]
//!
//! What it does, in order:
//!
//! 1. Resolves the ref to one commit, so every file comes from the same tree.
//! 2. Lists and downloads `third_party/lens_server_proto/*.proto` from it.
//! 3. Checks that every import of every file being compiled resolves to a file
//!    that is also being compiled (or to a well-known `google/protobuf` proto).
//! 4. Writes them into `third_party/lens_server_proto/`, drops files that no
//!    longer exist upstream, and records the commit in `CHROMIUM_COMMIT`.
//!
//! It exits non-zero on ANY problem. Nothing is skipped quietly: a missing
//! file or an import that cannot be resolved is a failure, because the
//! alternative is a half-updated tree that builds and then disagrees with the
//! live endpoint about field numbers.
//!
//! This mirrors `scripts/update_protos.py` in chrome-lens-py, which generates
//! the Python side from the same directory. The two should be re-pinned
//! together; `CHROMIUM_COMMIT` is what says whether they agree.
//!
//! Set `GITHUB_TOKEN` to avoid the unauthenticated API rate limit.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

const CHROMIUM: &str = "chromium/chromium";
const PROTO_DIR: &str = "third_party/lens_server_proto";

/// Files deliberately not vendored. Each entry carries a reason that is
/// printed on every run, so an exclusion can never go unnoticed.
const EXCLUDED: &[(&str, &str)] = &[(
    "aim_communication.proto",
    "imports tool_mode.proto, model_mode.proto (third_party/omnibox_proto) and \
     actions_data.proto -> common_quality_data.proto (components/optimization_guide), \
     none of which live in this directory; vendoring it would mean vendoring \
     unrelated Chromium protos under their full import paths. The client never uses it.",
)];

/// Imports that protoc resolves from its own bundled include directory.
const WELL_KNOWN_PREFIX: &str = "google/protobuf/";

fn vendor_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(PROTO_DIR)
}

async fn get(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let mut request = client
        .get(url)
        .header(reqwest::header::USER_AGENT, "chrome-lens-rs-update-protos");
    if let Ok(token) = std::env::var("GITHUB_TOKEN")
        && !token.is_empty()
        && url.starts_with("https://api.github.com/")
    {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .await
        .with_context(|| format!("GET {url} failed"))?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .with_context(|| format!("GET {url}: reading the body failed"))?;
    if !status.is_success() {
        bail!(
            "GET {url} returned {status}: {}",
            String::from_utf8_lossy(&body)
                .chars()
                .take(300)
                .collect::<String>()
        );
    }
    Ok(body.to_vec())
}

async fn resolve_commit(client: &reqwest::Client, git_ref: &str) -> Result<String> {
    let url = format!("https://api.github.com/repos/{CHROMIUM}/commits/{git_ref}");
    let body = get(client, &url).await?;
    let value: serde_json::Value = serde_json::from_slice(&body)?;
    value["sha"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("no commit sha in the response for {git_ref}"))
}

async fn list_protos(client: &reqwest::Client, commit: &str) -> Result<Vec<String>> {
    let url = format!("https://api.github.com/repos/{CHROMIUM}/contents/{PROTO_DIR}?ref={commit}");
    let body = get(client, &url).await?;
    let entries: serde_json::Value = serde_json::from_slice(&body)?;
    let entries = entries
        .as_array()
        .ok_or_else(|| anyhow!("{PROTO_DIR} at {commit} did not list as a directory"))?;

    let mut names: Vec<String> = entries
        .iter()
        .filter(|e| e["type"] == "file")
        .filter_map(|e| e["name"].as_str())
        .filter(|name| name.ends_with(".proto"))
        .map(str::to_owned)
        .collect();
    names.sort();

    if names.is_empty() {
        bail!("no .proto files listed under {PROTO_DIR} at {commit}");
    }
    Ok(names)
}

/// Every `import "x";` in a proto file, in source order.
fn imports_of(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    for line in source.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("import") else {
            continue;
        };
        // `import public "x";` and `import weak "x";` are both imports.
        let rest = rest.trim_start();
        let rest = rest
            .strip_prefix("public")
            .or_else(|| rest.strip_prefix("weak"))
            .unwrap_or(rest)
            .trim_start();
        let Some(rest) = rest.strip_prefix('"') else {
            continue;
        };
        if let Some(end) = rest.find('"') {
            found.push(rest[..end].to_owned());
        }
    }
    found
}

fn check_imports(sources: &[(String, String)]) -> Result<()> {
    let available: BTreeSet<&str> = sources.iter().map(|(name, _)| name.as_str()).collect();
    let mut problems = Vec::new();
    for (name, source) in sources {
        for imported in imports_of(source) {
            if imported.starts_with(WELL_KNOWN_PREFIX) || available.contains(imported.as_str()) {
                continue;
            }
            problems.push(format!(
                "{name} imports {imported:?}, which is not being vendored"
            ));
        }
    }
    if !problems.is_empty() {
        bail!(
            "unresolvable imports (vendor the missing file's source too, or exclude \
             the importer - with a reason - in EXCLUDED):\n  {}",
            problems.join("\n  ")
        );
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut git_ref = "main".to_owned();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--ref" => {
                git_ref = args
                    .next()
                    .ok_or_else(|| anyhow!("--ref needs a branch, tag or commit"))?;
            }
            "-h" | "--help" => {
                println!("usage: cargo run --bin update-protos -- [--ref <branch|tag|commit>]");
                return Ok(());
            }
            other => bail!("unexpected argument {other:?} (try --help)"),
        }
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;

    let commit = resolve_commit(&client, &git_ref).await?;
    println!("Chromium {git_ref} = {commit}");

    let listed = list_protos(&client, &commit).await?;
    let excluded: BTreeSet<&str> = EXCLUDED.iter().map(|(name, _)| *name).collect();
    for (name, reason) in EXCLUDED {
        if listed.iter().any(|n| n == name) {
            println!("EXCLUDED {name}: {reason}");
        } else {
            println!("NOTE {name} is in EXCLUDED but upstream no longer has it; drop the entry.");
        }
    }
    let wanted: Vec<String> = listed
        .into_iter()
        .filter(|name| !excluded.contains(name.as_str()))
        .collect();

    let base = format!("https://raw.githubusercontent.com/{CHROMIUM}/{commit}/{PROTO_DIR}");
    let mut sources = Vec::with_capacity(wanted.len());
    for name in &wanted {
        let body = get(&client, &format!("{base}/{name}")).await?;
        let text = String::from_utf8(body).with_context(|| format!("{name} is not valid UTF-8"))?;
        sources.push((name.clone(), text));
    }
    println!("Downloaded {} proto file(s).", sources.len());

    check_imports(&sources)?;

    let dir = vendor_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    // Drop files upstream no longer has, so a renamed proto cannot linger and
    // keep compiling against a message that is gone.
    let keep: BTreeSet<&str> = sources.iter().map(|(name, _)| name.as_str()).collect();
    for entry in std::fs::read_dir(&dir)? {
        let path = entry?.path();
        let is_stale = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| name.ends_with(".proto") && !keep.contains(name));
        if is_stale {
            println!("  removing stale {}", path.display());
            std::fs::remove_file(&path)?;
        }
    }

    for (name, text) in &sources {
        std::fs::write(dir.join(name), text.as_bytes())
            .with_context(|| format!("writing {name}"))?;
    }
    std::fs::write(dir.join("CHROMIUM_COMMIT"), format!("{commit}\n"))?;

    println!(
        "Vendored {} proto file(s) from chromium@{}.",
        sources.len(),
        &commit[..12]
    );
    println!("Now run `cargo build` - build.rs compiles these with a pinned protoc.");
    Ok(())
}
