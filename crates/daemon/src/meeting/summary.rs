//! The minutes: written when a meeting ends from its tidied sentences, kept
//! as `summary.json`, and put in the knowledge base under a slug fixed by the
//! meeting id — so a later, better pass overwrites the same page and deleting
//! the meeting knows which page to delete.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::store::{Entry, Kind, Meta};
use super::{llm, Meetings, TOPIC_SUMMARY};

/// The transcript handed to the model stops here; a meeting past it is
/// summarised from its start and its end.
const MAX_TRANSCRIPT_CHARS: usize = 60_000;

const SUMMARY_SYSTEM: &str = "你是会议纪要助手。根据会议记录写结构化纪要，只输出一个 JSON 对象，不要任何其他文字：\
{\"title\":\"一句话概括这场会\",\"topics\":[\"议题\"],\"decisions\":[\"结论\"],\
\"todos\":[{\"owner\":\"负责人，不知道写空字符串\",\"task\":\"事项\",\"due\":\"截止时间，没提到写空字符串\"}],\
\"open_questions\":[\"未决问题\"]}。\
记录里“我”是佩戴录音设备的人，“其他人”是会上的其他人；[批注] 是“我”会中做的标记，要优先体现在纪要里。\
只写记录里有的内容，不要编造。";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Todo {
    pub owner: String,
    pub task: String,
    pub due: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Minutes {
    pub title: String,
    pub topics: Vec<String>,
    pub decisions: Vec<String>,
    pub todos: Vec<Todo>,
    pub open_questions: Vec<String>,
}

/// The transcript as the model reads it.
pub fn render_transcript(entries: &[Entry]) -> String {
    let mut lines: Vec<String> = entries
        .iter()
        .filter_map(|e| {
            let who = if e.who == "me" { "我" } else if e.who == "other" { "其他人" } else { e.who.as_str() };
            let t = mmss(e.offset_ms);
            match e.kind {
                Kind::Speech => {
                    let text = e.clean.as_deref().filter(|c| !c.is_empty()).unwrap_or(&e.raw);
                    Some(format!("[{t}] {who}：{text}"))
                }
                Kind::Note => Some(format!("[{t}] [批注] {}", e.raw)),
                Kind::Qa => None,
            }
        })
        .collect();
    let total: usize = lines.iter().map(|l| l.chars().count() + 1).sum();
    if total > MAX_TRANSCRIPT_CHARS {
        // Keep the opening and the close; the middle is where a long meeting
        // repeats itself.
        let mut head = Vec::new();
        let mut used = 0;
        for l in &lines {
            used += l.chars().count() + 1;
            if used > MAX_TRANSCRIPT_CHARS / 2 {
                break;
            }
            head.push(l.clone());
        }
        let mut tail = Vec::new();
        let mut used = 0;
        for l in lines.iter().rev() {
            used += l.chars().count() + 1;
            if used > MAX_TRANSCRIPT_CHARS / 2 {
                break;
            }
            tail.push(l.clone());
        }
        tail.reverse();
        head.push("……（中间部分省略）……".into());
        head.extend(tail);
        lines = head;
    }
    lines.join("\n")
}

fn mmss(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

/// The JSON object in a reply, fenced or wrapped in prose or not.
pub fn parse_minutes(reply: &str) -> Option<Minutes> {
    let start = reply.find('{')?;
    let end = reply.rfind('}')?;
    serde_json::from_str(reply.get(start..=end)?).ok()
}

pub fn render_markdown(meta: &Meta, m: &Minutes) -> String {
    let mut md = String::new();
    let title = if m.title.is_empty() { "会议纪要" } else { m.title.as_str() };
    md.push_str(&format!(
        "---\ntitle: \"{}\"\ntype: meeting\nmeeting_id: \"{}\"\nstarted_at: \"{}\"\n---\n\n# {}\n\n",
        title.replace('"', "'"),
        meta.id,
        meta.started_at,
        title
    ));
    let section = |md: &mut String, name: &str, items: &[String]| {
        if !items.is_empty() {
            md.push_str(&format!("## {name}\n\n"));
            for i in items {
                md.push_str(&format!("- {i}\n"));
            }
            md.push('\n');
        }
    };
    section(&mut md, "议题", &m.topics);
    section(&mut md, "结论", &m.decisions);
    if !m.todos.is_empty() {
        md.push_str("## 待办\n\n");
        for t in &m.todos {
            let owner = if t.owner.is_empty() { "未指定".to_string() } else { t.owner.clone() };
            let due = if t.due.is_empty() { String::new() } else { format!("（{}）", t.due) };
            md.push_str(&format!("- [ ] {owner}：{}{due}\n", t.task));
        }
        md.push('\n');
    }
    section(&mut md, "未决问题", &m.open_questions);
    md
}

/// The knowledge-base page for a meeting.
pub fn brain_slug(meta: &Meta) -> String {
    let day = meta.started_at.get(..10).unwrap_or("undated");
    format!("meetings/{day}-{}", meta.id)
}

/// Write the quick minutes for a meeting that just ended.
pub async fn run(meetings: &Arc<Meetings>, meeting_id: &str) {
    let store = meetings.store();
    let Some(meta) = store.meta(meeting_id) else { return };
    let entries = store.transcript(meeting_id);
    if !entries.iter().any(|e| e.kind != Kind::Qa) {
        tracing::info!(target: "meeting", meeting_id, "nothing was said; no minutes");
        return;
    }
    let settings = meetings.settings();
    let config = crate::config::AgentConfig::load().unwrap_or_default();
    let choice = match llm::resolve(&config, settings.summary_provider.as_deref(), settings.summary_model.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(target: "meeting", meeting_id, error = %e, "no model for the minutes");
            meetings.publish(TOPIC_SUMMARY, json!({ "meeting_id": meeting_id, "error": e }), false).await;
            return;
        }
    };
    let transcript = render_transcript(&entries);
    let mut minutes = None;
    let mut last_err = String::new();
    for attempt in 0..2 {
        let system = if attempt == 0 { SUMMARY_SYSTEM.to_string() } else { format!("{SUMMARY_SYSTEM}\n只输出严格的 JSON。") };
        match llm::once(&choice, &system, transcript.clone(), 4096).await {
            Ok(reply) => {
                if let Some(m) = parse_minutes(&reply) {
                    minutes = Some(m);
                    break;
                }
                last_err = "the reply was not the JSON asked for".into();
            }
            Err(e) => last_err = e,
        }
    }
    let Some(minutes) = minutes else {
        tracing::warn!(target: "meeting", meeting_id, error = %last_err, "minutes failed");
        meetings.publish(TOPIC_SUMMARY, json!({ "meeting_id": meeting_id, "error": last_err })
            , false).await;
        return;
    };

    let markdown = render_markdown(&meta, &minutes);
    let slug = brain_slug(&meta);
    let brain_ref = if put_in_brain(&slug, &markdown).await { Some(slug) } else { None };
    let summary = json!({
        "pass": "quick",
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "model": format!("{}/{}", choice.provider, choice.model),
        "minutes": minutes,
        "markdown": markdown,
        "brain_ref": brain_ref,
    });
    if let Err(e) = store.put_summary(meeting_id, &summary) {
        tracing::warn!(target: "meeting", meeting_id, error = %e, "cannot keep the minutes");
    }
    tracing::info!(target: "meeting", meeting_id, todos = minutes.todos.len(), "minutes written");
    meetings.publish(TOPIC_SUMMARY, json!({ "meeting_id": meeting_id, "summary": summary }), false).await;
    meetings.tell_device(meeting_id, json!({
        "kind": "meeting_summary_ready",
        "meeting_id": meeting_id,
        "todos": minutes.todos.len(),
        "pass": "quick",
    })).await;
}

async fn put_in_brain(slug: &str, markdown: &str) -> bool {
    let Some(sup) = crate::brain_rpc::current_supervisor().await else {
        tracing::info!(target: "meeting", "no knowledge base; minutes kept with the meeting only");
        return false;
    };
    let args = json!({ "slug": slug, "content": markdown, "source_kind": "meeting" });
    match crate::brain_tools::invoke_brain_tool(&sup, "put_page", args).await {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!(target: "meeting", slug, error = %e, "cannot put the minutes in the knowledge base");
            false
        }
    }
}

pub async fn forget_in_brain(slug: &str) {
    let Some(sup) = crate::brain_rpc::current_supervisor().await else { return };
    if let Err(e) = crate::brain_tools::invoke_brain_tool(&sup, "delete_page", json!({ "slug": slug })).await {
        tracing::warn!(target: "meeting", slug, error = %e, "cannot delete the minutes from the knowledge base");
    }
}

/// Read back for the page.
pub fn minutes_of(summary: &Value) -> Option<Minutes> {
    serde_json::from_value(summary.get("minutes")?.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(seg: u32, offset_ms: u64, kind: Kind, who: &str, raw: &str, clean: Option<&str>) -> Entry {
        Entry {
            seg,
            offset_ms,
            dur_ms: 0,
            kind,
            who: who.into(),
            raw: raw.into(),
            clean: clean.map(String::from),
            clean_failed: false,
            late: false,
            who_fixed: false,
        }
    }

    #[test]
    fn transcript_prefers_clean_text_and_marks_notes() {
        let t = render_transcript(&[
            e(0, 65_000, Kind::Speech, "me", "嗯那个预算", Some("预算要再确认。")),
            e(1, 70_000, Kind::Speech, "other", "好的", None),
            e(2, 71_000, Kind::Note, "me", "标记一下，预算", None),
            e(3, 72_000, Kind::Qa, "me", "几号？", Some("周五。")),
        ]);
        assert_eq!(t, "[01:05] 我：预算要再确认。\n[01:10] 其他人：好的\n[01:11] [批注] 标记一下，预算");
    }

    #[test]
    fn long_transcripts_keep_both_ends() {
        let entries: Vec<Entry> = (0..3000).map(|i| e(i, u64::from(i) * 1000, Kind::Speech, "other", "这是一句足够长的会议发言内容。", None)).collect();
        let t = render_transcript(&entries);
        assert!(t.chars().count() <= MAX_TRANSCRIPT_CHARS + 20);
        assert!(t.starts_with("[00:00]"));
        assert!(t.contains("中间部分省略"));
        assert!(t.ends_with("这是一句足够长的会议发言内容。"));
        assert!(t.contains("[49:59]"));
    }

    #[test]
    fn minutes_parse_from_fenced_replies() {
        let m = parse_minutes("```json\n{\"title\":\"预算会\",\"todos\":[{\"owner\":\"张三\",\"task\":\"确认预算\",\"due\":\"周五\"}]}\n```").unwrap();
        assert_eq!(m.title, "预算会");
        assert_eq!(m.todos[0].owner, "张三");
        assert!(m.topics.is_empty());
        assert!(parse_minutes("no json").is_none());
    }

    #[test]
    fn markdown_and_slug() {
        let meta = Meta { id: "42".into(), started_at: "2026-11-06T06:30:05Z".into(), status: "ended".into(), ..Default::default() };
        let m = Minutes {
            title: "预算会".into(),
            todos: vec![Todo { owner: String::new(), task: "确认预算".into(), due: "周五".into() }],
            ..Default::default()
        };
        let md = render_markdown(&meta, &m);
        assert!(md.starts_with("---\ntitle: \"预算会\"\ntype: meeting\nmeeting_id: \"42\""));
        assert!(md.contains("- [ ] 未指定：确认预算（周五）"));
        assert!(!md.contains("## 议题"));
        assert_eq!(brain_slug(&meta), "meetings/2026-11-06-42");
    }
}
