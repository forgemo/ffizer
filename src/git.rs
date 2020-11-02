use crate::Error;
use git2::build::{CheckoutBuilder, RepoBuilder};
use git2::{Config, FetchOptions, Repository};
use git2_credentials;
use slog::{debug, info, warn, Logger};
use snafu::ResultExt;
use std::path::Path;

/// clone a repository at a rev to a directory
// TODO id the directory is already present then fetch and rebase (if not in offline mode)
pub fn retrieve<P, U, R>(logger: &Logger, dst: P, url: U, rev: R) -> Result<(), Error>
where
    P: AsRef<Path>,
    R: AsRef<str>,
    U: AsRef<str>,
{
    let dst = dst.as_ref();
    let mut fo = make_fetch_options().context(crate::GitRetrieve {
        dst: dst.to_path_buf(),
        url: url.as_ref().to_owned(),
        rev: rev.as_ref().to_owned(),
    })?;
    if dst.exists() {
        info!(logger, "git reset cached template"; "folder" => ?&dst);
        checkout(dst, &rev).context(crate::GitRetrieve {
            dst: dst.to_path_buf(),
            url: url.as_ref().to_owned(),
            rev: rev.as_ref().to_owned(),
        })?;
        info!(logger, "git pull cached template"; "folder" => ?&dst);
        pull(logger, dst, &rev, &mut fo).context(crate::GitRetrieve {
            dst: dst.to_path_buf(),
            url: url.as_ref().to_owned(),
            rev: rev.as_ref().to_owned(),
        })?;
    //until pull is fixed and work as expected
    // let mut tmp = dst.to_path_buf().clone();
    // tmp.set_extension("part");
    // if tmp.exists() {
    //     std::fs::remove_dir_all(&tmp)?;
    // }
    // clone(&tmp, url, "master", fo)?;
    // checkout(&tmp, rev)?;
    // std::fs::remove_dir_all(&dst)?;
    // std::fs::rename(&tmp, &dst)?;
    } else {
        info!(logger, "git clone into cached template"; "folder" => ?&dst);
        clone(&dst, &url, "master", fo)?;
        checkout(&dst, &rev).context(crate::GitRetrieve {
            dst: dst.to_path_buf(),
            url: url.as_ref().to_owned(),
            rev: rev.as_ref().to_owned(),
        })?;
    }
    Ok(())
}

/// a best attempt effort is made to authenticate
/// requests when required to support private
/// git repositories
fn make_fetch_options<'a>() -> Result<FetchOptions<'a>, git2::Error> {
    let mut cb = git2::RemoteCallbacks::new();
    let git_config = git2::Config::open_default()?;
    let mut ch = git2_credentials::CredentialHandler::new(git_config);
    cb.credentials(move |url, username, allowed| ch.try_next_credential(url, username, allowed));

    let mut fo = FetchOptions::new();
    let mut proxy_options = git2::ProxyOptions::new();
    proxy_options.auto();
    fo.proxy_options(proxy_options)
        .remote_callbacks(cb)
        .download_tags(git2::AutotagOption::All)
        .update_fetchhead(true);
    Ok(fo)
}

fn clone<P, U, R>(dst: P, url: U, rev: R, fo: FetchOptions<'_>) -> Result<(), Error>
where
    P: AsRef<Path>,
    R: AsRef<str>,
    U: AsRef<str>,
{
    std::fs::create_dir_all(&dst.as_ref()).context(crate::CreateFolder {
        path: dst.as_ref().to_path_buf(),
    })?;
    RepoBuilder::new()
        .branch(rev.as_ref())
        .fetch_options(fo)
        .clone(url.as_ref(), dst.as_ref())
        .context(crate::GitRetrieve {
            dst: dst.as_ref().to_path_buf(),
            url: url.as_ref().to_owned(),
            rev: rev.as_ref().to_owned(),
        })?;
    Ok(())
}

// from https://github.com/rust-lang/git2-rs/blob/master/examples/pull.rs
fn pull<'a, P, R>(
    logger: &Logger,
    dst: P,
    rev: R,
    fo: &mut FetchOptions<'a>,
) -> Result<(), git2::Error>
where
    P: AsRef<Path>,
    R: AsRef<str>,
{
    let repository = Repository::discover(dst.as_ref())?;

    // fetch
    let revref = rev.as_ref();
    let mut remote = repository.find_remote("origin")?;
    remote.fetch(&[revref], Some(fo), None)?;
    let reference = repository.find_reference("FETCH_HEAD")?;
    let fetch_head_commit = repository.reference_to_annotated_commit(&reference)?;
    do_merge(logger, &repository, "master", fetch_head_commit)?;
    Ok(())
}

// from https://github.com/rust-lang/git2-rs/blob/master/examples/pull.rs
fn fast_forward(
    logger: &Logger,
    repo: &Repository,
    lb: &mut git2::Reference,
    rc: &git2::AnnotatedCommit,
) -> Result<(), git2::Error> {
    let name = match lb.name() {
        Some(s) => s.to_string(),
        None => String::from_utf8_lossy(lb.name_bytes()).to_string(),
    };
    let msg = format!("Fast-Forward: Setting {} to id: {}", name, rc.id());
    debug!(logger, "{}", msg);
    lb.set_target(rc.id(), &msg)?;
    repo.set_head(&name)?;
    repo.checkout_head(Some(
        git2::build::CheckoutBuilder::default()
            // For some reason the force is required to make the working directory actually get updated
            // I suspect we should be adding some logic to handle dirty working directory states
            // but this is just an example so maybe not.
            .force(),
    ))?;
    Ok(())
}

// from https://github.com/rust-lang/git2-rs/blob/master/examples/pull.rs
fn normal_merge(
    logger: &Logger,
    repo: &Repository,
    local: &git2::AnnotatedCommit,
    remote: &git2::AnnotatedCommit,
) -> Result<(), git2::Error> {
    let local_tree = repo.find_commit(local.id())?.tree()?;
    let remote_tree = repo.find_commit(remote.id())?.tree()?;
    let ancestor = repo
        .find_commit(repo.merge_base(local.id(), remote.id())?)?
        .tree()?;
    let mut idx = repo.merge_trees(&ancestor, &local_tree, &remote_tree, None)?;

    if idx.has_conflicts() {
        warn!(logger, "merge conficts detected...");
        repo.checkout_index(Some(&mut idx), None)?;
        return Ok(());
    }
    let result_tree = repo.find_tree(idx.write_tree_to(repo)?)?;
    // now create the merge commit
    let msg = format!("Merge: {} into {}", remote.id(), local.id());
    let sig = repo.signature()?;
    let local_commit = repo.find_commit(local.id())?;
    let remote_commit = repo.find_commit(remote.id())?;
    // Do our merge commit and set current branch head to that commit.
    let _merge_commit = repo.commit(
        Some("HEAD"),
        &sig,
        &sig,
        &msg,
        &result_tree,
        &[&local_commit, &remote_commit],
    )?;
    // Set working tree to match head.
    repo.checkout_head(None)?;
    Ok(())
}

// from https://github.com/rust-lang/git2-rs/blob/master/examples/pull.rs
fn do_merge<'a>(
    logger: &Logger,
    repo: &'a Repository,
    remote_branch: &str,
    fetch_commit: git2::AnnotatedCommit<'a>,
) -> Result<(), git2::Error> {
    // 1. do a merge analysis
    let analysis = repo.merge_analysis(&[&fetch_commit])?;

    // 2. Do the appopriate merge
    if analysis.0.is_fast_forward() {
        debug!(logger, "git merge: doing a fast forward"; "analysis" => ?&analysis.0);
        // do a fast forward
        let refname = format!("refs/heads/{}", remote_branch);
        match repo.find_reference(&refname) {
            Ok(mut r) => {
                fast_forward(logger, repo, &mut r, &fetch_commit)?;
            }
            Err(_) => {
                // The branch doesn't exist so just set the reference to the
                // commit directly. Usually this is because you are pulling
                // into an empty repository.
                repo.reference(
                    &refname,
                    fetch_commit.id(),
                    true,
                    &format!("Setting {} to {}", remote_branch, fetch_commit.id()),
                )?;
                repo.set_head(&refname)?;
                repo.checkout_head(Some(
                    git2::build::CheckoutBuilder::default()
                        .allow_conflicts(true)
                        .conflict_style_merge(true)
                        .force(),
                ))?;
            }
        };
    } else if analysis.0.is_normal() {
        debug!(logger, "git merge: doing normal merge"; "analysis" => ?&analysis.0);
        // do a normal merge
        let head_commit = repo.reference_to_annotated_commit(&repo.head()?)?;
        normal_merge(logger, &repo, &head_commit, &fetch_commit)?;
    } else {
        debug!(logger, "git merge: nothing to do"; "analysis" => ?&analysis.0);
    }
    Ok(())
}

fn checkout<P, R>(dst: P, rev: R) -> Result<(), git2::Error>
where
    P: AsRef<Path>,
    R: AsRef<str>,
{
    let rev = rev.as_ref();
    let repository = Repository::discover(dst.as_ref())?;
    let mut co = CheckoutBuilder::new();
    co.force().remove_ignored(true).remove_untracked(true);
    let treeish = repository.revparse_single(rev)?;
    repository.checkout_tree(&treeish, Some(&mut co))?;
    Ok(())
}

/// kind can be "merge" or "diff"
pub fn find_cmd_tool(kind: &str) -> Result<String, git2::Error> {
    let config = Config::open_default()?;
    let tool = config.get_string(&format!("{}.tool", kind))?;
    config.get_string(&format!("{}tool.{}.cmd", kind, tool))
}

#[cfg(test)]
mod tests {
    use super::*;
    use run_script;
    use std::fs;
    use tempfile::tempdir;

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn retrieve_should_update_existing_template() -> Result<(), Box<dyn std::error::Error>> {
        let logger = slog::Logger::root(slog::Discard, slog::o!());
        if std::process::Command::new("git")
            .arg("version")
            .output()
            .is_err()
        {
            eprintln!("skip the test because `git` is not installed");
            return Ok(());
        }

        let tmp_dir = tempdir()?;

        // template v1
        let src_path = tmp_dir.path().join("src");
        let options = run_script::ScriptOptions::new();
        let args = vec![];
        let (code, output, error) = run_script::run(
            &format!(
                r#"
            mkdir -p {}
            cd {}
            git init
            git config user.email "test@example.com"
            git config user.name "Test Name"
            echo "v1: Lorem ipsum" > foo.txt
            git add foo.txt
            git commit -m "add foo.txt"
            "#,
                src_path.to_str().unwrap(),
                src_path.to_str().unwrap()
            ),
            &args,
            &options,
        )?;
        if code != 0 {
            eprintln!("---output:\n{}\n---error:\n{}\n---", output, error);
        }
        assert_eq!(code, 0);

        let dst_path = tmp_dir.path().join("dst");
        retrieve(&logger, &dst_path, src_path.to_str().unwrap(), "master")?;
        assert_eq!(
            fs::read_to_string(&dst_path.join("foo.txt"))?,
            "v1: Lorem ipsum\n"
        );

        // template v2
        let (code, output, error) = run_script::run(
            &format!(
                r#"
            cd {}
            echo "v2: Hello" > foo.txt
            git add foo.txt
            git commit -m "add foo.txt"
            "#,
                src_path.to_str().unwrap()
            ),
            &args,
            &options,
        )?;
        if code != 0 {
            eprintln!("---output:\n{}\n---error:\n{}\n---", output, error);
        }
        assert_eq!(code, 0);

        retrieve(&logger, &dst_path, src_path.to_str().unwrap(), "master")?;
        assert_eq!(
            fs::read_to_string(&dst_path.join("foo.txt"))?,
            "v2: Hello\n"
        );

        // template v3
        let (code, output, error) = run_script::run(
            &format!(
                r#"
            cd {}
            echo "v3: Hourra" > foo.txt
            git add foo.txt
            git commit -m "add foo.txt"
            "#,
                src_path.to_str().unwrap()
            ),
            &args,
            &options,
        )?;
        if code != 0 {
            eprintln!("---output:\n{}\n---error:\n{}\n---", output, error);
        }
        assert_eq!(code, 0);

        retrieve(&logger, &dst_path, src_path.to_str().unwrap(), "master")?;
        assert_eq!(
            fs::read_to_string(&dst_path.join("foo.txt"))?,
            "v3: Hourra\n"
        );
        //TODO always remove
        fs::remove_dir_all(tmp_dir)?;
        Ok(())
    }
}
