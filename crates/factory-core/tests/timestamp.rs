use factory_core::timestamp::Timestamp;

#[test]
fn timestamps_show_as_javascript_to_iso_string() {
    // Seconds from `date -u -d <moment> +%s`.
    let cases = [
        (0, "1970-01-01T00:00:00.000Z"),
        (951_868_799_999, "2000-02-29T23:59:59.999Z"),
        (1_795_142_838_365, "2026-11-20T02:47:18.365Z"),
        (4_107_542_400_007, "2100-03-01T00:00:00.007Z"),
        (-1_000, "1969-12-31T23:59:59.000Z"),
    ];
    for (millis, shown) in cases {
        assert_eq!(
            Timestamp::from_unix_millis(millis).to_string(),
            shown,
            "{millis}"
        );
    }
}
