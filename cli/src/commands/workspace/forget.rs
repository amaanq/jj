// Copyright 2020 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#[cfg(feature = "git")]
use std::path::Path;
#[cfg(feature = "git")]
use std::path::PathBuf;
#[cfg(feature = "git")]
use std::process::Command;

use clap_complete::ArgValueCandidates;
use itertools::Itertools as _;
#[cfg(feature = "git")]
use jj_lib::git;
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::WorkspaceNameBuf;
#[cfg(feature = "git")]
use jj_lib::repo::Repo as _;
use jj_lib::workspace_store::SimpleWorkspaceStore;
use jj_lib::workspace_store::WorkspaceStore as _;
use tracing::instrument;

use crate::cli_util::CommandHelper;
use crate::command_error::CommandError;
use crate::complete;
use crate::ui::Ui;

/// Stop tracking a workspace's working-copy commit in the repo
///
/// The workspace directory is not touched on disk. It can be deleted from disk
/// before or after running this command.
///
/// For colocated workspaces, use --cleanup to also remove the associated Git
/// worktree.
#[derive(clap::Args, Clone, Debug)]
pub struct WorkspaceForgetArgs {
    /// Names of the workspaces to forget. By default, forgets only the current
    /// workspace.
    #[arg(add = ArgValueCandidates::new(complete::workspaces))]
    workspaces: Vec<WorkspaceNameBuf>,

    /// Also remove the Git worktree for colocated workspaces
    ///
    /// This runs `git worktree remove` to clean up the Git worktree directory.
    /// By default, removal will fail if the worktree has uncommitted changes.
    /// Use --force together with --cleanup to remove it anyway.
    #[cfg(feature = "git")]
    #[arg(long)]
    cleanup: bool,

    /// Force removal of Git worktrees even if they have uncommitted changes
    ///
    /// Only has effect when used with --cleanup.
    #[cfg(feature = "git")]
    #[arg(long, requires = "cleanup")]
    force: bool,
}

#[instrument(skip_all)]
pub async fn cmd_workspace_forget(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &WorkspaceForgetArgs,
) -> Result<(), CommandError> {
    let mut workspace_command = command.workspace_helper(ui).await?;

    let wss = if args.workspaces.is_empty() {
        vec![workspace_command.workspace_name().to_owned()]
    } else {
        args.workspaces.clone()
    };

    let mut forget_ws = Vec::new();
    for ws in &wss {
        if workspace_command
            .repo()
            .view()
            .get_wc_commit_id(ws)
            .is_none()
        {
            writeln!(
                ui.warning_default(),
                "No such workspace: {}",
                ws.as_symbol(),
            )?;
        } else {
            forget_ws.push(ws);
        }
    }
    if forget_ws.is_empty() {
        writeln!(ui.status(), "Nothing changed.")?;
        return Ok(());
    }

    let workspace_store = SimpleWorkspaceStore::load(workspace_command.repo_path())?;

    // Collect worktrees to remove BEFORE committing the transaction, while
    // the workspace store still has the forgotten workspaces' paths.
    #[cfg(feature = "git")]
    let worktrees_to_remove = if args.cleanup {
        if let Ok(git_backend) = git::get_git_backend(workspace_command.repo().store()) {
            let git_repo = git_backend.git_repo();
            let common_dir = git_repo.common_dir().to_path_buf();
            let mut worktrees = Vec::new();
            for ws in &forget_ws {
                let Ok(Some(workspace_path)) = workspace_store.get_workspace_path(ws) else {
                    continue;
                };
                // Stored paths are relative to the repo path.
                let workspace_path = workspace_command.repo_path().join(workspace_path);
                let Ok(workspace_path) = dunce::canonicalize(&workspace_path) else {
                    continue;
                };
                if let Some(worktree_path) = colocated_worktree_path(&common_dir, &workspace_path) {
                    worktrees.push((*ws, worktree_path));
                }
            }
            let git_executable =
                git::GitSettings::from_settings(workspace_command.settings())?.executable_path;
            Some((git_executable, common_dir, worktrees, args.force))
        } else {
            None
        }
    } else {
        None
    };

    // bundle every workspace forget into a single transaction, so that e.g.
    // undo correctly restores all of them at once.
    let mut tx = workspace_command.start_transaction();

    for ws in &forget_ws {
        tx.repo_mut().remove_wc_commit(ws).await?;
        // Drop the stored per-workspace Git HEAD so a future workspace with
        // the same name doesn't inherit it.
        tx.repo_mut()
            .set_workspace_git_head(ws, RefTarget::absent());
    }

    workspace_store.forget(&forget_ws.iter().map(|x| x.as_ref()).collect::<Vec<_>>())?;

    let description = if let [ws] = forget_ws.as_slice() {
        format!("forget workspace {}", ws.as_symbol())
    } else {
        format!(
            "forget workspaces {}",
            forget_ws.iter().map(|ws| ws.as_symbol()).join(", ")
        )
    };

    tx.finish(ui, description).await?;

    // Clean up git worktrees AFTER the transaction commits successfully.
    // This ensures that if the transaction fails, the worktrees remain intact.
    // TODO: Use gix API when worktree removal is implemented.
    // See: https://github.com/Byron/gitoxide/blob/main/crate-status.md
    #[cfg(feature = "git")]
    if let Some((git_executable, common_dir, worktrees, force)) = worktrees_to_remove {
        for (ws, worktree_path) in worktrees {
            let mut cmd = Command::new(&git_executable);
            cmd.arg("-C").arg(&common_dir).arg("worktree").arg("remove");
            if force {
                cmd.arg("--force");
            }
            cmd.arg(&worktree_path);
            // Disable translation so we can parse output
            cmd.env("LC_ALL", "C");
            let result = cmd.output();

            match result {
                Ok(output) if !output.status.success() => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    // Check if worktree is already gone
                    if stderr.contains("is not a working tree") {
                        continue;
                    }
                    // Check if it's a dirty worktree error (only happens without --force)
                    if !force
                        && (stderr.contains("contains modified or untracked files")
                            || stderr.contains("is dirty"))
                    {
                        writeln!(
                            ui.warning_default(),
                            "Git worktree for workspace {} has uncommitted changes and was not \
                             removed.",
                            ws.as_symbol(),
                        )?;
                        writeln!(
                            ui.hint_default(),
                            "Use --cleanup --force to remove it anyway, or manually clean up with \
                             `git worktree remove --force {}`",
                            worktree_path.display()
                        )?;
                    } else {
                        writeln!(
                            ui.warning_default(),
                            "Failed to remove Git worktree for workspace {}: {}",
                            ws.as_symbol(),
                            stderr.trim()
                        )?;
                    }
                }
                Err(e) => {
                    writeln!(
                        ui.warning_default(),
                        "Failed to run git worktree remove for workspace {}: {}",
                        ws.as_symbol(),
                        e
                    )?;
                }
                Ok(_) => {
                    // Success - worktree was removed
                }
            }
        }
    }

    Ok(())
}

/// Returns the workspace path if it is a Git worktree of the repo at
/// `common_dir`.
///
/// The workspace's `.git` must be a gitlink file whose gitdir resolves under
/// `<common_dir>/worktrees/`; anything else (no `.git`, a `.git` directory, a
/// worktree of some other repo) is not ours to remove.
#[cfg(feature = "git")]
fn colocated_worktree_path(common_dir: &Path, workspace_path: &Path) -> Option<PathBuf> {
    let dot_git = workspace_path.join(".git");
    // Reading a .git directory fails, filtering out non-worktree colocation.
    let content = std::fs::read_to_string(&dot_git).ok()?;
    let gitdir = Path::new(content.strip_prefix("gitdir:")?.trim());
    let gitdir = if gitdir.is_absolute() {
        gitdir.to_path_buf()
    } else {
        workspace_path.join(gitdir)
    };
    let gitdir = dunce::canonicalize(gitdir).ok()?;
    let worktrees_dir = dunce::canonicalize(common_dir.join("worktrees")).ok()?;
    gitdir
        .starts_with(&worktrees_dir)
        .then(|| workspace_path.to_path_buf())
}
