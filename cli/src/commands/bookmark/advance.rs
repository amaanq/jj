// Copyright 2026 The Jujutsu Authors
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

use clap_complete::ArgValueCandidates;
use clap_complete::ArgValueCompleter;

use super::BookmarkMoveArgs;
use super::cmd_bookmark_move;
use crate::cli_util::CommandHelper;
use crate::cli_util::RevisionArg;
use crate::command_error::CommandError;
use crate::command_error::user_error;
use crate::complete;
use crate::ui::Ui;

/// Advance the closest bookmarks to a target revision
///
/// The target `--to` defaults to `revsets.advance-default-to` (which defaults
/// to `@`).
///
/// The bookmarks advanced is determined by revset alias `closest_bookmarks(to)`
/// where `to` is the target revision.
///
/// Positional bookmark name arguments can target specific bookmarks to advance
/// to the target.
///
/// Example:
///
/// `jj bookmark advance --to x` - Does the equivalent of
/// `jj bookmark move --from 'closest_bookmarks(x)' --to x`.
#[derive(clap::Args, Clone, Debug)]
pub struct BookmarkAdvanceArgs {
    /// Move bookmarks matching the given name patterns
    ///
    /// By default, the specified pattern matches bookmark names with glob
    /// syntax. You can also use other [string pattern syntax].
    ///
    /// [string pattern syntax]:
    ///     https://docs.jj-vcs.dev/latest/revsets/#string-patterns
    #[arg(add = ArgValueCandidates::new(complete::local_bookmarks))]
    names: Option<Vec<String>>,

    /// Move bookmarks to this revision, defaults to
    /// `revsets.advance-default-to`.
    #[arg(long, short, value_name = "REVSET")]
    #[arg(add = ArgValueCompleter::new(complete::revset_expression_all))]
    to: Option<RevisionArg>,

    /// Allow moving bookmarks backwards or sideways
    #[arg(long, short = 'B')]
    allow_backwards: bool,
}

pub fn cmd_bookmark_advance(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &BookmarkAdvanceArgs,
) -> Result<(), CommandError> {
    let workspace_command = command.workspace_helper(ui)?;

    let to = if let Some(to) = &args.to {
        to.clone()
    } else {
        RevisionArg::from(
            workspace_command
                .settings()
                .get_string("revsets.advance-default-to")?,
        )
    };

    // Validate the target revision and provide a better error message for the
    // default.
    workspace_command
        .resolve_single_rev(ui, &to)
        .map_err(|_err| {
            // Provide a better error message when the default target doesn't
            // resolve.
            let error = user_error("No suitable revision to advance to.");
            if args.to.is_none() {
                error.hinted(
                    "`revsets.advance-default-to` controls the default target. You can also \
                     specify a specific target with `--to`.",
                )
            } else {
                error
            }
        })?;

    let from = if args.names.is_none() {
        // TODO(algmyr): Is there a better way to construct this?
        vec![RevisionArg::from(format!("closest_bookmarks({to})"))]
    } else {
        vec![]
    };

    // Delegate to the move command.
    cmd_bookmark_move(
        ui,
        command,
        &BookmarkMoveArgs {
            names: args.names.clone(),
            from,
            to,
            allow_backwards: args.allow_backwards,
        },
    )
}
