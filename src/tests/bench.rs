use super::prelude::*;
use super::support::*;

#[test]
fn bench_terminal_policy_controls_finish_tool() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("README.md"), "benchmark repo").unwrap();

    let normal_job = bench_job(&bench_args(temp.path(), BenchTerminalPolicy::Normal)).unwrap();
    assert!(normal_job
        .personas
        .iter()
        .all(|persona| persona.allowed_tools.finish));
    assert!(normal_job
        .personas
        .iter()
        .all(|persona| persona.allowed_tools.record_finding));

    let finding_required_job = bench_job(&bench_args(
        temp.path(),
        BenchTerminalPolicy::FindingRequired,
    ))
    .unwrap();
    assert!(finding_required_job
        .personas
        .iter()
        .all(|persona| !persona.allowed_tools.finish));
    assert!(finding_required_job
        .personas
        .iter()
        .all(|persona| persona.allowed_tools.record_finding));
    assert!(finding_required_job
        .personas
        .iter()
        .all(|persona| persona.objective.contains("record_finding exactly once")));
}


