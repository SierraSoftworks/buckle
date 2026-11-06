//! The shared, presentational components.

pub mod actions;
pub mod attention;
pub mod avatar;
pub mod blueprint;
pub mod changes;
pub mod commit;
pub mod countdown;
pub mod diff;
pub mod error_block;
pub mod fleet_plan;
pub mod host_card;
pub mod icons;
pub mod json_tree;
pub mod link;
pub mod meter;
pub mod modal;
pub mod pipeline;
pub mod plan;
pub mod revision;
pub mod run_action;
pub mod run_report;
pub mod segmented;
pub mod shell;
pub mod split_button;
pub mod theme;
pub mod toast;
pub mod ui;

pub use actions::{
    ActionsButton, action_items, drift_item, target_items, use_actions, use_drift_check,
    use_target_actions,
};
pub use changes::Changes;
pub use commit::{Commit, CommitCard};
pub use countdown::Countdown;
pub use diff::{DiffView, secret_text};
pub use error_block::ErrorBlock;
pub use fleet_plan::PlanViews;
pub use host_card::{HostLink, host_state_pills};
pub use json_tree::JsonTree;
pub use link::RouteLink;
pub use meter::{Meter, Segment};
pub use modal::Confirm;
pub use pipeline::Pipeline;
pub use plan::PlanGroupView;
pub use revision::{RevisionLog, use_revision_actions};
pub use run_action::{RunAction, WorkloadActions, use_workload_rollout};
pub use run_report::RunReportView;
pub use segmented::Segmented;
pub use split_button::{MenuItem, SplitButton};
pub use toast::{ToastHost, use_toast};
pub use ui::{
    Button, Empty, ErrorState, HoverTip, Kind, Loading, PageHead, Panel, Pill, Refresh, TraceLink,
};
