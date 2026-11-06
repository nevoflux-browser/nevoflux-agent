//! The two model calls a meeting makes: tidying each sentence as it lands,
//! and writing the minutes at the end.
//!
//! Each can be set to its own provider and model in `[meeting]`; unset, it
//! follows the agent's own. Resolution is the goal evaluator's, which already
//! knows how to make a one-shot call on a direct API or a connected ACP agent.

use std::time::Duration;

use crate::config::AgentConfig;
use crate::goals::evaluator::{resolve_evaluator_for_goal, EvaluatorChoice};
use crate::wasm::llm::{execute_llm_chat, LlmChatRequest, LlmMessage};

/// A sentence not back in this long is shown as it was said.
pub const CLEAN_TIMEOUT: Duration = Duration::from_secs(5);
/// Sentences of context the tidier sees before the one it tidies.
pub const CLEAN_CONTEXT: usize = 3;

const CLEAN_SYSTEM: &str = "你是会议速记的润色器。把用户给出的“本句”改写成通顺的书面语：\
去掉口癖（嗯、啊、那个、就是说……）、结巴和重复，理顺语法和标点。\
不改变原意，不补充原文没有的信息，不回答其中的问题，不加任何解释。\
“前文”只用来理解指代，不要改写或复述前文。\
只输出改写后的本句；如果本句只有口癖没有内容，输出空行。";

pub fn clean_prompt(context: &[String], sentence: &str) -> String {
    let mut p = String::new();
    if !context.is_empty() {
        p.push_str("前文：\n");
        for c in context {
            p.push_str(c);
            p.push('\n');
        }
        p.push('\n');
    }
    p.push_str("本句：\n");
    p.push_str(sentence);
    p
}

/// What the tidier said, as the clean text: trimmed, with quotes or a
/// "本句：" echo some models add taken off.
pub fn clean_reply(reply: &str) -> String {
    let t = reply.trim();
    let t = t.strip_prefix("本句：").or_else(|| t.strip_prefix("本句:")).unwrap_or(t).trim();
    let t = t
        .strip_prefix('“')
        .and_then(|s| s.strip_suffix('”'))
        .or_else(|| t.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
        .unwrap_or(t);
    t.trim().to_string()
}

/// `provider`/`model` empty or absent: the agent's own.
pub fn resolve(config: &AgentConfig, provider: Option<&str>, model: Option<&str>) -> Result<EvaluatorChoice, String> {
    let provider = provider.filter(|s| !s.is_empty());
    let model = model.filter(|s| !s.is_empty());
    resolve_evaluator_for_goal(config, provider, model)
}

/// True if this choice sends the text off the machine.
pub fn is_cloud(choice: &EvaluatorChoice) -> bool {
    let local = choice
        .base_url
        .as_deref()
        .is_some_and(|u| u.contains("127.0.0.1") || u.contains("localhost") || u.contains("[::1]"));
    !(local || choice.provider == "ollama" || choice.provider.starts_with("local"))
}

pub async fn once(choice: &EvaluatorChoice, system: &str, user: String, max_tokens: u32) -> Result<String, String> {
    if choice.is_acp {
        let prompt = format!("{system}\n\n{user}");
        return crate::wasm::llm::acp_oneshot(choice.wire, &choice.model, &prompt)
            .await
            .map_err(|e| e.to_string());
    }
    let request = LlmChatRequest {
        messages: vec![LlmMessage::user(user)],
        system: Some(system.to_string()),
        temperature: Some(0.2),
        max_tokens: Some(max_tokens),
        tools: None,
    };
    execute_llm_chat(choice.wire, &choice.api_key, &choice.model, request, choice.base_url.as_deref())
        .await
        .map(|r| r.content)
        .map_err(|e| e.to_string())
}

/// One sentence tidied, or why not.
pub async fn clean(choice: &EvaluatorChoice, context: &[String], sentence: &str) -> Result<String, String> {
    let max = (sentence.chars().count() as u32 * 3).clamp(64, 1024);
    match tokio::time::timeout(CLEAN_TIMEOUT, once(choice, CLEAN_SYSTEM, clean_prompt(context, sentence), max)).await {
        Ok(r) => r.map(|t| clean_reply(&t)),
        Err(_) => Err("timed out".into()),
    }
}

const ASK_SYSTEM: &str = "你在会议中途回答佩戴者的问题。只根据给出的会议记录回答；记录里没有的就说没听到。\
回答会被朗读出来：用口语，一到三句，不要列表、不要 markdown。";

/// A question asked mid-meeting, answered from the record so far.
pub async fn ask(config: &AgentConfig, transcript: &str, question: &str) -> Result<String, String> {
    let choice = resolve(config, None, None)?;
    let user = format!("会议记录：\n{transcript}\n\n问题：{question}");
    once(&choice, ASK_SYSTEM, user, 400).await.map(|t| t.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_carries_context_only_when_there_is_some() {
        assert_eq!(clean_prompt(&[], "嗯那个"), "本句：\n嗯那个");
        let p = clean_prompt(&["预算要再确认。".into()], "就是说它它下周");
        assert!(p.starts_with("前文：\n预算要再确认。\n\n本句：\n就是说它它下周"));
    }

    #[test]
    fn reply_echoes_and_quotes_come_off() {
        assert_eq!(clean_reply("  本句：“下周确认预算。” \n"), "下周确认预算。");
        assert_eq!(clean_reply("\"Next week.\""), "Next week.");
        assert_eq!(clean_reply("\n"), "");
    }
}
