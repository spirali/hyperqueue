use chrono::Duration as ChronoDuration;
use serde_json::Value;
use std::process::Command;
use std::str;
use std::time::Duration;

fn parse_duration(raw_time: &str) -> anyhow::Result<Duration> {
    let numbers: Vec<&str> = raw_time.split(':').collect();
    let duration = ChronoDuration::hours(numbers[0].parse()?)
        + ChronoDuration::minutes(numbers[1].parse()?)
        + ChronoDuration::seconds(numbers[2].parse()?);
    Ok(duration.to_std()?)
}

fn get_time(job_id: &str, data: &str) -> anyhow::Result<Duration> {
    let data_json: Value = serde_json::from_str(data)?;

    let walltime = parse_duration(
        data_json["Jobs"][job_id]["Resource_List"]["walltime"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Could not find walltime key for job {}", job_id))?,
    )?;
    let used = parse_duration(
        data_json["Jobs"][job_id]["resources_used"]["walltime"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Could not find used time key for job {}", job_id))?,
    )?;

    Ok(walltime - used)
}

/// Calculates how much time is left for the given job using `qstat`.
/// TODO: make this async
pub fn get_remaining_timelimit(job_id: &str) -> anyhow::Result<Duration> {
    let result = Command::new("qstat")
        .args(&["-f", "-F", "json", job_id])
        .output()?;
    if !result.status.success() {
        anyhow::bail!(
            "qstat command exited with {}: {}\n{}",
            result.status,
            String::from_utf8_lossy(&result.stderr),
            String::from_utf8_lossy(&result.stdout)
        );
    }

    let output = String::from_utf8_lossy(&result.stdout).into_owned();
    log::debug!("qstat output: {}", output.trim());

    get_time(job_id, output.as_str())
}

/// Format a duration as a PBS time string, e.g. 01:05:02
pub fn format_pbs_duration(duration: &Duration) -> String {
    let mut seconds = duration.as_secs();
    let hours = seconds / 3600;
    seconds %= 3600;
    let minutes = seconds / 60;
    seconds %= 60;
    format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
}

pub fn parse_pbs_datetime(datetime: &str) -> anyhow::Result<chrono::NaiveDateTime> {
    Ok(chrono::NaiveDateTime::parse_from_str(
        datetime,
        "%a %b %d %H:%M:%S %Y",
    )?)
}

#[cfg(test)]
mod test {
    use crate::common::manager::pbs::{format_pbs_duration, parse_pbs_datetime};
    use std::time::Duration;

    #[test]
    fn test_format_pbs_duration() {
        assert_eq!(format_pbs_duration(&Duration::from_secs(0)), "00:00:00");
        assert_eq!(format_pbs_duration(&Duration::from_secs(1)), "00:00:01");
        assert_eq!(format_pbs_duration(&Duration::from_secs(61)), "00:01:01");
        assert_eq!(format_pbs_duration(&Duration::from_secs(3661)), "01:01:01");
    }

    #[test]
    fn test_parse_pbs_datetime() {
        let date = parse_pbs_datetime("Thu Aug 19 13:05:17 2021").unwrap();
        assert_eq!(
            date.format("%d.%m.%Y %H:%M:%S").to_string(),
            "19.08.2021 13:05:17"
        );
    }
}
