#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub start_secs: u32,
    pub end_secs: u32,
    pub text: String,
}

impl Line {
    pub fn to_telegram_line(&self) -> String {
        format!(
            "{:02}:{:02} {}",
            self.start_secs / 60,
            self.start_secs % 60,
            self.text
        )
    }
}

// TODO: need streaming support for `whisper_rs`
pub struct SegmentCallbackData {
    pub segment: i32,
    pub start_timestamp: i64,
    pub end_timestamp: i64,
    pub text: String,
}

impl From<SegmentCallbackData> for Line {
    fn from(segment: SegmentCallbackData) -> Self {
        let SegmentCallbackData {
            segment: _,
            start_timestamp,
            end_timestamp,
            text,
        } = segment;

        Self {
            // Segment timestamps are in centi-seconds
            start_secs: (start_timestamp / 100).try_into().unwrap(),
            end_secs: (end_timestamp / 100).try_into().unwrap(),
            text,
        }
    }
}
