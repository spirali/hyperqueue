use chrono::TimeZone;
use std::str::FromStr;
use std::time::{Duration, SystemTime};

pub struct ArgDuration(Duration);

impl ArgDuration {
    pub fn into_duration(self) -> Duration {
        self.0
    }
}

impl FromStr for ArgDuration {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(humantime::parse_duration(s)?))
    }
}

impl From<ArgDuration> for Duration {
    fn from(x: ArgDuration) -> Self {
        x.0
    }
}

pub fn local_to_system_time(datetime: chrono::NaiveDateTime) -> SystemTime {
    chrono::Local.from_local_datetime(&datetime).unwrap().into()
}
