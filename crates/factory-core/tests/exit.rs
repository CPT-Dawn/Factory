use factory_core::exit::Exit;

#[test]
fn exit_codes_are_the_spec_table() {
    // Spec #1, "Output contract".
    let table: Vec<(u8, &str)> = Exit::ALL.iter().map(|e| (e.code(), e.meaning())).collect();
    assert_eq!(
        table,
        vec![
            (0, "ok"),
            (2, "usage"),
            (3, "precondition, config or daemon unreachable"),
            (4, "no lease"),
            (10, "needs attention"),
            (75, "busy or out of quota"),
            (76, "device lost"),
            (77, "emulator"),
            (78, "stack"),
            (124, "timeout"),
        ]
    );
}

#[test]
fn a_code_reads_back_as_its_exit() {
    assert_eq!(Exit::from_code(76), Some(Exit::DeviceLost));
    assert_eq!(Exit::from_code(1), None);
}
