use crate::error::HoganError;
use anyhow::{Context, Result};
use git2::build::RepoBuilder;
use git2::{AutotagOption, Cred, FetchOptions, Reference, RemoteCallbacks, Repository, ResetType};
use std::path::Path;
use std::process::Command;
use std::str;
use url::Url;

pub fn ext_clone(url: &Url, path: &Path) -> Result<()> {
    info!("Cloning {:?} to {:?}", url, path);
    let mut clone = Command::new("git")
        .args(&["clone", &url.to_string(), path.to_str().unwrap()])
        .spawn()?;
    let result = clone.wait()?;
    info!("Clone output {}", result);
    Ok(())
}

pub fn clone(
    url: &Url,
    branch: Option<&str>,
    path: &Path,
    ssh_key_path: Option<&Path>,
) -> Result<Repository> {
    let mut callbacks = RemoteCallbacks::new();

    if let Some(password) = url.password() {
        debug!("Using password auth");
        callbacks.credentials(move |_url, username_from_url, _allowed_types| {
            Cred::userpass_plaintext(username_from_url.unwrap(), password)
        });
    } else if let Some(ssh_key_path) = ssh_key_path {
        debug!("Using SSH auth");
        callbacks.credentials(move |_url, username_from_url, _allowed_types| {
            Cred::ssh_key(username_from_url.unwrap(), None, ssh_key_path, None)
        });
    }

    callbacks.transfer_progress(|stats| {
        if stats.received_objects() == stats.total_objects() {
            let step = stats.total_objects() / 10;
            if step == 0
                || stats.indexed_objects() % step == 0
                || stats.total_objects() == stats.indexed_objects()
            {
                info!(
                    "Resolving deltas {}/{}",
                    stats.indexed_deltas(),
                    stats.total_deltas()
                );
            }
        } else if stats.total_objects() > 0 {
            let step = stats.total_objects() / 10;
            if step == 0
                || stats.received_objects() % step == 0
                || stats.total_objects() == stats.received_objects()
            {
                info!(
                    "Received {}/{} objects ({}) in {} bytes",
                    stats.received_objects(),
                    stats.total_objects(),
                    stats.indexed_objects(),
                    stats.received_bytes()
                );
            }
        }
        true
    });

    let mut fetch_options = FetchOptions::new();
    fetch_options.remote_callbacks(callbacks);

    let mut repo_builder = RepoBuilder::new();
    repo_builder.fetch_options(fetch_options);

    if let Some(branch) = branch {
        debug!("Setting branch to {}", branch);
        repo_builder.branch(branch);
    }

    info!("Cloning to {:?}", path);
    repo_builder
        .clone(url.as_str(), path)
        .map_err::<HoganError, _>(|e| e.into())
        .context(format!(
            "Error performing initial repository clone {}",
            url.as_str()
        ))
}

fn make_ssh_auth(ssh_key_path: &Path) -> RemoteCallbacks {
    let mut callback = RemoteCallbacks::new();
    callback.credentials(move |_url, username_from_url, _allowed_types| {
        Cred::ssh_key(username_from_url.unwrap(), None, ssh_key_path, None)
    });

    callback
}

fn make_password_auth(url: &Url) -> RemoteCallbacks {
    if let Some(password) = url.password() {
        let mut callback = RemoteCallbacks::new();
        callback.credentials(move |_url, username_from_url, _allowed_type| {
            Cred::userpass_plaintext(username_from_url.unwrap(), password)
        });
        callback
    } else {
        RemoteCallbacks::new()
    }
}

fn detach_head(repo: &Repository, sha: &str) -> Result<()> {
    let revspec = repo
        .revparse_single(sha)
        .map_err(|_| HoganError::UnknownSHA {
            sha: sha.to_owned(),
        })?;

    debug!("Found revision {}. Switching repo head.", sha);
    repo.reset(&revspec, ResetType::Hard, None)
        .map_err::<HoganError, _>(|e| e.into())
        .context(format!("Error detaching head to SHA {}", sha))
}

pub fn ext_fetch(path: &Path, remote: &str) -> Result<()> {
    info!("Fetching {}", remote);
    let mut fetch_cmd = Command::new("git")
        .current_dir(path.to_str().unwrap())
        .args(&["fetch", remote])
        .spawn()?;

    fetch_cmd.wait()?;
    Ok(())
}

pub fn ext_maintenance(path: &Path) -> Result<()> {
    info!("Performing maintenance");
    let mut maintenance_cmd = Command::new("git")
        .current_dir(path.to_str().unwrap())
        .args(&["maintenance", "run", "--auto"])
        .spawn()?;
    maintenance_cmd.wait()?;
    Ok(())
}

pub fn fetch(
    repo: &Repository,
    remote: &str,
    ssh_key_path: Option<&Path>,
    url: Option<&Url>,
) -> Result<()> {
    let mut cb = if let Some(s) = ssh_key_path {
        make_ssh_auth(s)
    } else if let Some(u) = url {
        make_password_auth(u)
    } else {
        RemoteCallbacks::default()
    };
    let mut remote = repo.find_remote(remote).or_else(|_| {
        repo.remote_anonymous(remote)
            .map_err::<HoganError, _>(|e| e.into())
            .context(format!("Unable to generate remote {}", remote))
    })?;
    cb.sideband_progress(|data| {
        debug!("Fetch: remote: {}", str::from_utf8(data).unwrap());
        true
    });

    cb.transfer_progress(|stats| {
        if stats.received_objects() == stats.total_objects() {
            let step = stats.total_objects() / 10;
            if step == 0
                || stats.indexed_objects() % step == 0
                || stats.indexed_objects() == stats.total_objects()
            {
                debug!(
                    "Fetch: Resolving deltas {}/{}",
                    stats.indexed_deltas(),
                    stats.total_deltas()
                );
            }
        } else if stats.total_objects() > 0 {
            let step = stats.total_objects() / 10;
            if step == 0
                || stats.received_objects() % step == 0
                || stats.received_objects() == stats.total_objects()
            {
                debug!(
                    "Fetch: Received {}/{} objects ({}) in {} bytes",
                    stats.received_objects(),
                    stats.total_objects(),
                    stats.indexed_objects(),
                    stats.received_bytes()
                );
            }
        }
        true
    });

    let mut fo = FetchOptions::new();
    fo.remote_callbacks(cb);
    remote
        .download(&Vec::<String>::new(), Some(&mut fo))
        .map_err::<HoganError, _>(|e| e.into())
        .context("Error fetching remote update")?;

    remote
        .disconnect()
        .map_err::<HoganError, _>(|e| e.into())
        .context("Error disconnecting from remote")?;

    remote
        .update_tips(None, true, AutotagOption::Unspecified, None)
        .map_err::<HoganError, _>(|e| e.into())
        .context("Error updating tips of git repository")?;

    Ok(())
}

pub fn reset(
    repo: &Repository,
    remote: &str,
    ssh_key_path: Option<&Path>,
    url: Option<&Url>,
    sha: Option<&str>,
    force_refresh: bool,
    allow_fetch: bool,
) -> Result<String> {
    if force_refresh && allow_fetch {
        fetch(repo, remote, ssh_key_path, url)?;
    };

    if let Some(sha) = sha {
        match detach_head(repo, sha) {
            Ok(_) => {}
            Err(_) => {
                if allow_fetch {
                    info!("Couldn't find {}. Trying to refreshing repo", sha);
                    fetch(repo, remote, ssh_key_path, url)?;
                    match detach_head(repo, sha) {
                        Ok(_) => {}
                        Err(e) => {
                            warn!("Unable to find ref {}: {:?}", sha, e);
                            return Err(e);
                        }
                    }
                } else {
                    return Err(HoganError::UnknownSHA {
                        sha: sha.to_string(),
                    })
                    .context("Unknown SHA when checking out, may resolve next update");
                }
            }
        }
    }

    get_head_sha(repo)
}

pub fn build_repo(path: &str) -> Result<Repository> {
    Repository::discover(path).map_err(|e| e.into())
}

fn find_ref_sha(reference: &Reference) -> Result<String> {
    if let Some(target) = reference.target() {
        let sha = target.to_string();
        Ok(sha[..7].to_string())
    } else {
        Err(HoganError::GitError {
            msg: "Unable to convert ref to SHA".to_string(),
        }
        .into())
    }
}

pub fn get_head_sha(repo: &Repository) -> Result<String> {
    let head = repo
        .head()
        .map_err::<HoganError, _>(|e| e.into())
        .context("Error finding head reference for repository")?;
    find_ref_sha(&head).context("Unable to find the head SHA")
}

pub fn find_branch_head(repo: &Repository, branch: &str) -> Result<String> {
    let branch_ref = repo
        .resolve_reference_from_short_name(branch)
        .map_err(|_| HoganError::UnknownBranch {
            branch: branch.to_owned(),
        })
        .context(format!("Unable to find branch {}", branch))?;
    find_ref_sha(&branch_ref).context(format!("Unable to find the head SHA of branch {}", branch))
}
