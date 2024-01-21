#[derive(Clone, Debug)]
pub struct Line {
    pub start_secs: u32,
    pub end_secs: u32,
    pub text: String,
}

impl Line {
    pub fn new(line: &str) -> Option<Self> {
        let start_min = line.get(1..3)?;
        let start_sec = line.get(4..6)?;
        let end_min = line.get(15..17)?;
        let end_sec = line.get(18..20)?;
        let text = line.get(27..)?.to_owned();

        let start_secs = start_min.parse::<u32>().ok()? * 60 + start_sec.parse::<u32>().ok()?;
        let end_secs = end_min.parse::<u32>().ok()? * 60 + end_sec.parse::<u32>().ok()?;

        Some(Self {
            start_secs,
            end_secs,
            text,
        })
    }

    pub fn into_telegram_line(self) -> String {
        format!(
            "{:02}:{:02} {}",
            self.start_secs / 60,
            self.start_secs % 60,
            self.text
        )
    }
}
