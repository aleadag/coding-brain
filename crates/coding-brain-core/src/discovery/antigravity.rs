use crate::process::ProcessSnapshotEntry;
use crate::provider::AgentProvider;
use crate::session::AgentSession;

pub(crate) fn sessions_from_processes(processes: &[ProcessSnapshotEntry]) -> Vec<AgentSession> {
    processes
        .iter()
        .filter(|process| process.has_executable_basename(&["agy"]))
        .map(|process| super::session_from_provider_process(AgentProvider::Antigravity, process))
        .collect()
}
