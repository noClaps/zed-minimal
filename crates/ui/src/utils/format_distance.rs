// This won't be documented further as it is intended to be removed, or merged with the `time_format` crate.

use chrono::{DateTime, Local, NaiveDateTime};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DateTimeType {
    Naive(NaiveDateTime),
    Local(DateTime<Local>),
}

impl DateTimeType {
    /// Converts the [`DateTimeType`] to a [`NaiveDateTime`].
    ///
    /// If the [`DateTimeType`] is already a [`NaiveDateTime`], it will be returned as is.
    /// If the [`DateTimeType`] is a [`DateTime<Local>`], it will be converted to a [`NaiveDateTime`].
    pub fn to_naive(&self) -> NaiveDateTime {
        match self {
            DateTimeType::Naive(naive) => *naive,
            DateTimeType::Local(local) => local.naive_local(),
        }
    }
}

pub struct FormatDistance {
    date: DateTimeType,
    base_date: DateTimeType,
    include_seconds: bool,
    add_suffix: bool,
    hide_prefix: bool,
}

impl FormatDistance {
    pub fn new(date: DateTimeType, base_date: DateTimeType) -> Self {
        Self {
            date,
            base_date,
            include_seconds: false,
            add_suffix: false,
            hide_prefix: false,
        }
    }

    pub fn from_now(date: DateTimeType) -> Self {
        Self::new(date, DateTimeType::Local(Local::now()))
    }

    pub fn include_seconds(mut self, include_seconds: bool) -> Self {
        self.include_seconds = include_seconds;
        self
    }

    pub fn add_suffix(mut self, add_suffix: bool) -> Self {
        self.add_suffix = add_suffix;
        self
    }

    pub fn hide_prefix(mut self, hide_prefix: bool) -> Self {
        self.hide_prefix = hide_prefix;
        self
    }
}

impl std::fmt::Display for FormatDistance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            format_distance(
                self.date,
                self.base_date.to_naive(),
                self.include_seconds,
                self.add_suffix,
                self.hide_prefix,
            )
        )
    }
}
/// Calculates the distance in seconds between two [`NaiveDateTime`] objects.
/// It returns a signed integer denoting the difference. If `date` is earlier than `base_date`, the returned value will be negative.
///
/// ## Arguments
///
/// * `date` - A [NaiveDateTime`] object representing the date of interest
/// * `base_date` - A [NaiveDateTime`] object representing the base date against which the comparison is made
fn distance_in_seconds(date: NaiveDateTime, base_date: NaiveDateTime) -> i64 {
    let duration = date.signed_duration_since(base_date);
    -duration.num_seconds()
}

/// Generates a string describing the time distance between two dates in a human-readable way.
fn distance_string(
    distance: i64,
    include_seconds: bool,
    add_suffix: bool,
    hide_prefix: bool,
) -> String {
    let suffix = if distance < 0 { " from now" } else { " ago" };

    let distance = distance.abs();

    let minutes = distance / 60;
    let hours = distance / 3_600;
    let days = distance / 86_400;
    let months = distance / 2_592_000;

    let string = if distance < 5 && include_seconds {
        if hide_prefix {
            "5 seconds"
        } else {
            "less than 5 seconds"
        }
        .to_string()
    } else if distance < 10 && include_seconds {
        if hide_prefix {
            "10 seconds"
        } else {
            "less than 10 seconds"
        }
        .to_string()
    } else if distance < 20 && include_seconds {
        if hide_prefix {
            "20 seconds"
        } else {
            "less than 20 seconds"
        }
        .to_string()
    } else if distance < 40 && include_seconds {
        "half a minute".to_string()
    } else if distance < 60 && include_seconds {
        if hide_prefix {
            "a minute"
        } else {
            "less than a minute"
        }
        .to_string()
    } else if distance < 90 && include_seconds {
        "1 minute".to_string()
    } else if distance < 30 {
        if hide_prefix {
            "a minute"
        } else {
            "less than a minute"
        }
        .to_string()
    } else if distance < 90 {
        "1 minute".to_string()
    } else if distance < 2_700 {
        format!("{} minutes", minutes)
    } else if distance < 5_400 {
        if hide_prefix {
            "1 hour"
        } else {
            "about 1 hour"
        }
        .to_string()
    } else if distance < 86_400 {
        if hide_prefix {
            format!("{} hours", hours)
        } else {
            format!("about {} hours", hours)
        }
        .to_string()
    } else if distance < 172_800 {
        "1 day".to_string()
    } else if distance < 2_592_000 {
        format!("{} days", days)
    } else if distance < 5_184_000 {
        if hide_prefix {
            "1 month"
        } else {
            "about 1 month"
        }
        .to_string()
    } else if distance < 7_776_000 {
        if hide_prefix {
            "2 months"
        } else {
            "about 2 months"
        }
        .to_string()
    } else if distance < 31_540_000 {
        format!("{} months", months)
    } else if distance < 39_425_000 {
        if hide_prefix {
            "1 year"
        } else {
            "about 1 year"
        }
        .to_string()
    } else if distance < 55_195_000 {
        if hide_prefix { "1 year" } else { "over 1 year" }.to_string()
    } else if distance < 63_080_000 {
        if hide_prefix {
            "2 years"
        } else {
            "almost 2 years"
        }
        .to_string()
    } else {
        let years = distance / 31_536_000;
        let remaining_months = (distance % 31_536_000) / 2_592_000;

        if remaining_months < 3 {
            if hide_prefix {
                format!("{} years", years)
            } else {
                format!("about {} years", years)
            }
            .to_string()
        } else if remaining_months < 9 {
            if hide_prefix {
                format!("{} years", years)
            } else {
                format!("over {} years", years)
            }
            .to_string()
        } else {
            if hide_prefix {
                format!("{} years", years + 1)
            } else {
                format!("almost {} years", years + 1)
            }
            .to_string()
        }
    };

    if add_suffix {
        format!("{}{}", string, suffix)
    } else {
        string
    }
}

/// Get the time difference between two dates into a relative human readable string.
///
/// For example, "less than a minute ago", "about 2 hours ago", "3 months from now", etc.
///
/// Use [`format_distance_from_now`] to compare a NaiveDateTime against now.
pub fn format_distance(
    date: DateTimeType,
    base_date: NaiveDateTime,
    include_seconds: bool,
    add_suffix: bool,
    hide_prefix: bool,
) -> String {
    let distance = distance_in_seconds(date.to_naive(), base_date);

    distance_string(distance, include_seconds, add_suffix, hide_prefix)
}

/// Get the time difference between a date and now as relative human readable string.
///
/// For example, "less than a minute ago", "about 2 hours ago", "3 months from now", etc.
pub fn format_distance_from_now(
    datetime: DateTimeType,
    include_seconds: bool,
    add_suffix: bool,
    hide_prefix: bool,
) -> String {
    let now = chrono::offset::Local::now().naive_local();

    format_distance(datetime, now, include_seconds, add_suffix, hide_prefix)
}
