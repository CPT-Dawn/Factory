use std::time::Duration;

use factory_core::duration::{parse, show};

#[test]
fn durations_read_as_a_whole_number_and_a_unit() {
    assert_eq!(parse("90s"), Ok(Duration::from_secs(90)));
    assert_eq!(parse("9m"), Ok(Duration::from_secs(540)));
    assert_eq!(parse("2h"), Ok(Duration::from_secs(7200)));
    assert_eq!(parse("0s"), Ok(Duration::ZERO));
    for bad in [
        "",
        "9",
        "m",
        "1.5m",
        "-1s",
        "9 m",
        "9d",
        "99999999999999999999h",
    ] {
        assert!(parse(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn a_duration_shows_in_its_largest_whole_unit() {
    for (secs, shown) in [
        (0, "0s"),
        (1, "1s"),
        (90, "90s"),
        (540, "9m"),
        (7200, "2h"),
        (3660, "61m"),
    ] {
        assert_eq!(show(Duration::from_secs(secs)), shown);
        assert_eq!(parse(shown), Ok(Duration::from_secs(secs)));
    }
}
