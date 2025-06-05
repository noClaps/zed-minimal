pub fn format_file_size(size: u64, use_decimal: bool) -> String {
    if use_decimal {
        if size < 1000 {
            format!("{size}B")
        } else if size < 1000 * 1000 {
            format!("{:.1}KB", size as f64 / 1000.0)
        } else {
            format!("{:.1}MB", size as f64 / (1000.0 * 1000.0))
        }
    } else {
        if size < 1024 {
            format!("{size}B")
        } else if size < 1024 * 1024 {
            format!("{:.1}KiB", size as f64 / 1024.0)
        } else {
            format!("{:.1}MiB", size as f64 / (1024.0 * 1024.0))
        }
    }
}
