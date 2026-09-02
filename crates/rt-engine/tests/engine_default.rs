// Verify the engine banner/selection: with the feature off, default is alacritty;
// RT_ENGINE=vtterm forces in-house. (The default-flip via feature is compile-time.)
//
// NOTE: This is a single test, not two, because RT_ENGINE is a process-global
// environment variable. Cargo runs tests from one binary on parallel threads in a
// single process, so two independent tests would race on the mutation and become
// non-deterministic. Both checks are performed sequentially here, and the
// environment is restored at the end.
#[test]
fn rt_engine_selection() {
    // Capture the original state of RT_ENGINE so we can restore it.
    let original_rt_engine = std::env::var("RT_ENGINE").ok();
    let budget = std::sync::Arc::new(rt_engine::budget::Budget::default());

    // Test 1: RT_ENGINE=vtterm must select the in-house engine.
    std::env::set_var("RT_ENGINE", "vtterm");
    let pane = rt_engine::TermPane::spawn(
        Some(("/bin/sh".into(), vec!["-c".into(), "printf X".into()])), None, 20, 5, &budget).unwrap();
    assert!(matches!(pane, rt_engine::TermPane::Vt(_)), "RT_ENGINE=vtterm must pick the in-house engine");

    // Test 2: With RT_ENGINE unset, the default must follow the build feature.
    std::env::remove_var("RT_ENGINE");
    let pane = rt_engine::TermPane::spawn(
        Some(("/bin/sh".into(), vec!["-c".into(), "printf X".into()])), None, 20, 5, &budget).unwrap();
    let is_vt = matches!(pane, rt_engine::TermPane::Vt(_));
    assert_eq!(is_vt, cfg!(feature = "vtterm-default"), "default must follow the build feature");

    // Restore the environment to its prior state.
    match original_rt_engine {
        Some(value) => std::env::set_var("RT_ENGINE", value),
        None => std::env::remove_var("RT_ENGINE"),
    }
}
