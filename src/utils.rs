// TODO: have a structured parsed line, so that we can do things based on timestamps and stuff

// FIXME: temporarily limiting the number of lines to avoid wall-of-texting things. Setup a sidecar
// chat (after getting config stuff setup)
pub fn srt_like_to_telegram_ts(lines: &str) -> String {
    let converted: Vec<_> = lines
        .lines()
        .take(10)
        .filter_map(srt_like_to_telegram_ts_line)
        .collect::<Vec<_>>();

    let truncated = converted.len() == 10;
    let mut output = converted.join("\n");
    if truncated {
        output.push_str("\n[truncated]");
    }
    output
}

// TODO: need to change this to timestamp right for >1 hr long transcriptions
pub fn srt_like_to_telegram_ts_line(line: &str) -> Option<String> {
    let mm_ss = line.get(1..6)?;
    let text = line.get(27..)?;
    Some(format!("{mm_ss} {text}"))
}
