//! 복제한 파일의 저장 시각 대신 원고에 기록된 최종 수정 시각으로 비교한다.
use serde_json::Value;

pub fn modified(bytes: &[u8], file_seconds: Option<i64>) -> Option<i64> {
    let recorded = std::str::from_utf8(bytes).ok().and_then(|raw| {
        let mut lines = raw.lines();
        if lines.next()? != "---" { return None; }
        let yaml = lines.take_while(|line| *line != "---").collect::<Vec<_>>().join("\n");
        let properties: Value = serde_yaml::from_str(&yaml).ok()?;
        chrono::DateTime::parse_from_rfc3339(properties["updatedAt"].as_str()?)
            .ok().map(|time| time.timestamp_millis())
    });
    recorded.or_else(|| file_seconds.and_then(|seconds| seconds.checked_mul(1000)))
}
