use std::io::Write;

use crate::Res;
use crate::model::Issue;

pub fn timestamp(ms: u64) -> String {
    let secs = ms / 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let doe = shifted.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

pub fn issue_line(w: &mut dyn Write, issue: &Issue) -> Res<()> {
    let assignee = issue.assignee.as_deref().unwrap_or("-");
    let labels = if issue.labels.is_empty() {
        String::new()
    } else {
        format!("  [{}]", issue.labels.join(","))
    };
    writeln!(
        w,
        "#{:<7} {:<11} {:<8} {:<10} {}{}",
        issue.id, issue.status, issue.priority, assignee, issue.title, labels
    )?;
    Ok(())
}

pub fn issue_detail(w: &mut dyn Write, issue: &Issue) -> Res<()> {
    writeln!(w, "#{} {}", issue.id, issue.title)?;
    writeln!(w, "status:   {}", issue.status)?;
    writeln!(w, "priority: {}", issue.priority)?;
    writeln!(
        w,
        "assignee: {}",
        issue.assignee.as_deref().unwrap_or("unassigned")
    )?;
    writeln!(
        w,
        "labels:   {}",
        if issue.labels.is_empty() {
            "none".to_string()
        } else {
            issue.labels.join(", ")
        }
    )?;
    writeln!(w, "created:  {}", timestamp(issue.created_ms))?;
    writeln!(w, "updated:  {}", timestamp(issue.updated_ms))?;
    if !issue.body.is_empty() {
        writeln!(w)?;
        writeln!(w, "{}", issue.body)?;
    }
    writeln!(w)?;
    writeln!(w, "comments ({}):", issue.comments.len())?;
    for comment in issue.comments.iter() {
        writeln!(
            w,
            "  {} {}: {}",
            timestamp(comment.at_ms),
            comment.author,
            comment.text
        )?;
    }
    Ok(())
}

fn shorten(text: &str) -> String {
    let flat = text.replace('\n', " ");
    if flat.chars().count() > 60 {
        let cut: String = flat.chars().take(57).collect();
        format!("{cut}...")
    } else {
        flat
    }
}

pub fn describe(old: &Issue, new: &Issue) -> Vec<String> {
    let mut out = Vec::new();
    if old.title != new.title {
        out.push(format!("title: {:?} -> {:?}", old.title, new.title));
    }
    if old.body != new.body {
        out.push("body edited".to_string());
    }
    if old.status != new.status {
        out.push(format!("status: {} -> {}", old.status, new.status));
    }
    if old.priority != new.priority {
        out.push(format!("priority: {} -> {}", old.priority, new.priority));
    }
    if old.assignee != new.assignee {
        out.push(format!(
            "assignee: {} -> {}",
            old.assignee.as_deref().unwrap_or("-"),
            new.assignee.as_deref().unwrap_or("-")
        ));
    }
    for label in &new.labels {
        if !old.labels.contains(label) {
            out.push(format!("+label {label}"));
        }
    }
    for label in &old.labels {
        if !new.labels.contains(label) {
            out.push(format!("-label {label}"));
        }
    }
    if new.comments.len() >= old.comments.len() {
        for comment in new.comments.iter().skip(old.comments.len()) {
            out.push(format!(
                "comment by {}: {}",
                comment.author,
                shorten(&comment.text)
            ));
        }
    } else {
        out.push(format!(
            "comments: {} -> {}",
            old.comments.len(),
            new.comments.len()
        ));
    }
    out
}
