pub(crate) mod connection;
mod factory;
mod hc;
mod history_read;
mod id;
mod runtime;
mod runtime_registry;
pub mod state;
mod title;

pub(crate) use factory::{TerminalCommand, TerminalFactory};
pub(crate) use history_read::{merge_scrolled_up, snapshot_text, ScreenSnapshot, UpwardMerge};
pub use id::TerminalId;
pub use runtime::TerminalRuntime;
pub(crate) use runtime_registry::TerminalRuntimeRegistry;
pub use state::{
    AgentMetadataReport, EffectivePresentation, EffectiveStateChange, TerminalState,
    TerminalStateMutation,
};
pub(crate) use title::stripped_terminal_title;
