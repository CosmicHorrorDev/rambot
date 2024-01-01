// TODO: have a structured parsed line, so that we can do things based on timestamps and stuff

// TODO: need to change this to timestamp right for >1 hr long transcriptions
pub fn srt_like_to_telegram_ts_line(line: &str) -> Option<String> {
    let mm_ss = line.get(1..6)?;
    let text = line.get(27..)?;
    Some(format!("{mm_ss} {text}"))
}
