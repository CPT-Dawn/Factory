use factory_core::timestamp::Timestamp;

#[test]
fn utc_iso_8601_moments_are_read_back_as_timestamps() {
    // Seconds from `date -u -d <moment> +%s`.
    let cases = [
        ("2026-11-30T00:21:20Z", Some(1_795_998_080_000)),
        ("2026-11-20T02:47:18.365Z", Some(1_795_142_838_365)),
        ("2000-02-29T23:59:59.9Z", Some(951_868_799_900)),
        ("1970-01-01T00:00:00Z", Some(0)),
        ("2026-11-30T00:21:20", None),
        ("2026-11-30 00:21:20Z", None),
        ("2026-13-01T00:00:00Z", None),
        ("2026-11-30T24:00:00Z", None),
        ("soon", None),
        ("", None),
    ];
    for (text, millis) in cases {
        assert_eq!(
            Timestamp::parse(text),
            millis.map(Timestamp::from_unix_millis),
            "{text}"
        );
    }
}

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
