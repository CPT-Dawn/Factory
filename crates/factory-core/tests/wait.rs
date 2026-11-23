use factory_core::wait::WaitOn;

#[test]
fn a_wait_reads_back_from_its_kind_and_what_it_is_on() {
    for (on, kind, target) in [
        (WaitOn::Lead("lead-cart".to_owned()), "lead", "lead-cart"),
        (WaitOn::Ci("acme/shop#12".to_owned()), "ci", "acme/shop#12"),
        (WaitOn::Review("t1-r1".to_owned()), "review", "t1-r1"),
        (WaitOn::Lease(7), "lease", "7"),
        (WaitOn::Claude, "claude", "quota"),
    ] {
        assert_eq!(on.parts(), (kind, target.to_owned()));
        assert_eq!(WaitOn::from_parts(kind, target), Some(on));
    }
    assert_eq!(WaitOn::from_parts("lease", "seven"), None);
    assert_eq!(WaitOn::from_parts("phone", "1"), None);
}
