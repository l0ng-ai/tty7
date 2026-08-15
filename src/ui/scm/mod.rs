//! Source control: the panel, its file rows, the commit box, and the graph.
//!
//! Every file here hangs `impl Tty7App` blocks, the same shape `sftp.rs` and
//! `file_tree.rs` use. The directory only keeps the surface from piling into
//! `right_panel.rs`.

// The graph and the commit detail view both have callers now, so their allows
// are gone. What is left is `status_rank`, which is the file tree's to use.
pub(crate) mod actions;
pub(crate) mod detail;
pub(crate) mod graph;
pub(crate) mod panel;
pub(crate) mod path;
pub(crate) mod state;
pub(crate) mod status;

pub(crate) use actions::ScmIntent;
pub(crate) use state::{GraphState, ScmPanelState};
