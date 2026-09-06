//! Git sync guidance names the mounted ref; running its command publishes
//! landed content to a local bare remote while the original clone stays put.
#![expect(
    clippy::too_many_lines,
    reason = "a test tells one story end to end; fragmenting it would hide the transition being pinned"
)]

use std::fs;
use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;

fn atelier(config_home: &Path, workspace: &Path) -> Command {
    let mut command = Command::cargo_bin("atelier").expect("atelier binary builds");
    command
        .env("ATELIER_CONFIG_HOME", config_home)
        .current_dir(workspace);
    command
}

fn git(path: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(path)
        .env("GIT_AUTHOR_NAME", "Atelier Test")
        .env("GIT_AUTHOR_EMAIL", "atelier@example.invalid")
        .env("GIT_COMMITTER_NAME", "Atelier Test")
        .env("GIT_COMMITTER_EMAIL", "atelier@example.invalid")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("run git fixture command");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("git fixture output is utf-8")
}

#[test]
fn sync_guidance_publishes_the_landed_ref_from_the_mount() {
    for (detached, git_ref, refspec) in [
        (
            false,
            "refs/heads/topic/owner'$HOME`id`",
            "refs/heads/topic/owner'\\''$HOME`id`:refs/heads/topic/owner'\\''$HOME`id`",
        ),
        (
            true,
            "refs/heads/atelier",
            "refs/heads/atelier:refs/heads/atelier",
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        let config = root.join("config");
        let workspace = root.join("work space's $HOME");
        let original = root.join("original clone");
        let remote = root.join("remote.git");
        for path in [&config, &workspace, &original, &remote] {
            fs::create_dir(path).unwrap();
        }
        fs::write(
            config.join("config.toml"),
            "[actor]\nname = \"test-actor\"\nkind = \"human\"\n",
        )
        .unwrap();
        git(&remote, &["init", "--bare", "-q"]);
        git(&original, &["init", "-q", "-b", "topic/owner'$HOME`id`"]);
        fs::write(original.join("lib.rs"), "pub fn original() {}\n").unwrap();
        git(&original, &["add", "lib.rs"]);
        git(&original, &["commit", "-qm", "original source"]);
        git(
            &original,
            &["remote", "add", "publish", remote.to_str().unwrap()],
        );
        let original_head = git(&original, &["rev-parse", "HEAD"]);
        if detached {
            git(&original, &["checkout", "--detach", "-q"]);
        }
        atelier(&config, &workspace)
            .arg("init")
            .assert()
            .success()
            .stdout(format!(
                "initialized workspace work space's $HOME at {}\n",
                workspace.display()
            ))
            .stderr("");
        let branch = if detached {
            "detached"
        } else {
            "topic/owner'$HOME`id`"
        };
        atelier(&config, &workspace)
            .arg("attach")
            .arg(&original)
            .args(["--mount", "sdk"])
            .assert()
            .success()
            .stdout(format!(
                "source git: HEAD {}; branch {branch}\nsource git state: tracked modifications: 0; untracked files: 0; estimated untracked bytes: 0\nattached local-git {} at sdk\n",
                original_head.trim(),
                original.display()
            ))
            .stderr("");
        atelier(&config, &workspace)
            .args(["session", "open", "--summary", "publish landed content"])
            .assert()
            .success()
            .stdout(format!(
                "opened session s1\nworking copy {}\nland with: atelier land s1\n",
                workspace.join(".atelier/sessions/s1").display()
            ))
            .stderr("");
        fs::write(
            workspace.join(".atelier/sessions/s1/sdk/lib.rs"),
            "pub fn landed() {}\n",
        )
        .unwrap();
        atelier(&config, &workspace)
            .args(["land", "s1"])
            .assert()
            .success()
            .stdout(
                predicate::str::is_match("^landed [0-9a-f]{40}\\nlanded sdk [0-9a-f]{40}\\n$")
                    .unwrap(),
            )
            .stderr("");
        let repository = workspace.join("sdk");
        assert_eq!(
            git(&repository, &["show", &format!("{git_ref}:lib.rs")]),
            "pub fn landed() {}\n"
        );
        assert_eq!(
            git(&repository, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "HEAD\n"
        );
        let sync = atelier(&config, &workspace)
            .args(["sync", "sdk"])
            .assert()
            .failure()
            .stdout("")
            .stderr(format!(
                "error: config error: \"sdk\" is a git source; the original clone at {original:?} remains unchanged.\n\
                 Landings update {git_ref:?} in the mounted repository at {repository:?}.\n\
                 Publish with:\n  git -C '{}/work space'\\''s $HOME/sdk' push -- '<remote>' '{refspec}'\n\
                 Replace <remote> with a remote name or URL; list configured remotes with:\n  git -C '{}/work space'\\''s $HOME/sdk' remote -v\n",
                root.display(), root.display(),
                original = original.to_str().unwrap(),
                repository = repository.to_str().unwrap()
            ));
        let diagnostic = String::from_utf8(sync.get_output().stderr.clone()).unwrap();
        let command = diagnostic
            .lines()
            .nth(3)
            .unwrap()
            .replace("'<remote>'", "'publish'");
        let published = std::process::Command::new("sh")
            .args(["-euc", &command])
            .current_dir(&original)
            .output()
            .unwrap();
        assert!(published.status.success(), "{published:?}");
        assert_eq!(
            git(&remote, &["show", &format!("{git_ref}:lib.rs")]),
            "pub fn landed() {}\n"
        );
        assert_eq!(git(&original, &["rev-parse", "HEAD"]), original_head);
        assert_eq!(
            fs::read_to_string(original.join("lib.rs")).unwrap(),
            "pub fn original() {}\n"
        );
        assert_eq!(git(&original, &["status", "--porcelain"]), "");
    }
}
