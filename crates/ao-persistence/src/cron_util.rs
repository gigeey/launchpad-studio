use chrono::Utc;
use chrono_tz::Tz;
use croner::Cron;
use std::str::FromStr;
use tracing::{debug, warn};

/// Resolve the effective timezone: user preference > system detection > UTC fallback.
fn resolve_timezone(timezone: Option<&str>) -> Option<Tz> {
    // Try user-provided timezone first
    if let Some(tz_str) = timezone {
        if let Ok(tz) = tz_str.parse::<Tz>() {
            return Some(tz);
        }
        warn!("Invalid timezone preference '{}', trying system detection", tz_str);
    }

    // Fall back to system timezone detection
    match iana_time_zone::get_timezone() {
        Ok(sys_tz) => match sys_tz.parse::<Tz>() {
            Ok(tz) => {
                debug!("Using system timezone: {}", sys_tz);
                Some(tz)
            }
            Err(_) => {
                warn!("System timezone '{}' is not a valid IANA timezone", sys_tz);
                None
            }
        },
        Err(e) => {
            warn!("Failed to detect system timezone: {}", e);
            None
        }
    }
}

/// Compute the next fire time from a cron expression, relative to now.
/// When `timezone` is provided (e.g. "America/Los_Angeles"), the cron expression
/// is evaluated in that timezone so that "0 18 * * *" means 6 PM local time.
/// Falls back to system timezone detection if no preference is set.
/// The returned DateTime is always in UTC for storage.
///
/// Shared by [`crate::assignment_store`] and the assignment-facing routes/tools
/// (assignments are the only remaining consumer of cron scheduling; the
/// scheduled-task feature that originally introduced this helper has been
/// removed).
pub fn compute_next_fire_at(
    cron_expr: Option<&str>,
    timezone: Option<&str>,
) -> Option<chrono::DateTime<Utc>> {
    let expr = cron_expr?;
    let cron = match Cron::from_str(expr) {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to parse cron expression '{}': {}", expr, e);
            return None;
        }
    };

    // Evaluate cron in user's timezone (or system timezone as fallback)
    if let Some(tz) = resolve_timezone(timezone) {
        let now_local = Utc::now().with_timezone(&tz);
        return match cron.find_next_occurrence(&now_local, false) {
            Ok(next_local) => Some(next_local.with_timezone(&Utc)),
            Err(e) => {
                warn!("Failed to compute next fire time for cron '{}': {}", expr, e);
                None
            }
        };
    }

    // Ultimate fallback: UTC
    match cron.find_next_occurrence(&Utc::now(), false) {
        Ok(next) => Some(next),
        Err(e) => {
            warn!("Failed to compute next fire time for cron '{}': {}", expr, e);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_next_fire_at_none_without_cron() {
        assert!(compute_next_fire_at(None, None).is_none());
    }

    #[test]
    fn compute_next_fire_at_none_for_invalid_cron() {
        assert!(compute_next_fire_at(Some("not a cron"), None).is_none());
    }

    #[test]
    fn compute_next_fire_at_some_for_valid_cron() {
        assert!(compute_next_fire_at(Some("0 9 * * *"), None).is_some());
    }
}
