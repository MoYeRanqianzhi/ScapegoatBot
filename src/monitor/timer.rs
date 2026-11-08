//! 定时源：`every` / `at` / `cron` 的解析、下一次触发时间与重启补投。
//!
//! - 三种写法只在 [`TimerSpec::parse`] 解析一次：配置校验、配置加载、`Monitor` 工具与
//!   `Agent` 工具的 monitor 绑定都调用它，写错在添加时就报错，不会到运行时才静默停摆。
//! - 时间按本地时区的“墙上时间”计算。夏令时跳过的时刻顺延到之后第一个有效分钟，
//!   回拨造成的重复时刻只取较早的一次（[`resolve_local`]）。
//! - cron 是标准 5 字段语义：日与周两个字段**都**受限时取“或”，任一以 `*` 开头时只看
//!   另一个。下一次触发按天推算（[`CronSchedule::next_after`]），永远不会匹配的表达式
//!   （例如 `0 0 30 2 *`）在解析时拒绝。
//! - 上次触发时间只在真正触发时写入：`every` 以它为锚点，重启不会让周期漂移；
//!   补投（`catch_up = "coalesce"`）也以它为起点计数。

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use chrono::{DateTime, Datelike, Local, LocalResult, NaiveDate, NaiveDateTime, TimeZone};

use super::{Monitor, MonitorEvent, MonitorSink, deliver_once};
use crate::config::CatchUp;
use crate::storage::Database;

/// `every` 的最小周期（秒）。
const MIN_EVERY_SECS: u64 = 60;
/// 补投计数的上限：长时间停机只需要知道“错过了很多次”。
const MAX_MISSED: u64 = 1_000;
/// 推算下一次触发最多向前看的天数：覆盖 2 月 29 日这类四年一遇的日期。
const LOOKAHEAD_DAYS: u32 = 4 * 366 + 1;
/// 定时循环每段最多睡这么久再重新计算：系统休眠、改时钟、夏令时切换后能及时纠正，
/// 取消也不用等一整个周期。
const MAX_SLEEP: Duration = Duration::from_secs(60);

/// 定时源。
#[derive(Debug, Clone)]
pub enum TimerSpec {
    /// 固定周期（≥ [`MIN_EVERY_SECS`]），以上次触发为锚点。
    Every(Duration),
    /// 每天本地时间 `HH:MM`。
    At { hour: u32, minute: u32 },
    /// 5 字段 cron（本地时区）。
    Cron(CronSchedule),
}

impl TimerSpec {
    /// 解析 `cron` / `every` / `at` 三选一。
    pub fn parse(
        cron: Option<&str>,
        every: Option<&str>,
        at: Option<&str>,
    ) -> Result<Self, String> {
        match (cron, every, at) {
            (Some(cron), None, None) => CronSchedule::parse(cron).map(Self::Cron),
            (None, Some(every), None) => {
                let seconds = crate::config::parse_duration_secs(every)
                    .ok_or_else(|| format!("invalid timer.every duration {every:?}"))?;
                if seconds < MIN_EVERY_SECS {
                    return Err(format!("timer.every must be at least {MIN_EVERY_SECS}s"));
                }
                Ok(Self::Every(Duration::from_secs(seconds)))
            }
            (None, None, Some(at)) => parse_at(at),
            _ => Err("timer must set exactly one of cron, every, at".to_owned()),
        }
    }

    /// 从工具参数 `{"cron"|"every"|"at": "..."}` 解析；字段存在但不是字符串时报错。
    pub fn from_json(value: &serde_json::Value) -> Result<Self, String> {
        let field = |key: &str| match value.get(key) {
            None => Ok(None),
            Some(serde_json::Value::String(text)) => Ok(Some(text.as_str())),
            Some(_) => Err(format!("timer.{key} must be a string")),
        };
        Self::parse(field("cron")?, field("every")?, field("at")?)
    }

    /// 严格晚于 `after` 的下一次触发；`Every` 把 `after` 当作上次触发。
    pub fn next_after(&self, after: DateTime<Local>) -> Option<DateTime<Local>> {
        match self {
            Self::Every(period) => {
                after.checked_add_signed(chrono::Duration::from_std(*period).ok()?)
            }
            Self::At { hour, minute } => next_daily(after, *hour, *minute),
            Self::Cron(cron) => cron.next_after(after),
        }
    }
}

/// `HH:MM`，00:00–23:59。
fn parse_at(raw: &str) -> Result<TimerSpec, String> {
    let invalid = || format!("timer.at must be HH:MM between 00:00 and 23:59, got {raw:?}");
    let (hour, minute) = raw.trim().split_once(':').ok_or_else(invalid)?;
    let hour: u32 = hour.parse().map_err(|_| invalid())?;
    let minute: u32 = minute.parse().map_err(|_| invalid())?;
    if hour > 23 || minute > 59 {
        return Err(invalid());
    }
    Ok(TimerSpec::At { hour, minute })
}

/// 本地墙上时间 → 时刻。
///
/// 夏令时跳过的时刻（不存在）顺延到之后第一个有效分钟；回拨造成的重复时刻取较早的
/// 一次。时区跳变不超过几个小时，三小时内一定能落到有效时刻。
fn resolve_local(naive: NaiveDateTime) -> Option<DateTime<Local>> {
    for offset in 0..180 {
        let candidate = naive.checked_add_signed(chrono::Duration::minutes(offset))?;
        match Local.from_local_datetime(&candidate) {
            LocalResult::Single(time) => return Some(time),
            LocalResult::Ambiguous(earliest, _) => return Some(earliest),
            LocalResult::None => continue,
        }
    }
    None
}

/// 每天 `HH:MM`：严格晚于 `after` 的下一次（看今天、明天、后天，足以越过任何时区跳变）。
fn next_daily(after: DateTime<Local>, hour: u32, minute: u32) -> Option<DateTime<Local>> {
    let mut day = after.date_naive();
    for _ in 0..3 {
        let candidate = resolve_local(day.and_hms_opt(hour, minute, 0)?)?;
        if candidate > after {
            return Some(candidate);
        }
        day = day.succ_opt()?;
    }
    None
}

/// 某月最多有几天（2 月按闰年算）；用于判断只受日字段限制的 cron 是否可能匹配。
fn max_days_in_month(month: u32) -> u32 {
    match month {
        2 => 29,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// 5 字段 cron（本地时区）：分 时 日 月 周。
#[derive(Debug, Clone)]
pub struct CronSchedule {
    minutes: Vec<u32>,
    hours: Vec<u32>,
    days: Vec<u32>,
    months: Vec<u32>,
    weekdays: Vec<u32>,
    /// 日字段受限（不以 `*` 开头）；与周字段一起决定按“与”还是“或”匹配日期。
    days_restricted: bool,
    /// 周字段受限（不以 `*` 开头）。
    weekdays_restricted: bool,
}

impl CronSchedule {
    /// 解析并校验；永远不会匹配的表达式返回错误。
    pub fn parse(expression: &str) -> Result<Self, String> {
        let fields: Vec<&str> = expression.split_whitespace().collect();
        if fields.len() != 5 {
            return Err("cron needs 5 fields: minute hour day month weekday".to_owned());
        }
        let schedule = Self {
            minutes: parse_field(fields[0], 0, 59)?,
            hours: parse_field(fields[1], 0, 23)?,
            days: parse_field(fields[2], 1, 31)?,
            months: parse_field(fields[3], 1, 12)?,
            weekdays: {
                // 0 与 7 都表示周日。
                let mut days: Vec<u32> = parse_field(fields[4], 0, 7)?
                    .into_iter()
                    .map(|day| if day == 7 { 0 } else { day })
                    .collect();
                days.sort_unstable();
                days.dedup();
                days
            },
            days_restricted: !fields[2].starts_with('*'),
            weekdays_restricted: !fields[4].starts_with('*'),
        };
        if !schedule.can_ever_match() {
            return Err(format!(
                "cron {expression:?} never matches: none of its days exist in its months"
            ));
        }
        Ok(schedule)
    }

    /// 这一天是否可能触发。
    fn day_matches(&self, date: NaiveDate) -> bool {
        if !self.months.contains(&date.month()) {
            return false;
        }
        let by_day = self.days.contains(&date.day());
        let by_weekday = self
            .weekdays
            .contains(&date.weekday().num_days_from_sunday());
        match (self.days_restricted, self.weekdays_restricted) {
            // 标准 cron：两个都写了具体值时，任一满足即可。
            (true, true) => by_day || by_weekday,
            (true, false) => by_day,
            (false, true) => by_weekday,
            (false, false) => true,
        }
    }

    /// 是否存在能匹配的日期：只受日字段限制时，选中的某个月里得有选中的某一天。
    fn can_ever_match(&self) -> bool {
        if self.weekdays_restricted || !self.days_restricted {
            return true;
        }
        self.months.iter().any(|month| {
            let length = max_days_in_month(*month);
            self.days.iter().any(|day| *day <= length)
        })
    }

    /// 严格晚于 `after` 的下一次匹配；按天推算，最多向前看 [`LOOKAHEAD_DAYS`] 天。
    pub fn next_after(&self, after: DateTime<Local>) -> Option<DateTime<Local>> {
        let start = after.naive_local();
        let mut date = start.date();
        for _ in 0..LOOKAHEAD_DAYS {
            if self.day_matches(date) {
                for hour in &self.hours {
                    for minute in &self.minutes {
                        let Some(naive) = date.and_hms_opt(*hour, *minute, 0) else {
                            continue;
                        };
                        // 墙上时间早于起点的候选不可能晚于起点（时区跳变只会往后顺延），
                        // 跳过它们可以省掉时区换算。
                        if naive < start {
                            continue;
                        }
                        if let Some(time) = resolve_local(naive)
                            && time > after
                        {
                            return Some(time);
                        }
                    }
                }
            }
            date = date.succ_opt()?;
        }
        None
    }
}

/// 解析 cron 字段：`*`、`*/n`、`a-b`、`a-b/n`、逗号列表。
fn parse_field(field: &str, min: u32, max: u32) -> Result<Vec<u32>, String> {
    let mut values = Vec::new();
    for part in field.split(',') {
        let (range_part, step) = match part.split_once('/') {
            Some((range, step)) => (
                range,
                step.parse::<u32>()
                    .map_err(|_| format!("invalid step in {part:?}"))?,
            ),
            None => (part, 1),
        };
        if step == 0 {
            return Err("cron step must be greater than zero".to_owned());
        }
        let (start, end) = if range_part == "*" {
            (min, max)
        } else if let Some((start, end)) = range_part.split_once('-') {
            (
                start
                    .parse::<u32>()
                    .map_err(|_| format!("invalid range start in {part:?}"))?,
                end.parse::<u32>()
                    .map_err(|_| format!("invalid range end in {part:?}"))?,
            )
        } else {
            let value = range_part
                .parse::<u32>()
                .map_err(|_| format!("invalid value in {part:?}"))?;
            (value, value)
        };
        if start < min || end > max || start > end {
            return Err(format!("cron value out of range in {part:?}"));
        }
        let mut value = start;
        while value <= end {
            values.push(value);
            value += step;
        }
    }
    values.sort_unstable();
    values.dedup();
    if values.is_empty() {
        return Err("cron field is empty".to_owned());
    }
    Ok(values)
}

/// 计算 `last`（不含）到 `now`（含）之间错过的触发次数，最多 [`MAX_MISSED`]。
pub(super) fn missed_occurrences(
    timer: &TimerSpec,
    last: DateTime<Local>,
    now: DateTime<Local>,
) -> u64 {
    if now <= last {
        return 0;
    }
    if let TimerSpec::Every(period) = timer {
        let seconds = (now - last).num_seconds().max(0) as u64;
        return (seconds / period.as_secs().max(1)).min(MAX_MISSED);
    }
    let mut count = 0;
    let mut cursor = last;
    while count < MAX_MISSED
        && let Some(next) = timer.next_after(cursor)
        && next <= now
    {
        count += 1;
        cursor = next;
    }
    count
}

fn last_fire_key(name: &str) -> String {
    format!("monitor.last:{name}")
}

/// 上次真正触发的时间（重启后用来对齐 `every` 与计算补投）。
fn read_last_fire(db: Option<&Arc<Database>>, name: &str) -> Option<DateTime<Local>> {
    let db = db?;
    let raw = db.meta_get(&last_fire_key(name)).ok()??;
    let text = String::from_utf8(raw).ok()?;
    let timestamp: i64 = text.trim().parse().ok()?;
    DateTime::from_timestamp(timestamp, 0).map(|utc| utc.with_timezone(&Local))
}

/// 记录一次真正的触发；只在触发时调用，启动时不写。
fn write_last_fire(db: Option<&Arc<Database>>, name: &str, at: DateTime<Local>) {
    let Some(db) = db else {
        return;
    };
    let _ = db.meta_set(&last_fire_key(name), at.timestamp().to_string().as_bytes());
}

/// 启动定时循环。
pub(super) fn spawn_timer(monitor: Arc<Monitor>, sink: Arc<dyn MonitorSink>, timer: TimerSpec) {
    tokio::spawn(async move {
        let db = monitor.db.as_ref();
        let mut last = read_last_fire(db, &monitor.name);
        // 重启补投：只补一次（`coalesce`），`skip` 直接放弃。
        if monitor.catch_up == CatchUp::Coalesce
            && let Some(previous) = last
        {
            let now = Local::now();
            let missed = missed_occurrences(&timer, previous, now);
            if missed > 0 {
                let event = MonitorEvent {
                    text: format!("[timer catch-up] {missed} missed occurrence(s)"),
                    ..MonitorEvent::default()
                };
                deliver_once(&monitor, &*sink, Some(&event), missed, false).await;
                last = Some(now);
                write_last_fire(db, &monitor.name, now);
            }
        }

        loop {
            if monitor.cancelled.load(Ordering::SeqCst) {
                break;
            }
            let now = Local::now();
            let next = match &timer {
                // 以上次真实触发为锚点；没有记录、或已经错过（`skip`）时从现在起算。
                TimerSpec::Every(_) => last
                    .and_then(|last| timer.next_after(last))
                    .filter(|next| *next > now)
                    .or_else(|| timer.next_after(now)),
                _ => timer.next_after(now),
            };
            let Some(next) = next else {
                sink.deliver(monitor.system_delivery(
                    "internal",
                    format!(
                        "system: monitor {:?} has no future occurrence; its timer stopped",
                        monitor.name
                    ),
                ))
                .await;
                break;
            };
            // 分段睡到 `next`：每段醒来先看是否已取消，再按当前时钟重算剩余时间。
            loop {
                if monitor.cancelled.load(Ordering::SeqCst) {
                    return;
                }
                let remaining = (next - Local::now()).to_std().unwrap_or_default();
                if remaining.is_zero() {
                    break;
                }
                tokio::time::sleep(remaining.min(MAX_SLEEP)).await;
            }
            let fired = Local::now();
            last = Some(fired);
            write_last_fire(db, &monitor.name, fired);
            deliver_once(&monitor, &*sink, None, 1, false).await;
            if monitor.one_shot || monitor.cancelled.load(Ordering::SeqCst) {
                break;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .expect("an unambiguous local time")
    }

    #[test]
    fn cron_parser_handles_ranges_steps_and_lists() {
        let schedule = CronSchedule::parse("*/15 9-11 * * 1-5").unwrap();
        assert_eq!(schedule.minutes, vec![0, 15, 30, 45]);
        assert_eq!(schedule.hours, vec![9, 10, 11]);
        assert_eq!(schedule.weekdays, vec![1, 2, 3, 4, 5]);
        assert!(CronSchedule::parse("* * *").is_err());
        assert!(CronSchedule::parse("60 * * * *").is_err());
        // 0 与 7 都是周日。
        assert_eq!(
            CronSchedule::parse("0 0 * * 0,7").unwrap().weekdays,
            vec![0]
        );
    }

    #[test]
    fn impossible_dates_are_rejected_when_parsed() {
        assert!(CronSchedule::parse("0 0 30 2 *").is_err());
        assert!(CronSchedule::parse("0 0 31 4,6,9,11 *").is_err());
        // 2 月 29 日存在（闰年）；日与周都受限时按“或”，周字段总能匹配。
        assert!(CronSchedule::parse("0 0 29 2 *").is_ok());
        assert!(CronSchedule::parse("0 0 30 2 1").is_ok());
    }

    #[test]
    fn day_and_weekday_are_or_when_both_are_restricted() {
        // 每月 1 日，或每周一。
        let schedule = CronSchedule::parse("0 9 1 * 1").unwrap();
        // 2026-11-02 是周一，不是 1 日。
        let next = schedule.next_after(local(2026, 11, 1, 10, 0)).unwrap();
        assert_eq!(next, local(2026, 11, 2, 9, 0));
        // 日字段以 `*` 开头时只看周字段。
        let weekly = CronSchedule::parse("0 9 */2 * 1").unwrap();
        assert!(!weekly.days_restricted);
        let next = weekly.next_after(local(2026, 11, 1, 10, 0)).unwrap();
        assert_eq!(next, local(2026, 11, 2, 9, 0));
    }

    #[test]
    fn next_after_is_strictly_later_and_finds_rare_dates() {
        let every_minute = CronSchedule::parse("* * * * *").unwrap();
        let from = local(2026, 11, 4, 8, 30);
        assert_eq!(
            every_minute.next_after(from),
            Some(local(2026, 11, 4, 8, 31))
        );
        // 四年一遇的 2 月 29 日。
        let leap = CronSchedule::parse("0 0 29 2 *").unwrap();
        assert_eq!(leap.next_after(from), Some(local(2028, 2, 29, 0, 0)));
    }

    #[test]
    fn at_fires_every_day_and_rejects_bad_times() {
        let at = TimerSpec::parse(None, None, Some("09:00")).unwrap();
        let first = at.next_after(local(2026, 11, 4, 10, 0)).unwrap();
        assert_eq!(first, local(2026, 11, 5, 9, 0));
        assert_eq!(at.next_after(first), Some(local(2026, 11, 6, 9, 0)));
        for bad in ["25:00", "09:60", "9", "aa:bb"] {
            assert!(TimerSpec::parse(None, None, Some(bad)).is_err(), "{bad}");
        }
        assert!(TimerSpec::parse(None, Some("30s"), None).is_err());
        assert!(TimerSpec::parse(Some("* * * * *"), Some("1m"), None).is_err());
        assert!(TimerSpec::parse(None, None, None).is_err());
    }

    #[test]
    fn catch_up_counts_missed_occurrences() {
        let every_minute = TimerSpec::Every(Duration::from_secs(60));
        let last = local(2026, 11, 4, 0, 0);
        let now = last + chrono::Duration::minutes(5);
        assert_eq!(missed_occurrences(&every_minute, last, now), 5);
        let cron = TimerSpec::parse(Some("* * * * *"), None, None).unwrap();
        assert_eq!(missed_occurrences(&cron, last, now), 5);
        // 时间倒过来不计。
        assert_eq!(missed_occurrences(&every_minute, now, last), 0);
        // 每天固定时刻：跨两天补两次。
        let at = TimerSpec::parse(None, None, Some("09:00")).unwrap();
        assert_eq!(
            missed_occurrences(&at, local(2026, 11, 2, 10, 0), local(2026, 11, 4, 10, 30)),
            2
        );
        // 每周一次、停机八天：以前只扫 1440 分钟，会漏掉。
        let weekly = TimerSpec::parse(Some("0 9 * * 1"), None, None).unwrap();
        assert_eq!(
            missed_occurrences(
                &weekly,
                local(2026, 11, 1, 10, 0),
                local(2026, 11, 9, 10, 0)
            ),
            2
        );
        // 长时间停机有上限。
        assert_eq!(
            missed_occurrences(&cron, last, last + chrono::Duration::days(30)),
            MAX_MISSED
        );
    }
}
