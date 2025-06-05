use std::time::Duration;

pub fn duration_alt_display(duration: Duration) -> String {
    if duration < Duration::from_secs(60) {
        format!("{}s", duration.as_secs())
    } else {
        duration_clock_format(duration)
    }
}

fn duration_clock_format(duration: Duration) -> String {
    let hours = duration.as_secs() / 3600;
    let minutes = (duration.as_secs() % 3600) / 60;
    let seconds = duration.as_secs() % 60;

    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else if minutes > 0 {
        format!("{minutes}:{seconds:02}")
    } else {
        format!("{seconds}")
    }
}
