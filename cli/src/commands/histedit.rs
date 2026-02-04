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

use std::collections::HashMap;
use std::collections::HashSet;
use std::io;
use std::time::Duration;

use crossterm::ExecutableCommand as _;
use crossterm::event::Event;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use crossterm::event::{self};
use crossterm::terminal::EnterAlternateScreen;
use crossterm::terminal::LeaveAlternateScreen;
use crossterm::terminal::disable_raw_mode;
use crossterm::terminal::enable_raw_mode;
use itertools::Itertools as _;
use jj_lib::backend::CommitId;
use jj_lib::commit::Commit;
use jj_lib::dag_walk;
use jj_lib::repo::Repo;
use jj_lib::revset::RevsetIteratorExt as _;
use ratatui::Terminal;
use ratatui::layout::Constraint;
use ratatui::layout::Direction;
use ratatui::layout::Layout;
use ratatui::layout::Offset;
use ratatui::prelude::CrosstermBackend;
use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::text::Text;
use renderdag::Ancestor;
use renderdag::GraphRowRenderer;
use renderdag::Renderer;
use tracing::instrument;

use crate::cli_util::CommandHelper;
use crate::cli_util::RevisionArg;
use crate::cli_util::short_commit_hash;
use crate::command_error::CommandError;
use crate::command_error::internal_error;
use crate::command_error::user_error;
use crate::complete;
use crate::ui::Ui;

/// Interactively edit the commit history.
#[derive(clap::Args, Clone, Debug)]
pub(crate) struct HisteditArgs {
    /// The revisions to edit.
    ///
    /// If no revisions are specified, this defaults to the `revsets.histedit`
    /// setting, or `reachable(@, mutable())` if it is not set.
    #[arg(long, short, value_name = "REVSETS")]
    #[arg(add = clap_complete::ArgValueCompleter::new(complete::revset_expression_mutable))]
    revisions: Vec<RevisionArg>,
}

#[instrument(skip_all)]
pub(crate) fn cmd_histedit(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &HisteditArgs,
) -> Result<(), CommandError> {
    let workspace_command = command.workspace_helper(ui)?;
    let repo = workspace_command.repo();
    let target_expression = if args.revisions.is_empty() {
        let revs = workspace_command
            .settings()
            .get_string("revsets.histedit")?;
        workspace_command.parse_revset(ui, &RevisionArg::from(revs))?
    } else {
        workspace_command.parse_union_revsets(ui, &args.revisions)?
    }
    .resolve()?;
    workspace_command.check_rewritable_expr(&target_expression)?;

    let gaps_revset = target_expression
        .connected()
        .minus(&target_expression)
        .evaluate(repo.as_ref())?;
    if let Some(commit_id) = gaps_revset.iter().next() {
        return Err(
            user_error("Cannot edit history of revsets with gaps in.").hinted(format!(
                "Revision {} would need to be in the set.",
                short_commit_hash(&commit_id?)
            )),
        );
    }

    let children_revset = target_expression
        .children()
        .minus(&target_expression)
        .evaluate(repo.as_ref())?;
    let external_children: Vec<_> = children_revset.iter().commits(repo.store()).try_collect()?;

    let revset = target_expression.evaluate(repo.as_ref())?;
    let commits: Vec<Commit> = revset.iter().commits(repo.store()).try_collect()?;
    if commits.is_empty() {
        writeln!(ui.status(), "No revisions to edit.")?;
        return Ok(());
    }

    // Set up the terminal
    io::stdout().execute(EnterAlternateScreen)?;
    enable_raw_mode()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.clear()?;

    let state = State::new(commits, external_children);

    let result = run_tui(
        ui,
        &mut terminal,
        &workspace_command.commit_summary_template(),
        state,
    );

    // Restore the terminal
    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;

    result
}

struct State {
    commits: HashMap<CommitId, Commit>,
    /// Commits in the original revset order
    original_order: Vec<CommitId>,
    /// The current order of commits in the UI
    current_order: Vec<CommitId>,
    parents: HashMap<CommitId, Vec<CommitId>>,
    children: HashMap<CommitId, Vec<CommitId>>,
    external_parents: HashSet<CommitId>,
    external_children: HashSet<CommitId>,
}

impl State {
    fn new(commits: Vec<Commit>, external_children: Vec<Commit>) -> Self {
        let original_order = commits
            .iter()
            .map(|commit| commit.id().clone())
            .collect_vec();
        let commits: HashMap<CommitId, Commit> = commits
            .into_iter()
            .map(|commit| {
                let id = commit.id().clone();
                (id, commit)
            })
            .collect();
        let mut parents: HashMap<CommitId, Vec<CommitId>> = HashMap::new();
        let mut children: HashMap<CommitId, Vec<CommitId>> = HashMap::new();
        let mut external_parents = HashSet::new();
        for (id, commit) in &commits {
            parents.insert(id.clone(), vec![]);
            children.insert(id.clone(), vec![]);
            for parent_id in commit.parent_ids() {
                parents
                    .entry(id.clone())
                    .or_default()
                    .push(parent_id.clone());
                if commits.contains_key(parent_id) {
                    children
                        .entry(parent_id.clone())
                        .or_default()
                        .push(id.clone());
                } else {
                    external_parents.insert(parent_id.clone());
                }
            }
        }
        for child in &external_children {
            for parent_id in child.parent_ids() {
                if commits.contains_key(parent_id) {
                    children
                        .entry(parent_id.clone())
                        .or_default()
                        .push(child.id().clone());
                }
            }
        }
        let external_children = external_children
            .iter()
            .map(|commit| commit.id().clone())
            .collect();
        let current_order = original_order.clone();
        State {
            commits,
            original_order,
            current_order,
            parents,
            children,
            external_parents,
            external_children,
        }
    }

    fn update_commit_order(&mut self) {
        let heads: HashSet<&CommitId> = dag_walk::heads(
            self.original_order.iter(),
            |id| *id,
            |id| {
                self.parents
                    .get(id)
                    .unwrap()
                    .iter()
                    .filter(|id| self.commits.contains_key(id))
            },
        );
        // Use the original order to get a deterninisic order.
        let heads = self
            .original_order
            .iter()
            .filter(|id| heads.contains(id))
            .collect_vec();
        let commit_ids: Vec<&CommitId> = dag_walk::topo_order_reverse(
            heads,
            |id| *id,
            |id| {
                self.parents
                    .get(id)
                    .unwrap()
                    .iter()
                    .filter(|id| self.commits.contains_key(id))
            },
            |_| panic!("cycle detected"),
        )
        .unwrap();
        self.current_order = commit_ids.into_iter().cloned().collect();
    }
}

fn run_tui<B: ratatui::backend::Backend>(
    ui: &mut Ui,
    terminal: &mut Terminal<B>,
    template: &crate::templater::TemplateRenderer<Commit>,
    mut state: State,
) -> Result<(), CommandError> {
    let help_items = [("q", "quit")];
    let mut help_spans = Vec::new();
    for (i, (key, desc)) in help_items.iter().enumerate() {
        if i > 0 {
            help_spans.push(Span::raw(" "));
        }
        help_spans.push(Span::styled(*key, Style::default().fg(Color::Magenta)));
        help_spans.push(Span::raw(format!(" {desc}")));
    }
    let help_line = Line::from(help_spans);

    loop {
        terminal
            .draw(|frame| {
                let layout = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Fill(1), Constraint::Length(1)])
                    .split(frame.area());
                let main_area = layout[0];
                let help_area = layout[1];

                let mut row_renderer = GraphRowRenderer::new()
                    .output()
                    .with_min_row_height(2)
                    .build_box_drawing();
                let mut row_area = main_area;
                for id in &state.current_order {
                    // TODO: Make the graph column width depend on what's needed to render the
                    // graph.
                    let row_layout =
                        Layout::horizontal([Constraint::Min(10), Constraint::Fill(100)])
                            .split(row_area);
                    let graph_area = row_layout[0];
                    let text_area = row_layout[1];

                    let commit = state.commits.get(id).unwrap();
                    let new_parents = state.parents.get(id).unwrap();

                    let edges = new_parents
                        .iter()
                        .map(|parent_id| Ancestor::Parent(parent_id))
                        .collect_vec();
                    let graph_lines =
                        row_renderer.next_row(id, edges, "○".to_string(), "".to_string());
                    let graph_text = Text::from(graph_lines);
                    row_area = row_area
                        .offset(Offset {
                            x: 0,
                            y: graph_text.height() as i32,
                        })
                        .intersection(main_area);
                    frame.render_widget(graph_text, graph_area);

                    let mut text_lines = vec![];
                    let mut formatter = ui.new_formatter(&mut text_lines);
                    template.format(&commit, formatter.as_mut()).unwrap();
                    drop(formatter);
                    let text = ansi_to_tui::IntoText::into_text(&text_lines).unwrap();
                    frame.render_widget(text, text_area);
                }

                frame.render_widget(&help_line, help_area);
            })
            .map_err(|e| internal_error(format!("Failed to draw TUI: {e}")))?;

        if event::poll(Duration::from_millis(100))
            .map_err(|e| internal_error(format!("Failed to poll for TUI events: {e}")))?
            && let Event::Key(KeyEvent {
                code, modifiers, ..
            }) = event::read()
                .map_err(|e| internal_error(format!("Failed to read TUI events: {e}")))?
        {
            match (code, modifiers) {
                (KeyCode::Char('q'), KeyModifiers::NONE) => {
                    return Ok(());
                }
                _ => {
                    continue;
                }
            }
            state.update_commit_order();
        }
    }
}
